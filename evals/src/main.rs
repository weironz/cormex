//! `cortex-evals` 命令行。
//!
//! ```bash
//! cargo run -p cortex-evals -- validate                      # 只校验题集，不需要数据库
//! cargo run -p cortex-evals -- run                           # 跑默认题集，出基线数字
//! cargo run -p cortex-evals -- run --json out.json --md out.md
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use cortex_evals::DEFAULT_SUITE;
use cortex_evals::harness::{CrossLinkOptions, ExtractMode, JudgeKind, RetrievalConfig};
use cortex_evals::report::render_text;
use cortex_evals::runner::{RunOptions, run};
use cortex_evals::suite::Suite;
use cortex_memory::retrieval::RecencyMode;

#[derive(Parser)]
#[command(name = "cortex-evals", about = "Cortex 检索质量评测", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 跑一遍评测，输出报告。
    ///
    /// `Box` 不是洁癖：`RunArgs` 已经攒到二十来个旋钮，比 `Validate`
    /// 大一个数量级，不装箱的话整个 enum 都按它的大小分配
    Run(Box<RunArgs>),
    /// 只做题集的静态校验（不连数据库）—— CI 的第一道门
    Validate {
        #[arg(long, default_value = DEFAULT_SUITE)]
        suite: PathBuf,
    },
}

#[derive(Parser)]
struct RunArgs {
    #[arg(long, default_value = DEFAULT_SUITE)]
    suite: PathBuf,

    /// 候选事实从哪来：seed = 题集手写（确定性，可进 CI）；llm = 真调模型抽取
    #[arg(long, value_enum, default_value_t = Mode::Seed)]
    mode: Mode,

    /// 机器可读报告的落盘路径（供 CI 与基线比对）
    #[arg(long)]
    json: Option<PathBuf>,

    /// 人类可读报告的落盘路径。不给则只打到 stdout
    #[arg(long)]
    md: Option<PathBuf>,

    /// 只跑 id 里含此子串的题（调 bad case 用）
    #[arg(long)]
    only: Option<String>,

    /// 算注入预算用的 context 窗口
    #[arg(long, default_value_t = 128_000)]
    context_window: usize,

    /// 实体向量近似归并的阈值。不给则用生产默认值
    #[arg(long)]
    entity_threshold: Option<f32>,

    /// 跑完保留临时 schema，便于手工查 bad case
    #[arg(long)]
    keep_schema: bool,

    /// 关掉域加权（不把题目的 domain 传给检索器）。跑 A/B 用
    #[arg(long)]
    ignore_domain: bool,

    /// 弃权阈值。不给则用生产默认值。扫这条曲线找召回与噪声的平衡点
    #[arg(long)]
    abstain_below: Option<f64>,

    /// 时间近因的形态：channel = 独立一路；decay = 只当融合分乘数；off = 不要。
    /// 默认与生产一致（off，定案见 `RecencyMode` 的文档）
    #[arg(long, value_enum, default_value_t = Recency::Off)]
    recency: Recency,

    /// `--recency decay` 时的强度与半衰期（天）
    #[arg(long, default_value_t = 0.3)]
    recency_strength: f64,
    #[arg(long, default_value_t = 3.0)]
    recency_half_life: f64,

    /// 开 / 关 episodes 那一路（L0 原文倒查）。都不给则用生产默认值
    #[arg(long)]
    episodes: bool,
    #[arg(long, conflicts_with = "episodes")]
    no_episodes: bool,

    /// 单路强命中的补偿权重。0 = 关。默认与生产一致
    #[arg(long, default_value_t = 0.04)]
    peak_bonus: f64,

    /// 语义地板：全库最近的余弦距离超过它就整体弃权。不给则用生产默认值
    #[arg(long, conflicts_with = "no_semantic_floor")]
    semantic_floor: Option<f64>,

    /// 强制关掉语义地板（A/B 用）
    #[arg(long)]
    no_semantic_floor: bool,

    /// 时间回放退回「纯时间倒序的全貌」（改造前的行为）。A/B 用
    #[arg(long)]
    replay_chronological: bool,

    /// Recall@5 的最低门槛。跑出来低于它就以非零码退出 —— CI 回归门
    #[arg(long)]
    min_recall5: Option<f64>,

    /// 灌完语料后跑一遍跨域连接扫描（`relates_to`）。默认关
    #[arg(long)]
    cross_link: bool,

    /// 谁来判「这一对值不值得连」：prefilter = 预筛全收（确定性，可进 CI）；
    /// cheap = 真调廉价模型（随模型漂移，不进回归门）
    #[arg(long, value_enum, default_value_t = Judge::Prefilter)]
    cross_link_judge: Judge,

    /// 候选的余弦距离带。默认与生产一致
    #[arg(long)]
    cross_link_min_distance: Option<f64>,
    #[arg(long)]
    cross_link_max_distance: Option<f64>,

    /// 单个实体最多连出多少条边
    #[arg(long)]
    cross_link_degree_cap: Option<usize>,

    /// 让边的 statement 也进 BM25/向量索引（改造前的行为）。A/B 用
    #[arg(long)]
    cross_link_indexed: bool,

    /// 关掉「跨域桥带出来的那一侧免域惩罚」。A/B 用
    #[arg(long)]
    no_bridge_exemption: bool,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Judge {
    Prefilter,
    Cheap,
}

impl From<Judge> for JudgeKind {
    fn from(j: Judge) -> Self {
        match j {
            Judge::Prefilter => Self::Prefilter,
            Judge::Cheap => Self::Cheap,
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Mode {
    Seed,
    Llm,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Recency {
    Channel,
    Decay,
    Off,
}

impl From<Mode> for ExtractMode {
    fn from(m: Mode) -> Self {
        match m {
            Mode::Seed => Self::Seed,
            Mode::Llm => Self::Llm,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match real_main().await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("错误：{err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn real_main() -> Result<ExitCode> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cortex_evals=info,warn".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Validate { suite } => {
            let s = Suite::load(&suite)?;
            println!(
                "题集 {} 校验通过：{} 场景 / {} 轮 / {} 条铺垫事实 / {} 道题",
                s.name,
                s.scenarios.len(),
                s.turn_count(),
                s.seed_fact_count(),
                s.questions.len()
            );
            for (kind, n) in s.count_by_kind() {
                println!("  {:<10} {n} 题", kind.label());
            }
            for (lang, n) in s.count_by_lang() {
                println!("  {:<10} {n} 题", lang.label());
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Run(args) => run_command(*args).await,
    }
}

async fn run_command(args: RunArgs) -> Result<ExitCode> {
    let suite = Suite::load(&args.suite)?;

    let database_url = std::env::var("DATABASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .context(
            "没有 DATABASE_URL。评测必须连真库 —— 四路召回里三路是 SQL，\
             脱离 Postgres 测出来的不是这套检索",
        )?;

    let opts = RunOptions {
        database_url,
        mode: args.mode.into(),
        keep_schema: args.keep_schema,
        entity_threshold: args.entity_threshold,
        retrieval: RetrievalConfig {
            context_window: args.context_window,
            abstain_below: args.abstain_below,
            recency: match args.recency {
                Recency::Channel => RecencyMode::Channel,
                Recency::Off => RecencyMode::Off,
                Recency::Decay => RecencyMode::Decay {
                    strength: args.recency_strength,
                    half_life_days: args.recency_half_life,
                },
            },
            // 三态：显式开 / 显式关 / 不给就跟着生产默认走。
            // 用 `!no_episodes` 那种两态写法会让「不给参数」永远等于「开」，
            // 于是默认一改，评测就悄悄跑的不是生产配置了 —— 踩过一次
            episode_channel: match (args.episodes, args.no_episodes) {
                (true, _) => true,
                (_, true) => false,
                _ => RetrievalConfig::default().episode_channel,
            },
            peak_bonus: args.peak_bonus,
            semantic_floor: args.semantic_floor,
            no_semantic_floor: args.no_semantic_floor,
            replay_ranked: !args.replay_chronological,
            bridge_exemption: !args.no_bridge_exemption,
            ..RetrievalConfig::default()
        },
        only: args.only,
        ignore_domain: args.ignore_domain,
        cross_link: CrossLinkOptions {
            enabled: args.cross_link,
            judge: args.cross_link_judge.into(),
            min_distance: args
                .cross_link_min_distance
                .unwrap_or(CrossLinkOptions::default().min_distance),
            max_distance: args
                .cross_link_max_distance
                .unwrap_or(CrossLinkOptions::default().max_distance),
            degree_cap: args
                .cross_link_degree_cap
                .unwrap_or(CrossLinkOptions::default().degree_cap),
            indexed: args.cross_link_indexed,
        },
    };

    let report = run(&suite, &opts).await?;
    let text = render_text(&report);
    println!("{text}");

    if let Some(path) = &args.md {
        std::fs::write(path, &text).with_context(|| format!("写不了 {}", path.display()))?;
        eprintln!("人类可读报告：{}", path.display());
    }
    if let Some(path) = &args.json {
        std::fs::write(path, report.to_json()?)
            .with_context(|| format!("写不了 {}", path.display()))?;
        eprintln!("机器可读报告：{}", path.display());
    }

    // 回归门。数字掉下去就红 —— 这是这套工具唯一能自动阻止倒退的地方
    if let Some(min) = args.min_recall5 {
        let actual = report.overall.recall.get(&5).copied().unwrap_or(0.0);
        if actual < min {
            eprintln!("回归门未过：整体 Recall@5 = {actual:.3}，低于门槛 {min:.3}");
            return Ok(ExitCode::FAILURE);
        }
    }

    Ok(ExitCode::SUCCESS)
}
