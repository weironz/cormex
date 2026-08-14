//! 转录回扫器：把预签名直传漏掉的媒体补回记忆。
//!
//! # 为什么需要它
//!
//! 上传有两条路：
//!
//! | 路径 | 服务端见过字节吗 | 转录 |
//! |---|---|---|
//! | `POST /blobs` 中转（≤ 32 MiB） | 见过 | 上传时顺手排一次 |
//! | `presign` + `commit`（> 32 MiB） | **从没见过** | 无从触发 |
//!
//! 也就是说**大文件是纯归档，检索不到**。而大文件恰恰是最值钱的那些
//! （长截图、扫描件、录屏），用户三个月后想找的正是它们。
//!
//! 本模块以 `blobs` 表为权威清单，周期性地找出「可转录的类型 + 还没有任何
//! `blob_transcripts` 行」的对象，从对象存储拉回来补跑一遍。它同时也兜住了
//! 中转路径上转录失败/进程重启丢任务的那部分。
//!
//! # 四条必须守住的东西
//!
//! 1. **尊重 redaction。** 候选查询里就排掉了有墓碑的 blob（见
//!    [`cortex_store::Store::blobs_missing_transcript`]），管线里
//!    [`cortex_memory::transcribe::RedactionGuard`] 再复检一次。
//!    少了第一道，回扫器会周期性地把已抹除的内容从对象存储拉回内存、
//!    送上网 —— 即使结果最终被丢弃，秘密也已经出过进程了。
//! 2. **限速限并发。** 一次把几百个大文件全拉下来会同时打爆内存与 vision
//!    配额。批量、并发、单个体积三个闸门都在下面的常量里。
//! 3. **失败可重试，但不能无限重试同一个坏 blob。** 见 [`Ledger`]。
//! 4. **没配 `CORTEX_VISION_PROVIDER` 就整条关闭**，与上传路径一致。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use cortex_core::CortexError;

use crate::blobs::MediaStore;
use crate::live::{Live, TranscribeOutcome};

/// 两轮回扫之间的间隔。
///
/// 五分钟：直传完成到能被检索到之间的延迟上限。做得更短没有意义 ——
/// 一次 vision 调用本身就要十几秒，而做得更长会让「刚传的大图搜不到」
/// 从「等一会儿」变成「是不是坏了」。
const SCAN_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// 启动后等多久跑第一轮。
///
/// 不在启动瞬间就跑：进程刚起来时连接池在预热、embedding 模型可能还在加载，
/// 此时压一批 vision 调用进去只会让启动更慢，而回扫本来就不急。
const FIRST_SCAN_DELAY: Duration = Duration::from_secs(30);

/// 一轮最多处理多少个 blob。
///
/// 闸门作用在**每轮**而不是总量：积压一千个时它们会分几十轮消化完，
/// 而任一时刻内存与配额的占用都是有界的。
const BATCH: usize = 8;

/// 同时在跑的转录数。
///
/// 2 而不是 8：每个在跑的任务都把整份字节握在内存里（见 [`MAX_BYTES`]），
/// 而 vision 供应商那侧通常也有并发限流 —— 打满只会换来一批 429，
/// 然后这些 blob 全被记成失败。
const CONCURRENCY: usize = 2;

/// 单个 blob 的体积上限。
///
/// 转录要把整份字节读进内存（`BlobStore::get` 没有流式接口），一个 2 GB 的
/// 录屏能直接把进程打死。超过这个数的**永久跳过**并明说 —— 静默跳过会让
/// 「为什么这个视频搜不到」查无可查。
///
/// 64 MiB 是直传门槛（32 MiB）的两倍：真正需要回扫的正是那一档。
const MAX_BYTES: i64 = 64 * 1024 * 1024;

/// 同一个 blob 最多试几次。
const MAX_ATTEMPTS: u32 = 3;

/// SQL 侧的粗筛。
///
/// 只是**粗筛** —— 真正认领与否由转录管线按 MIME 决定。它存在的唯一理由是
/// 不要把整张 `blobs` 表拉进内存。
///
/// 眼下只有图片：`cortex-memory` 里没有可用的 ASR 实现（缺口见
/// [`cortex_memory::transcribe::Transcriber`] 的文档），把 `audio/%` 与
/// `video/%` 放进来只会让回扫器每五分钟把同一批视频认领一次、
/// 拿到 `Unsupported`、再记一次日志。ASR 落地时改这里一行即可。
const MIME_PREFIXES: &[&str] = &["image/%"];

/// 起一个后台回扫任务。
///
/// 未配置 vision 模型时**什么都不做**（连任务都不起），与上传路径一致：
/// 图片照常归档，只是检索不到。
pub fn spawn(st: crate::state::AppState, blobs: MediaStore) {
    let Some(live) = st.live().cloned() else {
        return; // mock 后端，没有库可回扫
    };
    if !live.transcription_enabled() {
        tracing::info!("未配置 vision 模型，转录回扫器不启动");
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_SCAN_DELAY).await;
        // ledger（拉黑坏 blob）**每个租户一份**：hash 是内容寻址的，
        // 两个租户可能各有一份同样字节的图，而其中一个转不了不代表
        // 另一个也转不了（对象存储的键带租户前缀，可能只有一边缺文件）
        let mut ledgers: std::collections::HashMap<String, Ledger> =
            std::collections::HashMap::new();
        let mut tick = tokio::time::interval(SCAN_INTERVAL);
        loop {
            tick.tick().await;

            // 每轮重新列：刚注册的人也要被回扫覆盖到。
            // 原先这里只有启动时那个连 public 的 store，于是新用户传的图
            // **永远不会被补转录** —— 而那没有任何症状，只是搜不到
            let schemas = match st.tenant_schemas().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "列不出租户，本轮回扫跳过");
                    continue;
                }
            };
            for schema in &schemas {
                let Ok(store) = st.tenant_store(schema).await else {
                    tracing::warn!(schema = schema.as_str(), "连不上这个租户，跳过回扫");
                    continue;
                };
                let bound = live.bind(&store);
                let ledger = ledgers.entry(schema.as_str().to_owned()).or_default();
                scan_once(bound.as_live(), &blobs, ledger).await;
            }
            // 已经不存在的租户不必再留着黑名单
            ledgers.retain(|k, _| schemas.iter().any(|s| s.as_str() == k));
        }
    });
}

/// 一轮回扫。
async fn scan_once(live: &Arc<Live>, blobs: &MediaStore, ledger: &mut Ledger) {
    let prefixes: Vec<String> = MIME_PREFIXES.iter().map(|s| (*s).to_string()).collect();

    // 多要一些候选：已被 ledger 拉黑的那些还占着查询结果的位置，
    // 只要 BATCH 条会让一个坏 blob 永久堵住它后面的队列
    let want = (BATCH * 4) as i64;
    let candidates = match live.store().blobs_missing_transcript(&prefixes, want).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "回扫候选查询失败，本轮跳过");
            return;
        }
    };

    let todo: Vec<cortex_store::Blob> = candidates
        .into_iter()
        .filter(|b| !ledger.is_blocked(&b.hash))
        .take(BATCH)
        .collect();
    if todo.is_empty() {
        return;
    }

    tracing::info!(count = todo.len(), "开始回扫补转录");

    // Semaphore 而不是 `buffer_unordered`：闸门要能被 `MAX_BYTES` 那条
    // 提前返回的路径无成本地绕过（拿不到许可才等），而组合子会把每个 future
    // 都排进队列
    let permits = Arc::new(tokio::sync::Semaphore::new(CONCURRENCY));
    let mut tasks = Vec::with_capacity(todo.len());

    for blob in todo {
        let permits = Arc::clone(&permits);
        let live = Arc::clone(live);
        let blobs = blobs.clone();
        tasks.push(tokio::spawn(async move {
            let _permit = permits.acquire_owned().await;
            let verdict = transcribe_one(&live, &blobs, &blob).await;
            (blob.hash, verdict)
        }));
    }

    for t in tasks {
        match t.await {
            Ok((hash, verdict)) => ledger.record(&hash, verdict),
            Err(e) => tracing::warn!(error = %e, "回扫子任务异常结束"),
        }
    }
}

/// 一个 blob 的回扫结局，从 ledger 的角度看。
enum Verdict {
    /// 到此为止，不必再来（成功、被抹除、没人认领、太大）
    Done,
    /// 这次没成，可以再试
    Retry,
}

async fn transcribe_one(live: &Live, blobs: &MediaStore, blob: &cortex_store::Blob) -> Verdict {
    if blob.size_bytes > MAX_BYTES {
        // 明说而不是静默跳过：否则「为什么这个大文件搜不到」查无可查
        tracing::warn!(
            hash = %blob.hash,
            size_bytes = blob.size_bytes,
            limit = MAX_BYTES,
            "对象超过回扫体积上限，永久跳过 —— 转录需要整份字节进内存"
        );
        return Verdict::Done;
    }

    let bytes = match blobs.get(&blob.hash).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(hash = %blob.hash, error = %e, "回扫取字节失败");
            return classify(&e);
        }
    };

    match live
        .transcribe_and_store(&blob.hash, &bytes, &blob.mime)
        .await
    {
        Ok(outcome) => {
            match &outcome {
                TranscribeOutcome::Stored { segments } => {
                    tracing::info!(hash = %blob.hash, segments, "回扫补上了转录");
                }
                TranscribeOutcome::Unsupported { mime } => {
                    // 粗筛放进来了但没有转录器认领（例如 image/heic）。
                    // 重试多少次都是同一个结果，拉黑
                    tracing::info!(hash = %blob.hash, mime, "没有转录器认领，回扫不再重试");
                }
                TranscribeOutcome::Redacted | TranscribeOutcome::Disabled => {
                    tracing::debug!(hash = %blob.hash, "回扫无需继续");
                }
            }
            // 四种结局都是**确定性**的：同样的字节再跑一遍还是同一个答案。
            // 只有 Err 才值得重试
            Verdict::Done
        }
        Err(e) => {
            tracing::warn!(hash = %blob.hash, error = %e, "回扫转录失败");
            classify(&e)
        }
    }
}

/// 「这次失败了」还是「这个永远转不了」。
///
/// [`CortexError::Invalid`] 是内容本身的问题（字节不是合法图片、转录器产出的
/// 段落非法）—— 换一次网络重来一遍，结果一模一样。其余（供应商 5xx、限流、
/// 数据库抖动、对象存储超时）都是环境问题，值得重试。
///
/// 分错方向的代价不对称：把永久错误当成可重试，是每五分钟白烧一次模型调用；
/// 把可重试当成永久，只是这一个 blob 要等到进程重启才再有机会。
/// 因此拿不准时往「可重试」倒 —— 上面还有 [`MAX_ATTEMPTS`] 兜着。
const fn classify(e: &CortexError) -> Verdict {
    match e {
        CortexError::Invalid(_) => Verdict::Done,
        _ => Verdict::Retry,
    }
}

/// 重试账本 —— **进程内**，不落库。
///
/// # 为什么不建一张表
///
/// 落库要么走 [`cortex_store::Store::write_txn`]（那会给每次失败追加一行
/// `sync_log`，把服务端的运维状态推送给每一台设备），要么绕过它裸写
/// （那就打穿了「WriteTxn 是唯一写入口」这个结构性保证，而那个保证的全部
/// 价值就在于没人能绕过）。两个代价都比它要解决的问题大。
///
/// 放弃的是**跨重启的记忆**：进程重启后一个坏 blob 会再被试一次。
/// 这可以接受 —— 单次尝试的成本有界（一次取字节 + 一次模型调用），
/// 而永久坏掉的 blob 本来就是稀有事件；重启也不是高频动作。
/// 真正要防的「每五分钟重试同一个坏 blob 直到天荒地老」被挡住了。
#[derive(Debug, Default)]
struct Ledger {
    /// hash → 已经试过几次。到达 [`MAX_ATTEMPTS`] 或拿到确定性结局即拉黑
    attempts: HashMap<String, u32>,
    blocked: std::collections::HashSet<String>,
}

impl Ledger {
    fn is_blocked(&self, hash: &str) -> bool {
        self.blocked.contains(hash)
    }

    fn record(&mut self, hash: &str, verdict: Verdict) {
        match verdict {
            Verdict::Done => {
                // 成功的那些下一轮本来就查不出来了（有 blob_transcripts 行），
                // 拉黑是为了 Unsupported / 超大 / 内容坏掉这几类：
                // 它们永远满足「没有转录行」这个候选条件
                self.attempts.remove(hash);
                self.blocked.insert(hash.to_owned());
            }
            Verdict::Retry => {
                let n = self.attempts.entry(hash.to_owned()).or_default();
                *n += 1;
                if *n >= MAX_ATTEMPTS {
                    tracing::warn!(
                        hash,
                        attempts = *n,
                        "连续失败达到上限，本进程内不再重试该 blob"
                    );
                    self.blocked.insert(hash.to_owned());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deterministic_outcome_blocks_immediately() {
        // Unsupported / 超大 / 字节坏掉这几类永远满足「没有转录行」这个候选
        // 条件。不立刻拉黑的话，回扫器每五分钟就会把它们再认领一遍
        let mut l = Ledger::default();
        l.record("a", Verdict::Done);
        assert!(
            l.is_blocked("a"),
            "确定性结局必须立刻拉黑，否则会被无限认领"
        );
    }

    #[test]
    fn transient_failures_are_retried_but_not_forever() {
        let mut l = Ledger::default();
        for i in 1..MAX_ATTEMPTS {
            l.record("b", Verdict::Retry);
            assert!(
                !l.is_blocked("b"),
                "第 {i} 次失败就放弃太早了 —— 供应商 5xx 与限流都是会自己好的"
            );
        }
        l.record("b", Verdict::Retry);
        assert!(
            l.is_blocked("b"),
            "连续失败 {MAX_ATTEMPTS} 次后必须停手，否则一个坏 blob 会被无限重试"
        );
    }

    #[test]
    fn only_content_errors_are_treated_as_permanent() {
        // 分错方向的代价不对称：把永久错误当可重试是每轮白烧一次模型调用，
        // 把可重试当永久只是这一个 blob 要等到重启。拿不准往「可重试」倒
        assert!(matches!(
            classify(&CortexError::Invalid("这不是一张合法的图片".into())),
            Verdict::Done
        ));
        for transient in [
            CortexError::Provider("429 Too Many Requests".into()),
            CortexError::Store("connection reset".into()),
            CortexError::Memory("embedder 暂时不可用".into()),
        ] {
            assert!(
                matches!(classify(&transient), Verdict::Retry),
                "{transient} 是环境问题，应当可重试"
            );
        }
    }

    #[test]
    fn the_size_ceiling_is_above_the_direct_upload_threshold() {
        // 回扫要救的正是「大到必须走直传」的那一档。上限若低于直传门槛，
        // 这个模块就一个 blob 都救不了，而且不会有任何报错
        assert!(
            MAX_BYTES > crate::blobs::DIRECT_UPLOAD_LIMIT as i64,
            "回扫体积上限必须高于直传门槛，否则它救不了任何一个直传上来的对象"
        );
    }
}
