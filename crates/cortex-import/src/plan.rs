//! 先摊开账，再动手。
//!
//! # 为什么 dry-run 是默认
//!
//! 每一对消息触发一次抽取，也就是一次 LLM 调用。三年历史可能是几千次 ——
//! 那是真金白银，而且跑起来之后没有「撤销」这个操作（记忆是 append-only，
//! 撤销要走 redact，而它要显式触发、二次确认、留墓碑）。
//!
//! 所以默认只算不做：把对话数、消息数、会触发多少次抽取、多少 token、
//! 时间跨度全摆出来，然后停下。**这条规矩对界面同样成立** ——
//! 桌面端与网页端的导入页也必须先给这份账，再给「开始」按钮。
//!
//! # 为什么报 token 而不是报钱
//!
//! 各家定价不同、还在变，而且抽取走的是**服务端配的**廉价模型 ——
//! 客户端根本不知道那是哪个。硬编一个价格只会给出一个看起来精确的错数字。
//! 报 token，由知道自己价格的人自己乘。

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, Utc};
use cortex_proto::episodes::{EpisodeAck, NewEpisodeRequest};

use crate::{Conversation, Platform, requests_for};

/// 两次投递之间至少隔这么久。
///
/// 服务端有抽取并发闸门，但**那不是背压** —— 灌得太快只会让队列长在服务端
/// 内存里，而生产那台是 2 核 3.5 GB。这里主动慢下来，是客户端该做的事。
///
/// 150ms ≈ 每秒 6~7 对。三年历史（假设 5000 对）大约 15 分钟 ——
/// 一次性操作，这个速度完全可以接受，而它让服务端始终有余量伺候正常对话。
pub const PACE: Duration = Duration::from_millis(150);

/// 一次导入要做什么。
#[derive(Debug)]
pub struct Plan {
    pub platform: Platform,
    pub conversations: Vec<Conversation>,
}

/// 摊开的账。
///
/// 派生 `serde` 是为了原样走 HTTP 给界面 —— 界面显示的数字与 CLI
/// 打印的数字**必须是同一次计算的结果**，各算各的迟早会对不上。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Estimate {
    pub conversations: usize,
    pub messages: usize,
    /// 会触发多少次抽取 = 多少次 LLM 调用
    pub pairs: usize,
    /// 送进抽取的文本量（粗估）
    pub tokens: usize,
    pub earliest: Option<DateTime<Utc>>,
    pub latest: Option<DateTime<Utc>>,
    /// 有多少条消息落了单（进 episodes，但不产生事实）
    pub unpaired: usize,
}

impl Estimate {
    /// 转成下发给客户端的形状。
    ///
    /// 转换写在这一侧而不是 `cortex-proto` 里：那个 crate 是**纯契约**，
    /// 不依赖任何实现。反过来让它 `use cortex_import` 会把依赖方向倒过来。
    #[must_use]
    pub fn to_wire(&self, platform: Platform) -> cortex_proto::import::ImportEstimate {
        cortex_proto::import::ImportEstimate {
            platform: platform.label().to_string(),
            conversations: self.conversations,
            messages: self.messages,
            pairs: self.pairs,
            tokens: self.tokens,
            earliest: self.earliest.map(|t| t.to_rfc3339()),
            latest: self.latest.map(|t| t.to_rfc3339()),
            unpaired: self.unpaired,
            minutes: self.minutes(),
        }
    }

    /// 按当前限速，至少要跑多少分钟。
    #[must_use]
    pub fn minutes(&self) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        let m = self.pairs as f64 * PACE.as_secs_f64() / 60.0;
        m.ceil()
    }
}

impl Plan {
    #[must_use]
    pub fn estimate(&self) -> Estimate {
        let mut e = Estimate {
            conversations: self.conversations.len(),
            messages: 0,
            pairs: 0,
            tokens: 0,
            earliest: None,
            latest: None,
            unpaired: 0,
        };
        for c in &self.conversations {
            e.messages += c.messages.len();
            let pairs = c.pairs();
            e.pairs += pairs.len();
            e.unpaired += c.messages.len() - pairs.len() * 2;
            for (u, a) in pairs {
                // 与服务端拼给模型的那一段同构（`用户：…\n助手：…`），
                // 所以这个数字是「真的会被送进去的量」，不是文件大小
                e.tokens += cortex_core::injection::estimate_tokens(&u.text)
                    + cortex_core::injection::estimate_tokens(&a.text);
            }
            for m in &c.messages {
                e.earliest = Some(e.earliest.map_or(m.at, |x: DateTime<Utc>| x.min(m.at)));
                e.latest = Some(e.latest.map_or(m.at, |x: DateTime<Utc>| x.max(m.at)));
            }
        }
        e
    }
}

/// 灌入的出口。三处各实现一份：CLI 打远端 HTTP、本地 agent 打远端 HTTP、
/// cortexd 直接走自己的写入路径。
///
/// # 为什么要抽成 trait，而不是让三处各抄一遍 [`execute`]
///
/// 那个循环里有三件**顺序敏感**的事，抄错任何一件都不报错、只是结果不对：
///
/// 1. user 必须先于 assistant 落库（anchor 指向它，服务端要回读原文）
/// 2. 两次投递之间要限速（不然队列长在服务端内存里）
/// 3. 改标题必须在内容之后（会话要先存在）
///
/// 抽成 trait，这三条只有一份实现。
pub trait Sink {
    /// 写一条 episode。已经存在时 `already_existed` 为真（幂等靠它）。
    fn write_episode(
        &self,
        req: &NewEpisodeRequest,
    ) -> impl Future<Output = Result<EpisodeAck>> + Send;

    /// 给会话改名。失败不致命 —— 内容已经进去了，少个标题而已。
    fn rename_session(
        &self,
        session_id: &str,
        title: &str,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// 一次导入跑到哪儿了。
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Progress {
    /// 已处理的对话数
    pub conversations_done: usize,
    pub conversations_total: usize,
    /// 已灌入的对数
    pub pairs_done: usize,
    /// 服务端认出「早就有了」的条数 —— 没有重复计费
    pub skipped: usize,
    pub failures: usize,
}

/// 真跑。
///
/// # 断点续传是免费的
///
/// episode id 由 [`cortex_core::Id::derived`] 从「原始时刻 + 稳定种子」算出，
/// 而 `POST /episodes` 按 id 判重。所以中断之后重跑同一条命令即可 ——
/// 已经写过的会被服务端识别出来（`already_existed`），不重复计费。
/// 不需要任何进度文件，也就没有「进度文件与实际状态不一致」这条故障。
///
/// # Errors
/// 目前不返回 `Err` —— 单条写入失败只计数不中断（重跑会补上）。
/// 签名留着 `Result` 是为了将来能加「连续失败太多就停」这类判据，
/// 而那时改签名会波及三个调用方。
pub async fn execute<S: Sink, F: FnMut(Progress)>(
    sink: &S,
    plan: &Plan,
    mut on_progress: F,
) -> Result<Progress> {
    let mut p = Progress {
        conversations_done: 0,
        conversations_total: plan.conversations.len(),
        pairs_done: 0,
        skipped: 0,
        failures: 0,
    };

    for conv in &plan.conversations {
        let pairs = conv.pairs();
        if pairs.is_empty() {
            p.conversations_done += 1;
            on_progress(p);
            continue;
        }
        let session = conv.session_id(plan.platform).to_string();

        for (u, a) in pairs {
            let [user_req, asst_req] = requests_for(plan.platform, conv, u, a);
            // user 必须先落：assistant 那条的 anchor 指向它，而服务端会拿
            // anchor 去库里读回 user 的原文来拼抽取输入。反过来的话，
            // 抽取会因为「找不到对应的 user episode」而整轮跳过
            match sink.write_episode(&user_req).await {
                Ok(ack) if ack.already_existed => p.skipped += 1,
                Ok(_) => {}
                Err(e) => {
                    p.failures += 1;
                    tracing::warn!(error = %e, "写 user episode 失败，跳过这一对");
                    continue;
                }
            }
            match sink.write_episode(&asst_req).await {
                Ok(ack) => {
                    if ack.already_existed {
                        p.skipped += 1;
                    }
                    p.pairs_done += 1;
                }
                Err(e) => {
                    p.failures += 1;
                    tracing::warn!(error = %e, "写 assistant episode 失败");
                }
            }

            tokio::time::sleep(PACE).await;
        }

        // 标题放在最后：会话要先有内容，改名才有东西可改。
        // 失败不算致命 —— 内容已经进去了，少一个标题而已
        if let Err(e) = sink
            .rename_session(&session, &conv.display_title(plan.platform))
            .await
        {
            tracing::debug!(error = %e, session, "会话改名失败");
        }

        p.conversations_done += 1;
        on_progress(p);
    }
    Ok(p)
}

/// 解析一份已经读进内存的导出文件，顺带做时间与数量的裁剪。
///
/// # Errors
/// 解析不了，或者裁剪之后一段对话都不剩。
pub fn plan_from_bytes(
    bytes: &[u8],
    platform: Option<Platform>,
    since: Option<DateTime<Utc>>,
    max: Option<usize>,
) -> Result<Plan> {
    let (platform, mut conversations) = crate::parse(bytes, platform)?;

    if let Some(since) = since {
        conversations.retain(|c| c.messages.iter().any(|m| m.at >= since));
    }
    // 新的排前面：`--max-conversations 5` 试水时，要试的显然是最近的那几段
    conversations.sort_by_key(|c| std::cmp::Reverse(c.at));
    if let Some(n) = max {
        conversations.truncate(n);
    }

    if conversations.is_empty() {
        bail!("按当前的时间 / 数量筛选条件筛完之后，一段对话都不剩");
    }
    Ok(Plan {
        platform,
        conversations,
    })
}

/// 读文件并解析成 `Plan`。
///
/// # Errors
/// 文件读不了，或者 [`plan_from_bytes`] 报的任何错。
pub fn load(
    path: &std::path::Path,
    platform: Option<Platform>,
    since: Option<DateTime<Utc>>,
    max: Option<usize>,
) -> Result<Plan> {
    let bytes = std::fs::read(path).with_context(|| format!("读不了 {}", path.display()))?;
    plan_from_bytes(&bytes, platform, since, max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, Role};

    fn conv(n_pairs: usize, extra_dangling: bool) -> Conversation {
        let mut messages = Vec::new();
        for i in 0..n_pairs {
            messages.push(Message {
                source_id: format!("u{i}"),
                role: Role::User,
                text: "问一个不长不短的问题".into(),
                at: format!("2024-03-0{}T10:00:00Z", i + 1).parse().unwrap(),
            });
            messages.push(Message {
                source_id: format!("a{i}"),
                role: Role::Assistant,
                text: "给一个不长不短的回答".into(),
                at: format!("2024-03-0{}T10:00:01Z", i + 1).parse().unwrap(),
            });
        }
        if extra_dangling {
            messages.push(Message {
                source_id: "dangling".into(),
                role: Role::User,
                text: "最后一问没人答".into(),
                at: "2024-03-09T10:00:00Z".parse().unwrap(),
            });
        }
        Conversation {
            source_id: "c".into(),
            title: "t".into(),
            at: "2024-03-01T10:00:00Z".parse().unwrap(),
            messages,
        }
    }

    /// 账要算对：**抽取次数等于对数**，不是消息数。
    ///
    /// 这个数字直接就是钱。按消息数报会让用户以为要花两倍。
    #[test]
    fn the_estimate_counts_extraction_calls_not_messages() {
        let plan = Plan {
            platform: Platform::ChatGpt,
            conversations: vec![conv(3, false), conv(2, false)],
        };
        let e = plan.estimate();
        assert_eq!(e.conversations, 2);
        assert_eq!(e.messages, 10);
        assert_eq!(e.pairs, 5, "5 对 = 5 次 LLM 调用，不是 10 次");
        assert!(e.tokens > 0);
        assert_eq!(e.unpaired, 0);
    }

    /// 落单的消息要单独报出来。
    ///
    /// 它们会写进原文却不产生任何事实 —— 用户看到「导入了 100 条消息」
    /// 却只多了 40 条记忆时，需要有个地方解释这个差。
    #[test]
    fn dangling_messages_are_reported_separately() {
        let plan = Plan {
            platform: Platform::Claude,
            conversations: vec![conv(2, true)],
        };
        let e = plan.estimate();
        assert_eq!(e.pairs, 2);
        assert_eq!(e.unpaired, 1, "最后那一问没人答，要报出来");
    }

    /// 时间跨度取的是**消息**的时刻，不是对话的创建时刻。
    #[test]
    fn the_span_comes_from_the_messages() {
        let plan = Plan {
            platform: Platform::ChatGpt,
            conversations: vec![conv(3, false)],
        };
        let e = plan.estimate();
        assert_eq!(
            e.earliest.unwrap().format("%Y-%m-%d").to_string(),
            "2024-03-01"
        );
        assert_eq!(
            e.latest.unwrap().format("%Y-%m-%d").to_string(),
            "2024-03-03"
        );
    }

    /// `Estimate` 要能原样走 HTTP —— 界面上的数字必须与 CLI 打印的
    /// 出自**同一次计算**，各算各的迟早会对不上。
    #[test]
    fn the_estimate_survives_a_round_trip() {
        let plan = Plan {
            platform: Platform::Claude,
            conversations: vec![conv(2, true)],
        };
        let e = plan.estimate();
        let back: Estimate =
            serde_json::from_str(&serde_json::to_string(&e).expect("序列化")).expect("反序列化");
        assert_eq!(back.pairs, e.pairs);
        assert_eq!(back.tokens, e.tokens);
        assert_eq!(back.unpaired, e.unpaired);
        assert_eq!(back.earliest, e.earliest, "时刻不能在往返里丢精度");
    }

    /// 一个假的出口，用来验 [`execute`] 那三条顺序敏感的规矩。
    #[derive(Default)]
    struct Recorder {
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl Sink for Recorder {
        async fn write_episode(&self, req: &NewEpisodeRequest) -> Result<EpisodeAck> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("write:{}:{}", req.role, req.id));
            Ok(EpisodeAck {
                episode_id: req.id.clone(),
                already_existed: false,
                memories: Vec::new(),
            })
        }
        async fn rename_session(&self, session_id: &str, _title: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("rename:{session_id}"));
            Ok(())
        }
    }

    /// **user 必须先落，改标题必须最后。**
    ///
    /// 顺序反了都不报错：assistant 先落会让服务端读不回 user 的原文，
    /// 于是整轮抽取被跳过 —— 原文进去了，事实一条没有。
    #[tokio::test]
    async fn writes_go_user_first_and_the_title_comes_last() {
        let sink = Recorder::default();
        let plan = Plan {
            platform: Platform::Claude,
            conversations: vec![conv(2, false)],
        };
        execute(&sink, &plan, |_| {}).await.expect("跑通");

        let calls = sink.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 5, "两对写四条 + 一次改名，实际：{calls:?}");
        assert!(calls[0].starts_with("write:user"), "第一条必须是 user");
        assert!(calls[1].starts_with("write:assistant"));
        assert!(
            calls[4].starts_with("rename:"),
            "改标题必须在内容之后 —— 会话要先有东西才改得动"
        );
    }
}
