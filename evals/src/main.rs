//! The `cortex-evals` command line.
//!
//! ```bash
//! cargo run -p cortex-evals -- validate   # validate the suite only, no database needed
//! cargo run -p cortex-evals -- run        # run the default suite, produce baseline numbers
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
    /// Run the eval once and emit a report.
    ///
    /// The `Box` is not fastidiousness: `RunArgs` has accumulated some twenty
    /// knobs, an order of magnitude more than `Validate`, and without boxing
    /// the whole enum would be allocated at its size
    Run(Box<RunArgs>),
    /// Static validation of the suite only (no database) — CI's first gate
    Validate {
        #[arg(long, default_value = DEFAULT_SUITE)]
        suite: PathBuf,
    },
}

#[derive(Parser)]
struct RunArgs {
    #[arg(long, default_value = DEFAULT_SUITE)]
    suite: PathBuf,

    /// Where candidate facts come from: seed = handwritten in the suite
    /// (deterministic, CI-safe); llm = really call the model to extract
    #[arg(long, value_enum, default_value_t = Mode::Seed)]
    mode: Mode,

    /// Path to write the machine-readable report (for CI and baseline
    /// comparison)
    #[arg(long)]
    json: Option<PathBuf>,

    /// Path to write the human-readable report. Without it, stdout only
    #[arg(long)]
    md: Option<PathBuf>,

    /// Run only the questions whose id contains this substring (for chasing bad
    /// cases)
    #[arg(long)]
    only: Option<String>,

    /// The context window used to compute the injection budget
    #[arg(long, default_value_t = 128_000)]
    context_window: usize,

    /// Threshold for approximate entity-vector merging. Without it, the
    /// production default
    #[arg(long)]
    entity_threshold: Option<f32>,

    /// Keep the temp schema after the run, so bad cases can be inspected by
    /// hand
    #[arg(long)]
    keep_schema: bool,

    /// Turn off domain weighting (do not pass the question's domain to the
    /// retriever). For A/B runs
    #[arg(long)]
    ignore_domain: bool,

    /// Abstention threshold. Without it, the production default. Sweep this
    /// curve to find the balance point between recall and noise
    #[arg(long)]
    abstain_below: Option<f64>,

    /// The form of temporal recency: channel = its own retrieval path; decay =
    /// only a multiplier on the fused score; off = none. The default matches
    /// production (off; the decision is documented on `RecencyMode`)
    #[arg(long, value_enum, default_value_t = Recency::Off)]
    recency: Recency,

    /// Strength and half-life (in days) when `--recency decay` is used
    #[arg(long, default_value_t = 0.3)]
    recency_strength: f64,
    #[arg(long, default_value_t = 3.0)]
    recency_half_life: f64,

    /// Turn the episodes path (L0 raw-text reverse lookup) on / off. With
    /// neither flag, the production default
    #[arg(long)]
    episodes: bool,
    #[arg(long, conflicts_with = "episodes")]
    no_episodes: bool,

    /// Compensation weight for a strong hit on a single path. 0 = off. The
    /// default matches production
    #[arg(long, default_value_t = 0.04)]
    peak_bonus: f64,

    /// Semantic floor: abstain entirely when the closest cosine distance in the
    /// whole store exceeds it. Without it, the production default
    #[arg(long, conflicts_with = "no_semantic_floor")]
    semantic_floor: Option<f64>,

    /// Force the semantic floor off (for A/B)
    #[arg(long)]
    no_semantic_floor: bool,

    /// Make time replay fall back to "the full picture in pure reverse
    /// chronological order" (the pre-rework behavior). For A/B
    #[arg(long)]
    replay_chronological: bool,

    /// Minimum threshold for Recall@5. A run below it exits with a non-zero
    /// code — the CI regression gate
    #[arg(long)]
    min_recall5: Option<f64>,

    /// Run a cross-domain link scan (`relates_to`) after the corpus is loaded.
    /// Off by default
    #[arg(long)]
    cross_link: bool,

    /// Who decides "is this pair worth linking": prefilter = accept everything
    /// the prefilter yields (deterministic, CI-safe); cheap = really call the
    /// cheap model (drifts with the model, not part of the regression gate)
    #[arg(long, value_enum, default_value_t = Judge::Prefilter)]
    cross_link_judge: Judge,

    /// The candidates' cosine distance band. The default matches production
    #[arg(long)]
    cross_link_min_distance: Option<f64>,
    #[arg(long)]
    cross_link_max_distance: Option<f64>,

    /// The maximum number of edges a single entity may link out
    #[arg(long)]
    cross_link_degree_cap: Option<usize>,

    /// Let an edge's statement enter the BM25/vector index too (the pre-rework
    /// behavior). For A/B
    #[arg(long)]
    cross_link_indexed: bool,

    /// Turn off "the side pulled in by a cross-domain bridge is exempt from the
    /// domain penalty". For A/B
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
            // Three states: explicitly on / explicitly off / neither, follow
            // the production default. A two-state form like `!no_episodes`
            // would make "no flag given" forever equal to "on", so the moment
            // the default changed, the eval would quietly stop running the
            // production configuration — hit once already
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

    // The regression gate. If the number drops, it goes red — this is the only
    // place in this toolchain that can stop a regression automatically
    if let Some(min) = args.min_recall5 {
        let actual = report.overall.recall.get(&5).copied().unwrap_or(0.0);
        if actual < min {
            eprintln!("回归门未过：整体 Recall@5 = {actual:.3}，低于门槛 {min:.3}");
            return Ok(ExitCode::FAILURE);
        }
    }

    Ok(ExitCode::SUCCESS)
}
