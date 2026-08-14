//! 换 embedding 模型时不静默失忆 —— 存储层的那一半。
//!
//! # 这里守的是什么
//!
//! 四条向量查询全部按 `embedding_model` 过滤，所以「跨模型算距离得到垃圾」
//! 不会发生。代价是换模型的当天**向量那一路返回 0 行** —— 而 BM25 /
//! 图遍历 / recency 三路照常工作，于是它看起来不像坏了，
//! 只像「语义召回好像变差了点」。没有任何人被告知。
//!
//! 下面第一条测试**先复现这个失效**，再验证旁表把它补回来。
//! 只验后者的测试是没有意义的：它证明不了那条路原本是断的。

mod common;

use cortex_store::{EMBEDDING_DIM, NewFactEmbedding, Vector};

/// 造一条方向确定的单位向量：第 `i` 维为 1。
///
/// 不用随机向量：随机的话「第 k 近」是什么由运气决定，断言只能写成
/// 「返回了若干条」—— 那种断言对排序错误是瞎的。
fn unit(i: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; EMBEDDING_DIM];
    v[i % EMBEDDING_DIM] = 1.0;
    v
}

/// 写 n 条带向量的事实，全部记在 `model` 名下。返回它们的 id。
async fn seed(ctx: &common::TestDb, model: &str, n: usize) -> Vec<String> {
    let ep = common::new_episode("s-embed", "换模型测试");
    let ent = common::new_entity("project", "cortex");
    let facts: Vec<_> = (0..n)
        .map(|i| {
            let mut f = common::new_fact(ent.id, "uses", &format!("thing-{i}"), ep.id);
            f.embedding = Some(Vector::from(unit(i)));
            f.embedding_model = model.to_owned();
            f
        })
        .collect();
    let ids: Vec<String> = facts.iter().map(|f| f.id.to_string()).collect();

    ctx.store
        .write_txn(async |tx| {
            tx.insert_episode(&ep).await?;
            tx.insert_entity(&ent).await?;
            for f in &facts {
                tx.insert_fact(f).await?;
            }
            Ok(())
        })
        .await
        .expect("写入种子数据");
    ids
}

#[tokio::test]
async fn switching_model_silently_drops_vector_recall_until_backfilled() {
    let Some(ctx) = common::setup().await else {
        eprintln!("跳过：数据库不可用");
        return;
    };
    let store = &ctx.store;

    let ids = seed(&ctx, "old-model", 5).await;
    let probe = Vector::from(unit(0));

    // ── ① 旧模型下一切正常 ──────────────────────────────────
    let hit = store
        .recall_vector(&probe, "old-model", 10)
        .await
        .expect("向量召回");
    assert_eq!(hit.len(), 5, "旧模型下应当召回全部 5 条");
    assert_eq!(
        hit[0].id, ids[0],
        "第 0 维为 1 的探针应当最近的是第 0 条 —— 顺序错了说明外层没按距离排"
    );

    // ── ② 换模型：**这就是那个静默失效** ────────────────────
    let after = store
        .recall_vector(&probe, "new-model", 10)
        .await
        .expect("向量召回");
    assert!(
        after.is_empty(),
        "换模型后旧向量必须一条都不返回（跨空间的距离不可比）。\
         真正的问题在于这件事**没有任何报错** —— BM25 与图遍历照常兜着，\
         界面上只表现为「语义召回好像变差了点」。实际返回了 {} 条",
        after.len()
    );

    // ── ③ 回填：往旁表插新空间的向量 ────────────────────────
    store
        .write_txn(async |tx| {
            for (i, id) in ids.iter().enumerate() {
                tx.insert_fact_embedding(&NewFactEmbedding {
                    fact_id: id.parse().expect("fact id 是 ULID"),
                    embedding_model: "new-model".to_owned(),
                    // 刻意用**反过来**的方向：如果并集查询错拿了原列的向量，
                    // 排序会与这里的期望相反，测试就抓得到
                    embedding: Vector::from(unit(4 - i)),
                })
                .await?;
            }
            Ok(())
        })
        .await
        .expect("回填旁表向量");

    // ── ④ 补回来了 ─────────────────────────────────────────
    let healed = store
        .recall_vector(&probe, "new-model", 10)
        .await
        .expect("向量召回");
    assert_eq!(healed.len(), 5, "回填之后新模型下应当重新召回全部 5 条");
    assert_eq!(
        healed[0].id, ids[4],
        "旁表里第 4 条才是第 0 维为 1 的那个 —— 拿到 ids[0] 说明并集用的是\
         原列的向量而不是旁表的，也就是回填等于没做"
    );

    // ── ⑤ 旧模型不受影响 ───────────────────────────────────
    let old_still = store
        .recall_vector(&probe, "old-model", 10)
        .await
        .expect("向量召回");
    assert_eq!(
        old_still.len(),
        5,
        "回填不该动旧空间 —— 双模型召回（回填期间不丢记忆）全靠这一点"
    );

    ctx.cleanup().await;
}

#[tokio::test]
async fn a_fact_present_on_both_sides_is_returned_once() {
    let Some(ctx) = common::setup().await else {
        eprintln!("跳过：数据库不可用");
        return;
    };
    let store = &ctx.store;

    // 原列就是 same-model，再往旁表塞一条**同模型**的 —— 正常回填不会造出
    // 这种重叠（回填器只挑 embedding_model <> 当前 的事实），但「换走再换回来」
    // 能。重复的症状是同一条记忆在召回里出现两次、RRF 给它双份排名，
    // 是一个悄悄放大的偏置而不是报错
    let ids = seed(&ctx, "same-model", 3).await;
    store
        .write_txn(async |tx| {
            tx.insert_fact_embedding(&NewFactEmbedding {
                fact_id: ids[0].parse().expect("fact id"),
                embedding_model: "same-model".to_owned(),
                embedding: Vector::from(unit(7)),
            })
            .await
        })
        .await
        .expect("写旁表向量");

    let hit = store
        .recall_vector(&Vector::from(unit(0)), "same-model", 10)
        .await
        .expect("向量召回");
    assert_eq!(hit.len(), 3, "两侧都命中的事实必须只出现一次，实际 {hit:?}");

    let mut seen: Vec<&str> = hit.iter().map(|f| f.id.as_str()).collect();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), 3, "去重后条数应当不变 —— 变了说明有重复行");

    ctx.cleanup().await;
}

#[tokio::test]
async fn census_and_backlog_agree_with_each_other() {
    let Some(ctx) = common::setup().await else {
        eprintln!("跳过：数据库不可用");
        return;
    };
    let store = &ctx.store;

    let ids = seed(&ctx, "old-model", 4).await;

    // 普查：只有旧模型
    let census = store.embedding_census().await.expect("普查");
    assert_eq!(
        census,
        vec![("old-model".to_string(), 4)],
        "普查结果应当只有旧模型一档、覆盖 4 条"
    );

    // 待办：4 条都缺新模型的向量
    let todo = store
        .facts_missing_embedding("new-model", 100)
        .await
        .expect("取待办");
    assert_eq!(todo.len(), 4, "换模型时全部 4 条都该在待办里");

    // 回填两条
    store
        .write_txn(async |tx| {
            for id in ids.iter().take(2) {
                tx.insert_fact_embedding(&NewFactEmbedding {
                    fact_id: id.parse().expect("fact id"),
                    embedding_model: "new-model".to_owned(),
                    embedding: Vector::from(unit(1)),
                })
                .await?;
            }
            Ok(())
        })
        .await
        .expect("回填两条");

    // 普查：两档并存，这正是「回填进行中」应有的样子
    let census = store.embedding_census().await.expect("普查");
    assert_eq!(
        census,
        vec![("old-model".to_string(), 4), ("new-model".to_string(), 2)],
        "回填一半时两档都在，且按覆盖数降序"
    );

    // 待办少了两条 —— 这条断言同时守住「回填器不会把同一条反复补」
    let todo = store
        .facts_missing_embedding("new-model", 100)
        .await
        .expect("取待办");
    assert_eq!(
        todo.len(),
        2,
        "已补的两条必须从待办里消失，否则回填器会无限循环地重补同一批"
    );

    ctx.cleanup().await;
}

/// 执行计划守卫 —— 基准集**不能被物化**。
///
/// # 它验什么，不验什么
///
/// **验**：计划里不出现 `CTE Scan`。这是计划**形状**的性质，与数据量无关，
/// 所以在测试规模上也测得准。
///
/// **不验**：两侧有没有真的走 HNSW。那取决于行数与统计信息 ——
/// 测试库里只有几条，planner 会选 btree（`idx_facts_model`）而不是 HNSW，
/// 连 `enable_seqscan = off` 都逼不出来（它只关掉顺序扫描，btree 还在）。
/// 想验索引选择得造几千行 × 1024 维，慢且脆，而且验的是 planner 的成本模型
/// 不是我们的查询写法。
///
/// # 为什么这半条仍然值得写
///
/// 第一版把基准集抽成了 `WITH base AS (…)`。它被两个分支各引用一次，
/// PG 12 起只在**引用一次**时内联，于是它被物化 —— 分支一的
/// `ORDER BY embedding <=> $1` 只能对着一堆已落地的行排序，HNSW 彻底出局。
///
/// 那个写法在小库上跑得飞快，等库长大了才暴露，而那时它看起来像
/// 「数据库变慢了」而不是「索引没被用上」。这条测试拦的正是它。
///
/// # 顺带记下一个**本次改动之前就存在**的问题
///
/// 分支一其实**用不了** `idx_facts_vec`，原因与本次改动无关：
/// `active_facts` 是带 LEFT JOIN 的视图（join 到 `fact_status`，
/// 后者又是 `fact_events` 上的 DISTINCT ON），planner 必须先把 join 做完
/// 才能按距离排序。改动前那条查询的计划是：
///
/// ```text
///   Sort (Sort Key: f.embedding <=> …)
///     ->  Nested Loop Left Join
///           ->  Seq Scan on facts f
///           ->  Sort  ->  Seq Scan on fact_events
/// ```
///
/// 也就是说主召回路上那个 HNSW 一直是摆设。**已修**：
/// migrations/20260810000001 把视图换成两层反连接，5 万条事实上
/// 289.7ms → 0.5ms。下面那条「不许出现外连接」的断言守的就是这个。
#[tokio::test]
async fn the_base_set_is_not_materialized() {
    let Some(ctx) = common::setup().await else {
        eprintln!("跳过：数据库不可用");
        return;
    };

    let ids = seed(&ctx, "m", 3).await;
    ctx.store
        .write_txn(async |tx| {
            for id in &ids {
                tx.insert_fact_embedding(&NewFactEmbedding {
                    fact_id: id.parse().expect("fact id"),
                    embedding_model: "other".to_owned(),
                    embedding: Vector::from(unit(2)),
                })
                .await?;
            }
            Ok(())
        })
        .await
        .expect("旁表也要有行，否则分支二在计划里根本不出现");

    // `query_raw` 不支持绑定参数，所以把三个参数拼成字面量。
    // 1024 维的向量字面量有几 KB —— 在测试里可以接受，在生产代码里不行
    let probe = format!(
        "'[{}]'::vector",
        unit(0)
            .iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    let sql = cortex_store::RECALL_VECTOR_SQL
        .replace("$1", &probe)
        .replace("$2", "'m'")
        .replace("$3", "10");

    let text = ctx.query_raw(format!("EXPLAIN {sql}")).await.join(
        "
",
    );

    assert!(
        !text.contains("CTE Scan"),
        "计划里出现了 CTE Scan —— 基准集被物化了，向量索引彻底出局。         别把 base 抽成 `WITH`：它被两个分支各引用一次，PG 不会内联它。"
    );
    // 两个分支都要真的出现在计划里 —— 少一个说明并集写法把某一侧优化掉了，
    // 而那正是「回填了却召不回来」的样子
    assert!(
        text.contains("facts"),
        "计划里没有 facts —— 分支一不见了：
{text}"
    );
    assert!(
        text.contains("fact_embeddings"),
        "计划里没有 fact_embeddings —— 分支二不见了，回填等于白做：
{text}"
    );

    // **ORDER BY 前面不能有外连接。**
    //
    // 这条守的是 active_facts 的形状。它原来是 `facts LEFT JOIN fact_status`，
    // 而外连接必须**先做完**才能按距离排序 —— 于是 idx_facts_vec 从来没被
    // 用上过，主召回路上那个 HNSW 一直是摆设。反连接（NOT EXISTS）不一样：
    // planner 可以把它当成 facts 上的一个过滤条件，索引就够得着了。
    //
    // # 为什么不直接断言「计划里有 idx_facts_vec」
    //
    // 试过，测不出来。测试库里只有个位数行，那个规模下 planner 选顺序扫描
    // 或者小 btree 永远更便宜 —— 连 `enable_seqscan = off` 都逼不出 HNSW，
    // 它会改走 idx_facts_model 再排序。**4 行数据上「planner 选了什么」
    // 证明不了任何事**，所以这里断言的是结构，不是选择。
    //
    // 定量证据在 migrations/20260810000001 里：5 万条事实上
    // 289.7ms → 0.5ms，19.8 万次缓冲访问 → 531 次。
    assert!(
        !text.contains("Left Join"),
        "计划里出现了外连接 —— active_facts 又变回「必须先 join 完」的形状了。
这不会让结果变错，只会让每次向量召回都退化成全表扫描 + 几万次 1024 维距离计算，
而且没有任何别的测试会为此变红。见 migrations/20260810000001：
{text}"
    );

    ctx.cleanup().await;
}
