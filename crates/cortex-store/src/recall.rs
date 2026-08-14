//! 多路召回的查询侧。
//!
//! 每一路各自返回**已按该路自身相关性排好序**的候选，融合交给
//! `cortex-memory::fusion` 的 RRF 完成 —— 存储层不做排序策略，
//! 只负责把各路的原始结果捞出来。哪几路要、哪几路不要，
//! 由 `cortex-memory::retrieval` 按评测数据裁决，不在这里判断。
//!
//! 日常召回全部只查 `active_facts` 视图（已失效的事实不该进日常检索）。
//! 例外是 `facts_as_of*` 那一组：它们要的正是「在那个时刻我以为什么是真的」，
//! 必须回到 `facts` + `fact_events` 重放。三条回放召回共用一份快照判定
//! （`as_of_snapshot!`），差别只在**快照之上按什么排序**。

use chrono::{DateTime, Utc};
use sqlx::{FromRow as _, Row as _};

use crate::error::Result;
use crate::model::{Entity, Fact, fact_columns};
use crate::store::Store;

/// 一次召回返回的条数上限的合理区间。
/// 太少会让 RRF 没有融合空间，太多会拖慢图遍历那一路。
///
/// `pub(crate)`：换模型的回填待办查询（`query.rs`）也用同一个上限。
/// 复制一份常量的下场是两处慢慢漂开，而漂开的症状是「某条路径悄悄
/// 返回得比别处多/少」，没有任何报错。
pub(crate) const MAX_LIMIT: i64 = 200;

/// 「在 `$1` 那一刻 Cortex 认为仍然为真」的快照。
///
/// 三条回放召回（近因 / BM25 / 向量）共用同一份判定。**必须查 `facts` 而非
/// `active_facts`**：后者是**当前**的有效集，用它做回放会把「后来才被推翻的
/// 结论」提前抹掉，那正好是回放要看的东西。
macro_rules! as_of_snapshot {
    () => {
        concat!(
            "SELECT ",
            fact_columns!(),
            " FROM facts f
              WHERE f.created_at <= $1
                AND NOT EXISTS (
                    SELECT 1 FROM fact_events e
                     WHERE e.fact_id = f.id
                       AND e.op = 'invalidate'
                       AND e.created_at <= $1
                )"
        )
    };
}

/// 向量召回的**并集**：事实原列 ∪ `fact_embeddings` 旁表。
///
/// # 为什么是并集
///
/// 换模型之后，「模型 M 下的向量」分散在两处：写入时就用 M 的落在
/// `facts.embedding`，后来被回填器补出来的落在 `fact_embeddings`。
/// 只查一处的后果是**静默的部分失忆** —— 漏掉的那一半照常有 BM25 与
/// 图遍历兜着，所以它看起来不像坏了（见 migrations/20260808000001 的注释）。
///
/// # 为什么写成两个各自 `ORDER BY … LIMIT` 的分支
///
/// 为了让每一侧都**有机会**走自己的向量索引。写成 `JOIN` 之后再统一
/// `ORDER BY`，或者用 `LATERAL`，索引连机会都没有 —— 那种查询在小库上
/// 跑得飞快，等库长大了才暴露，而那时它看起来像「数据库变慢了」
/// 而不是「索引没被用上」。
///
/// 两侧现在都真的走索引了 —— 但分支一是**后来才修好的**，这段历史值得留着：
///
/// `active_facts` 原本是 `facts LEFT JOIN fact_status`，而外连接必须先做完
/// 才能按距离排序，于是 `idx_facts_vec` 从这个项目的第一天起就没被用上过。
/// 5 万条事实上实测 289.7ms、19.8 万次缓冲、4.3 万次 1024 维距离计算；
/// 换成反连接之后 0.5ms、531 次缓冲。视图定义见
/// migrations/20260810000001，那里有三种写法的完整对照。
///
/// **教训是它不报错**：结果始终完全正确，只是慢，而小库上根本看不出来。
/// 所以给 `active_facts` 改形状之前先看一眼那条 migration ——
/// 一个不小心加回来的 join 会把这里悄悄打回原形。
///
/// # 为什么 `$base` 被写了两遍，而不是抽成 CTE
///
/// 写成 `WITH base AS (…)` 更好看，但**它会让分支一的 HNSW 失效**。
/// PG 12 起只在 CTE 被引用**一次**时内联；这里两个分支各引用一次，
/// 于是它被物化成临时结果，`ORDER BY embedding <=> $1` 只能对着
/// 一堆已经落地的行做排序。
///
/// 这不是推测 —— 第一版就是 CTE，`EXPLAIN` 打出来是：
///
/// ```text
///   ->  Sort  (Sort Key: b.embedding <=> …)
///         ->  CTE Scan on base b          ← 分支一，没有索引
///   ->  Index Scan using idx_fact_embeddings_vec on fact_embeddings  ← 分支二有
/// ```
///
/// 去掉 CTE 之后两侧都是 `Index Scan`。重复那段 SQL 是它的代价。
///
/// # `DISTINCT ON` 是防御性的
///
/// 正常情况下两侧不会撞：回填器只挑 `embedding_model <> 当前` 的事实
/// （见 `Store::facts_missing_embedding`），所以一条事实不会同时在两边。
/// 但「换走再换回来」这种路径能造出重叠，而重叠的症状是同一条记忆
/// 在召回里出现两次、RRF 给它双份排名 —— 一个悄悄放大的偏置。
macro_rules! vector_union {
    (base = $base:expr, vec = $v:literal, model = $m:literal, limit = $l:literal) => {
        concat!(
            "SELECT ",
            fact_columns!("d."),
            ", d.distance
               FROM (
                 SELECT DISTINCT ON (u.id) ",
            fact_columns!("u."),
            ", u.distance
                   FROM (
                     (SELECT ",
            fact_columns!("b."),
            ", b.embedding <=> ",
            $v,
            " AS distance
                        FROM ",
            $base,
            " b
                       WHERE b.embedding IS NOT NULL AND b.embedding_model = ",
            $m,
            "        ORDER BY b.embedding <=> ",
            $v,
            "
                       LIMIT ",
            $l,
            ")
                     UNION ALL
                     (SELECT ",
            fact_columns!("b."),
            ", fe.distance
                        FROM (SELECT fact_id, embedding <=> ",
            $v,
            " AS distance
                                FROM fact_embeddings
                               WHERE embedding_model = ",
            $m,
            "                ORDER BY embedding <=> ",
            $v,
            "
                               LIMIT ",
            $l,
            ") fe
                        JOIN ",
            $base,
            " b ON b.id = fe.fact_id)
                   ) u
                  ORDER BY u.id, u.distance
               ) d
              ORDER BY d.distance
              LIMIT ",
            $l
        )
    };
}

/// 实时向量召回用的那条 SQL。
///
/// 提成常量只为一件事：让 `tests/embedding_migration.rs` 能对**真正跑的那条
/// 查询**做 `EXPLAIN`。测试里手抄一份 SQL 的下场是它慢慢漂开，
/// 而漂开之后那条测试守的是一条已经不存在的查询 —— 它照样绿。
#[doc(hidden)]
pub const RECALL_VECTOR_SQL: &str = vector_union!(
    base = "active_facts",
    vec = "$1",
    model = "$2",
    limit = "$3"
);

impl Store {
    /// ① BM25 全文召回 —— 专有名词与精确匹配。
    ///
    /// `tsquery_input` 由 `cortex-memory::tokenize::to_tsquery_input` 生成
    /// （jieba 分词后 OR 连接）。这一路是中文检索的主力：向量对
    /// 「张伟」这类专有名词表现很差，全文才能精确命中。
    pub async fn recall_bm25(&self, tsquery_input: &str, limit: i64) -> Result<Vec<Fact>> {
        if tsquery_input.trim().is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, Fact>(concat!(
            "SELECT ",
            fact_columns!(),
            "  FROM active_facts
              WHERE tsv @@ to_tsquery('simple', $1)
              ORDER BY ts_rank(tsv, to_tsquery('simple', $1)) DESC, created_at DESC
              LIMIT $2"
        ))
        .bind(tsquery_input)
        .bind(limit.clamp(1, MAX_LIMIT))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// ② 向量余弦召回 —— 语义相近。
    ///
    /// 按 `embedding_model` 过滤：换模型期间新旧向量不在同一语义空间，
    /// 距离不可比。混着查会得到毫无意义的排序（见 memory.md §七）。
    pub async fn recall_vector(
        &self,
        embedding: &pgvector::Vector,
        embedding_model: &str,
        limit: i64,
    ) -> Result<Vec<Fact>> {
        // 与 recall_vector_scored 共用同一条 SQL：多回来的 distance 列
        // 被 `FromRow for Fact` 按名取列时直接忽略。
        // 两份手抄的 SQL 迟早会漂开，而漂开的症状是「带分数的那条路和不带
        // 分数的那条路召回结果不一样」—— 没有任何报错
        let rows = sqlx::query_as::<_, Fact>(RECALL_VECTOR_SQL)
            .bind(embedding)
            .bind(embedding_model)
            .bind(limit.clamp(1, MAX_LIMIT))
            .fetch_all(self.pool())
            .await?;
        Ok(rows)
    }

    /// ② 的带分数版本 —— 除事实外一并返回**余弦距离**（不是相似度）。
    ///
    /// # 为什么要把分数捞出来
    ///
    /// RRF 只用排名，这是它的优点（量纲免疫），但也意味着融合分**完全不含
    /// 绝对相关性信息**：一个库里根本没有答案的问题，四路照样各自返回自己
    /// 最好的那几条，它们照样互相印证，融合分照样很高。评测实测：
    /// 弃权阈值从 0 扫到 0.045，误召率纹丝不动地停在 0.370 ——
    /// 那道闸门管的是**注入条数**，不是**相不相关**。
    ///
    /// 唯一还带绝对量纲的信号就是这里的余弦距离：它由模型定标，
    /// 与语料规模无关。相似度 = 1 - 距离。
    ///
    /// 保留不带分数的 [`Store::recall_vector`] 是因为召回路本身不需要分数，
    /// 而多返回一列会让那条热路径每次多搬一遍 f64。
    pub async fn recall_vector_scored(
        &self,
        embedding: &pgvector::Vector,
        embedding_model: &str,
        limit: i64,
    ) -> Result<Vec<(Fact, f64)>> {
        let rows = sqlx::query(RECALL_VECTOR_SQL)
            .bind(embedding)
            .bind(embedding_model)
            .bind(limit.clamp(1, MAX_LIMIT))
            .fetch_all(self.pool())
            .await?;

        rows.into_iter()
            .map(|r| {
                let distance: f64 = r.try_get("distance")?;
                Ok((Fact::from_row(&r)?, distance))
            })
            .collect()
    }

    /// ③ 图遍历召回 —— 从命中实体出发扩展 1–2 跳的关联事实。
    ///
    /// `facts` 表里 `subject_id → object_entity_id` 的每一行就是图的一条边，
    /// 因此这里是普通的递归 CTE，不需要图数据库。
    ///
    /// 实体先经 `canonical_entities` 归并，否则别名会让同一实体的邻居分裂。
    /// 结果**按跳数升序** —— 近的比远的相关。
    ///
    /// # 为什么要嵌一层子查询
    ///
    /// `DISTINCT ON (f.id)` 强制 `ORDER BY` 以 `f.id` 打头，于是外层排序
    /// 只能是 `ORDER BY f.id, hop` —— 那是**按 fact id 排序**，而 id 是
    /// ULID（创建顺序），跟相关性毫无关系。更糟的是 `LIMIT` 也会按 id 截断，
    /// 语料一大，近跳的事实会被远跳的老事实挤掉。
    ///
    /// 所以先在内层用 `DISTINCT ON` 完成「每条事实只保留最小跳数」的去重，
    /// 再在外层按跳数排序与截断。评测发现了这个 bug：某题的 gold 是全库
    /// 最新的事实，却在 7 条图召回里排到第 6。
    pub async fn recall_graph(
        &self,
        seed_entity_ids: &[String],
        max_hops: i32,
        limit: i64,
    ) -> Result<Vec<Fact>> {
        if seed_entity_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, Fact>(concat!(
            "WITH RECURSIVE seeds AS (
                 SELECT DISTINCT COALESCE(c.canonical, s.id) AS id
                   FROM unnest($1::text[]) AS s(id)
                   LEFT JOIN canonical_entities c ON c.id = s.id
             ),
             reach(entity_id, hop) AS (
                 SELECT id, 0 FROM seeds
               UNION
                 SELECT COALESCE(f.object_entity_id, f.subject_id), r.hop + 1
                   FROM reach r
                   JOIN active_facts f
                     ON f.subject_id = r.entity_id
                  WHERE r.hop < $2
                    AND f.object_entity_id IS NOT NULL
             ),
             nearest AS (
                 SELECT DISTINCT ON (f.id) ",
            fact_columns!("f."),
            ", r.hop
                   FROM active_facts f
                   JOIN reach r
                     ON f.subject_id = r.entity_id OR f.object_entity_id = r.entity_id
                  ORDER BY f.id, r.hop
             )
             SELECT ",
            fact_columns!(),
            "  FROM nearest
              ORDER BY hop, created_at DESC
              LIMIT $3"
        ))
        .bind(seed_entity_ids)
        .bind(max_hops.clamp(1, 3))
        .bind(limit.clamp(1, MAX_LIMIT))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// ④ 时间近因召回 —— 上下文连续性。
    ///
    /// 按**系统时间**（`created_at`，即 Cortex 何时知道）而非事件时间排序：
    /// 「最近聊过什么」问的是认知的新鲜度，不是世界的时间线。
    pub async fn recall_recent(&self, within_days: i64, limit: i64) -> Result<Vec<Fact>> {
        let rows = sqlx::query_as::<_, Fact>(concat!(
            "SELECT ",
            fact_columns!(),
            "  FROM active_facts
              WHERE created_at > now() - make_interval(days => $1::int)
              ORDER BY created_at DESC
              LIMIT $2"
        ))
        .bind(within_days.clamp(1, 3650) as i32)
        .bind(limit.clamp(1, MAX_LIMIT))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// ⑤ episodes 全文召回 —— 从**原始对话**倒查它抽出来的事实。
    ///
    /// 前四路全部只看 `facts.statement`，也就是抽取器改写过的一句话。
    /// 「上次我们说的那个」「你是从哪儿知道的」这类**出处追问**里，
    /// 用户复述的是当时的原话，而原话的措辞往往一个字都没进 statement——
    /// 于是 BM25 那一路查无此词，向量那一路也对不上。
    /// `episodes.tsv` 的 GIN 索引建了却一直没人用，这一路就是补上它。
    ///
    /// 排序按 episode 的 `ts_rank`，同一 episode 内按事实新旧。
    /// 返回的仍然是 `Fact` —— 注入契约只认事实，L0 原文不进 prompt。
    pub async fn recall_via_episodes(
        &self,
        tsquery_input: &str,
        episode_limit: i64,
        limit: i64,
    ) -> Result<Vec<Fact>> {
        if tsquery_input.trim().is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, Fact>(concat!(
            "WITH hit AS (
                 SELECT e.id, ts_rank(e.tsv, to_tsquery('simple', $1)) AS ep_rank
                   FROM episodes e
                  WHERE e.tsv @@ to_tsquery('simple', $1)
                  ORDER BY ep_rank DESC, e.occurred_at DESC
                  LIMIT $2
             )
             SELECT ",
            fact_columns!("f."),
            "   FROM active_facts f
                JOIN hit ON hit.id = f.source_episode_id
               ORDER BY hit.ep_rank DESC, f.created_at DESC
               LIMIT $3"
        ))
        .bind(tsquery_input)
        .bind(episode_limit.clamp(1, MAX_LIMIT))
        .bind(limit.clamp(1, MAX_LIMIT))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// 按**系统时间**回放：在 `as_of` 那一刻，Cortex 认为哪些事实为真。
    ///
    /// 这是双时间轴的核心能力，回答「三个月前我以为什么」。
    /// 注意它不能查 `active_facts` 视图——那是**当前**的有效集；
    /// 必须回到 `facts` + `fact_events` 按时间重放。
    ///
    /// 只按 `created_at` 降序 —— 这是「当时的全貌」。带查询词的回放走
    /// [`Store::facts_as_of_bm25`] 与 [`Store::facts_as_of_vector`]，
    /// 三者在 `cortex-memory` 侧再 RRF 到一起。
    pub async fn facts_as_of(&self, as_of: DateTime<Utc>, limit: i64) -> Result<Vec<Fact>> {
        let rows = sqlx::query_as::<_, Fact>(concat!(
            as_of_snapshot!(),
            " ORDER BY f.created_at DESC LIMIT $2"
        ))
        .bind(as_of)
        .bind(limit.clamp(1, MAX_LIMIT))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// 回放快照上的 BM25 排序。
    ///
    /// # 为什么不是「先取全貌再在内存里排」
    ///
    /// 全貌在十万条量级根本取不回来。回放的相关性排序必须**下推到 SQL**，
    /// 让 `idx_facts_tsv` 那个 GIN 索引干活，否则这条路在语料变大后
    /// 只会从「排序不对」退化成「查不动」。
    pub async fn facts_as_of_bm25(
        &self,
        as_of: DateTime<Utc>,
        tsquery_input: &str,
        limit: i64,
    ) -> Result<Vec<Fact>> {
        if tsquery_input.trim().is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, Fact>(concat!(
            as_of_snapshot!(),
            "   AND f.tsv @@ to_tsquery('simple', $2)
             ORDER BY ts_rank(f.tsv, to_tsquery('simple', $2)) DESC, f.created_at DESC
             LIMIT $3"
        ))
        .bind(as_of)
        .bind(tsquery_input)
        .bind(limit.clamp(1, MAX_LIMIT))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// 回放快照上的向量排序。理由同 [`Store::facts_as_of_bm25`]。
    ///
    /// 同样按 `embedding_model` 过滤：跨模型的距离不可比，回放也不例外。
    pub async fn facts_as_of_vector(
        &self,
        as_of: DateTime<Utc>,
        embedding: &pgvector::Vector,
        embedding_model: &str,
        limit: i64,
    ) -> Result<Vec<Fact>> {
        // 回放的基准集是 as_of 快照（**必须查 facts 而非 active_facts**，
        // 理由见 as_of_snapshot! 的注释），并集那一层的逻辑与实时召回相同
        let rows = sqlx::query_as::<_, Fact>(vector_union!(
            base = concat!("(", as_of_snapshot!(), ")"),
            vec = "$2",
            model = "$3",
            limit = "$4"
        ))
        .bind(as_of)
        .bind(embedding)
        .bind(embedding_model)
        .bind(limit.clamp(1, MAX_LIMIT))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// 由文本反查候选实体，供图遍历取种子。
    ///
    /// 先精确匹配名称，再回落到前缀匹配——实体名通常很短，
    /// 全文检索在这里反而不如直接匹配准。
    pub async fn entities_matching(&self, terms: &[String], limit: i64) -> Result<Vec<String>> {
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT DISTINCT COALESCE(c.canonical, e.id) AS id
               FROM entities e
               LEFT JOIN canonical_entities c ON c.id = e.id
              WHERE e.name = ANY($1)
                 OR EXISTS (
                     SELECT 1 FROM unnest($1::text[]) AS t(term)
                      WHERE e.name ILIKE t.term || '%'
                 )
              LIMIT $2",
        )
        .bind(terms)
        .bind(limit.clamp(1, MAX_LIMIT))
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(|r| r.get::<String, _>("id")).collect())
    }
}

impl Store {
    /// 按向量近邻查实体，供抽取期的**别名消解**使用。
    ///
    /// 与 [`Store::recall_vector`] 的区别在用途：那是检索召回，这是写入前
    /// 判断「cortex」和「Cortex 项目」是不是同一个东西。
    ///
    /// 返回按余弦距离升序的候选及其**距离**（不是相似度）——
    /// 阈值判断留给调用方，因为「多近算同一个」高度依赖 embedding 模型，
    /// 存储层不该替它做决定。相似度 = 1 - 距离。
    ///
    /// 按 `kind` 过滤：人和项目即使同名也不该合并。
    /// 按 `embedding_model` 过滤：跨模型的距离不可比。
    pub async fn entities_near(
        &self,
        kind: &str,
        embedding: &pgvector::Vector,
        embedding_model: &str,
        limit: i64,
    ) -> Result<Vec<(Entity, f64)>> {
        // 与事实侧同样的并集：写入时就用当前模型的落在 entities.embedding，
        // 回填补出来的落在 entity_embeddings。只查一处会让消解在换模型后
        // 静默失灵 —— 而失灵的表现是给同一个人建出第二个实体，
        // 从此他的事实分裂在两个节点上，图遍历再也走不通
        let rows = sqlx::query(
            // 三层：内层两个分支各自走自己的 HNSW → 中层 DISTINCT ON 去重
            // （它强制按 id 排序）→ **外层必须重新按 distance 排**，
            // 否则调用方拿到的是按 ULID 排的结果，而 ULID 有序意味着
            // 「按创建时间排」—— 那看起来像一份合理的列表，只是和相关性无关
            "SELECT id, kind, name, summary, embedding, embedding_model,
                    device_id, created_at, distance
               FROM (
                 SELECT DISTINCT ON (u.id) u.*
                   FROM (
                     (SELECT id, kind, name, summary, embedding, embedding_model,
                             device_id, created_at, embedding <=> $2 AS distance
                        FROM entities
                       WHERE kind = $1
                         AND embedding IS NOT NULL
                         AND embedding_model = $3
                       ORDER BY embedding <=> $2
                       LIMIT $4)
                     UNION ALL
                     (SELECT e.id, e.kind, e.name, e.summary, e.embedding, e.embedding_model,
                             e.device_id, e.created_at, ee.distance
                        FROM (SELECT entity_id, embedding <=> $2 AS distance
                                FROM entity_embeddings
                               WHERE embedding_model = $3
                               ORDER BY embedding <=> $2
                               LIMIT $4) ee
                        JOIN entities e ON e.id = ee.entity_id
                       WHERE e.kind = $1)
                   ) u
                  ORDER BY u.id, u.distance
               ) d
              ORDER BY distance
              LIMIT $4",
        )
        .bind(kind)
        .bind(embedding)
        .bind(embedding_model)
        .bind(limit.clamp(1, MAX_LIMIT))
        .fetch_all(self.pool())
        .await?;

        rows.into_iter()
            .map(|r| {
                let distance: f64 = r.try_get("distance")?;
                let entity = Entity::from_row(&r)?;
                Ok((entity, distance))
            })
            .collect()
    }
}
