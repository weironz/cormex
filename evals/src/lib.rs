//! Cortex 检索质量评测。
//!
//! 项目文档自己说「瓶颈是检索质量不是数据量」，但在这个 crate 之前，
//! RRF 的 k 值、四路召回宽度、域加权系数、预算截断——全部只能凭感觉调。
//! 这里做的事只有一件：**让每一次「优化」都有数字可以对照**。
//!
//! ```text
//! 题集(JSON) ──► 真实 ingest 管线 ──► 隔离的临时 schema
//!                                          │
//!                     ┌────────────────────┴──────────────────┐
//!                     ▼                                       ▼
//!            Retriever::retrieve                    四路原始召回（诊断）
//!            （四路 + RRF + 域加权 + 预算）          bm25/vector/graph/recency
//!                     │                                       │
//!                     └──────────────► 指标 ◄─────────────────┘
//!                        Recall@k · MRR · 逐路独占 · 误召率
//!                                     │
//!                          人类可读报告 + 机器可读 JSON
//! ```
//!
//! 各模块的取舍写在各自的文件头：
//! - [`suite`] 题集格式与静态校验（为什么 gold 不能按 fact id 写）
//! - [`matcher`] gold 匹配（为什么只看 statement）
//! - [`metrics`] 指标（为什么必须分组、为什么必须有逐路贡献）
//! - [`harness`] 隔离与真实管线（为什么不能直接 INSERT facts）
//! - [`report`] 报告（为什么 JSON 与文本都要）

pub mod harness;
pub mod matcher;
pub mod metrics;
pub mod report;
pub mod runner;
pub mod suite;

/// 仓库内自建题集的默认路径（相对仓库根）。
pub const DEFAULT_SUITE: &str = "evals/suites/cortex-zh-en-v1.json";
