//! 换 embedding 模型之后，把旧事实的向量补出来。
//!
//! # 它在补什么
//!
//! 四条向量查询全部按 `embedding_model` 过滤（见
//! `cortex_store::Store::recall_vector`），所以换模型的当天，
//! **旧事实一条都进不了向量召回**。而 BM25 / 图遍历 / 近因三路照常工作 ——
//! 于是它看起来不像坏了，只像「语义召回好像变差了点」。
//! 没有这个回填器，那个状态是**永久**的：新事实一天天累积，
//! 旧记忆永远回不到语义空间里。
//!
//! # 为什么默认开着
//!
//! 关掉的代价是上面那段话原样成立，而它**没有任何症状**。
//! 开着的代价是可见的：一次全量回填 = 一次全库的 embedding 调用，
//! 走云服务是要花钱的。所以它开着，但**先响亮地把数字报出来再动手** ——
//! 启动日志里会写清「发现 N 条属于旧模型 X，将以每轮 M 条回填」。
//! 不想让它跑就设 `CORTEX_REEMBED=off`。
//!
//! # 与 `backfill`（转录回扫）的关系
//!
//! 骨架是同一套（延迟首轮 / 定期扫 / 每轮有上限），但两者互不知道对方。
//! 分开跑是刻意的：转录依赖 vision 供应商，回填依赖 embedding 服务，
//! 一边挂了不该拖住另一边。

use std::sync::Arc;
use std::time::Duration;

use cortex_memory::Embedder as _;
use cortex_store::{NewEntityEmbedding, NewFactEmbedding, Vector};

use crate::live::Live;

/// 开关。取值刻意不是 `1` / `true` —— 它只能是读懂了才会写下的东西。
const ENV_ENABLED: &str = "CORTEX_REEMBED";
/// 每轮补多少条。
const ENV_BATCH: &str = "CORTEX_REEMBED_BATCH";
/// 两轮之间隔多久（秒）。
const ENV_INTERVAL: &str = "CORTEX_REEMBED_INTERVAL_SECS";

/// 每轮的默认条数。
///
/// 64 不是性能上的取舍，是**成本上的可预期性**：走云 embedding 时
/// 一轮就是 64 次调用，配合默认 5 分钟的间隔，最坏一小时约 768 条。
/// 一个十万条的库要几天补完 —— 慢，但每一步都是有界的，而且
/// 「新的先补」（见 `Store::facts_missing_embedding` 的排序）
/// 意味着最可能被检索到的那批最先回来。
///
/// 急着补完的部署把这个数调大，那是显式的选择。
const DEFAULT_BATCH: usize = 64;

/// 两轮之间的默认间隔。
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// 启动后等多久跑第一轮。
///
/// 与转录回扫同一个理由：进程刚起来时连接池在预热、embedding 服务
/// 可能还没就绪（compose 里 cortexd 与 embeddings 是同时拉起来的），
/// 此时压一批请求进去只会换来一串超时失败。
const FIRST_SCAN_DELAY: Duration = Duration::from_secs(45);

/// 连续失败多少轮之后放慢节奏。
///
/// embedding 服务没起来时，按原节奏死磕只会刷满日志。退到十倍间隔，
/// 服务回来之后第一轮成功就复位 —— 不做指数退避是因为这里的上限
/// 已经是分钟级，再退下去就变成「好了也要等半小时」。
const SLOW_AFTER_FAILURES: u32 = 3;

/// 起一个后台回填任务。
pub fn spawn(st: crate::state::AppState) {
    if !enabled() {
        tracing::info!(
            "{ENV_ENABLED}=off：换模型后的向量回填**不会**运行。\
             旧模型下的事实将一直进不了向量召回"
        );
        return;
    }
    let batch = usize_env(ENV_BATCH, DEFAULT_BATCH);
    let interval = Duration::from_secs(u64::from(
        u32::try_from(usize_env(ENV_INTERVAL, DEFAULT_INTERVAL.as_secs() as usize)).unwrap_or(300),
    ));

    tokio::spawn(async move {
        tokio::time::sleep(FIRST_SCAN_DELAY).await;

        let Some(live) = st.live().cloned() else {
            return; // mock 后端，没有库可回填
        };

        // 先报数再动手。这是「默认开」这个选择的前提条件：
        // 自动花钱可以，但不能在用户不知道花了多少的情况下花
        live.report_embedding_state().await;

        let mut failures: u32 = 0;
        loop {
            let outcome = scan_all_tenants(&st, &live, batch).await;
            match outcome {
                Outcome::Idle => failures = 0,
                Outcome::Progressed => failures = 0,
                Outcome::Failed => failures = failures.saturating_add(1),
            }
            let wait = if failures >= SLOW_AFTER_FAILURES {
                interval * 10
            } else {
                interval
            };
            tokio::time::sleep(wait).await;
        }
    });
}

/// 一轮的结局。
enum Outcome {
    /// 没有待办 —— 绝大多数时候是这个
    Idle,
    /// 补了至少一条
    Progressed,
    /// 这一轮什么也没补成
    Failed,
}

/// 把一轮跑遍**所有**租户。
///
/// # 为什么这一层是必须的
///
/// 回填器原先只认启动时那个连 `public` 的 `Store`。多用户之后，
/// 新注册的人的记忆**永远不会被回填** —— 而这没有任何症状：
/// 他只是觉得「搜东西好像不太准」，因为向量召回那一路对他是空的。
///
/// 每轮重新列一次租户，不缓存：刚注册的人要在下一轮就被覆盖到。
///
/// 一个租户失败不影响其余的 —— 它们是各自独立的库。
async fn scan_all_tenants(st: &crate::state::AppState, live: &Arc<Live>, batch: usize) -> Outcome {
    let schemas = match st.tenant_schemas().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "列不出租户，这一轮跳过");
            return Outcome::Failed;
        }
    };

    let mut any_progress = false;
    let mut any_failure = false;
    for schema in &schemas {
        let store = match st.tenant_store(schema).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(schema = schema.as_str(), error = %e, "连不上这个租户，跳过");
                any_failure = true;
                continue;
            }
        };
        let bound = live.bind(&store);
        match scan_once(bound.as_live(), batch).await {
            Outcome::Progressed => any_progress = true,
            Outcome::Failed => any_failure = true,
            Outcome::Idle => {}
        }
    }

    // 有进展就算有进展 —— 退避是为了「服务挂了别死磕」，
    // 而只要还有一个租户在推进，服务就没挂
    if any_progress {
        Outcome::Progressed
    } else if any_failure {
        Outcome::Failed
    } else {
        Outcome::Idle
    }
}

/// 一轮回填：取一批待办 → **在事务外**算向量 → 短事务批量插。
///
/// 「算向量在事务外」是 CLAUDE.md 的硬约束：`write_txn` 的第一条语句就取
/// advisory lock 串行化提交顺序，在持锁事务里跨网络等于把全局写入串行度
/// 压到那次 HTTP 的往返上。
async fn scan_once(live: &Arc<Live>, batch: usize) -> Outcome {
    let model = live.embedder().model_id().to_owned();
    let store = live.store();

    let todo = match store.facts_missing_embedding(&model, batch as i64).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "回填待办查询失败，本轮跳过");
            return Outcome::Failed;
        }
    };
    let entities = store
        .entities_missing_embedding(&model, batch as i64)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "实体回填待办查询失败，本轮只补事实");
            Vec::new()
        });

    if todo.is_empty() && entities.is_empty() {
        return Outcome::Idle;
    }

    tracing::info!(
        facts = todo.len(),
        entities = entities.len(),
        model = %model,
        "开始回填 embedding"
    );

    // ── 事务外：算向量 ──────────────────────────────────────
    let texts: Vec<String> = todo
        .iter()
        .map(|(_, s)| s.clone())
        .chain(entities.iter().map(|(_, n)| n.clone()))
        .collect();

    let vectors = match live.embedder().embed(&texts).await {
        Ok(v) => v,
        Err(e) => {
            // 不逐条重试：整批失败几乎总是「服务连不上 / 鉴权不对 /
            // 模型名错了」，逐条重试只会把同一条错误刷 64 遍
            tracing::warn!(error = %e, "向量化失败，本轮不写入");
            return Outcome::Failed;
        }
    };
    if vectors.len() != texts.len() {
        tracing::warn!(
            got = vectors.len(),
            want = texts.len(),
            "embedder 返回条数对不上，本轮丢弃 —— 按位置配对会写错行"
        );
        return Outcome::Failed;
    }

    // ── 事务内：只写 ────────────────────────────────────────
    let split = todo.len();
    let written = store
        .write_txn(async |tx| {
            for ((id, _), v) in todo.iter().zip(&vectors[..split]) {
                let Ok(fact_id) = id.parse() else {
                    // 库里的 id 不是 ULID 说明有人绕过写入通道塞了行。
                    // 跳过而不是让整批失败 —— 一条坏行不该挡住其余的
                    tracing::warn!(id, "fact id 不是合法 ULID，跳过");
                    continue;
                };
                tx.insert_fact_embedding(&NewFactEmbedding {
                    fact_id,
                    embedding_model: model.clone(),
                    embedding: Vector::from(v.clone()),
                })
                .await?;
            }
            for ((id, _), v) in entities.iter().zip(&vectors[split..]) {
                let Ok(entity_id) = id.parse() else {
                    tracing::warn!(id, "entity id 不是合法 ULID，跳过");
                    continue;
                };
                tx.insert_entity_embedding(&NewEntityEmbedding {
                    entity_id,
                    embedding_model: model.clone(),
                    embedding: Vector::from(v.clone()),
                })
                .await?;
            }
            Ok(())
        })
        .await;

    match written {
        Ok(()) => {
            let (covered, stale) = store.embedding_coverage(&model).await.unwrap_or((0, 0));
            tracing::info!(
                facts = todo.len(),
                entities = entities.len(),
                covered,
                stale,
                "回填完成一轮"
            );
            Outcome::Progressed
        }
        Err(e) => {
            tracing::warn!(error = %e, "回填写入失败，本轮全部回滚");
            Outcome::Failed
        }
    }
}

/// 读开关。默认**开**。
///
/// 取值刻意只认 `off` / `false` / `0` 这几个：写了别的（比如 `no`）
/// 会当成开着并留一行 warn，而不是静默关掉 —— 静默关掉正是这整件事
/// 要解决的问题的形状。
fn enabled() -> bool {
    match std::env::var(ENV_ENABLED) {
        Err(_) => true,
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "" => true,
            "off" | "false" | "0" | "no" => false,
            other => {
                tracing::warn!(
                    value = other,
                    "{ENV_ENABLED} 的取值不认识，按**开启**处理。\
                     要关掉请写 off"
                );
                true
            }
        },
    }
}

fn usize_env(key: &str, default: usize) -> usize {
    match std::env::var(key) {
        Err(_) => default,
        Ok(v) if v.trim().is_empty() => default,
        Ok(v) => match v.trim().parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => {
                tracing::warn!(key, value = %v, default, "取值非法，用默认值");
                default
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 开关的取值语义。**不能用 `#[test]` 并行跑** —— `set_var` 是进程全局的。
    ///
    /// 放在一个测试函数里顺序验，与 `cortex-agent` 的沙箱开关测试同一个理由。
    #[test]
    fn the_switch_defaults_to_on_and_only_off_turns_it_off() {
        // SAFETY: 本文件里只有这一个测试函数读写这个变量
        unsafe { std::env::remove_var(ENV_ENABLED) };
        assert!(enabled(), "没设时必须是开的 —— 默认关会让静默失忆原样存在");

        for v in ["off", "OFF", " false ", "0", "no"] {
            unsafe { std::env::set_var(ENV_ENABLED, v) };
            assert!(!enabled(), "{v:?} 应当关掉回填");
        }

        // 写错的取值按**开**处理：静默关掉正是这件事要解决的问题的形状
        for v in ["yes", "disable", "关"] {
            unsafe { std::env::set_var(ENV_ENABLED, v) };
            assert!(
                enabled(),
                "{v:?} 不是认识的关闭取值，必须按开启处理并留下警告，\
                 而不是静默地把回填关掉"
            );
        }

        unsafe { std::env::remove_var(ENV_ENABLED) };
    }

    #[test]
    fn batch_size_falls_back_on_garbage() {
        unsafe { std::env::remove_var(ENV_BATCH) };
        assert_eq!(usize_env(ENV_BATCH, DEFAULT_BATCH), DEFAULT_BATCH);

        for v in ["0", "-5", "abc", ""] {
            unsafe { std::env::set_var(ENV_BATCH, v) };
            assert_eq!(
                usize_env(ENV_BATCH, DEFAULT_BATCH),
                DEFAULT_BATCH,
                "{v:?} 不是正整数，应当回落到默认值而不是让回填器每轮补 0 条\
                 （那等于静默关掉）"
            );
        }
        unsafe { std::env::set_var(ENV_BATCH, "8") };
        assert_eq!(usize_env(ENV_BATCH, DEFAULT_BATCH), 8);
        unsafe { std::env::remove_var(ENV_BATCH) };
    }
}
