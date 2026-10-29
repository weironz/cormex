//! The `cormex-evals` command line.
//!
//! ```bash
//! cargo run -p cormex-evals -- validate   # validate the suite only, no database needed
//! cargo run -p cormex-evals -- run        # run the default suite, produce baseline numbers
//! cargo run -p cormex-evals -- run --json out.json --md out.md
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use cormex_evals::harness::{CrossLinkOptions, ExtractMode, JudgeKind, RetrievalConfig};
use cormex_evals::report::render_text;
use cormex_evals::runner::{RunOptions, run};
use cormex_evals::suite::Suite;
use cormex_evals::{DEFAULT_SESSION_SUITE, DEFAULT_SUITE};
use cormex_memory::retrieval::RecencyMode;

#[derive(Parser)]
#[command(
    name = "cormex-evals",
    about = "Cormex retrieval-quality evaluation",
    version
)]
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
        /// Defaults to the memory suite, or to the session suite with
        /// `--sessions`.
        #[arg(long)]
        suite: Option<PathBuf>,
        /// Validate the session-retrieval suite instead of the memory one.
        #[arg(long)]
        sessions: bool,
    },
}

#[derive(Parser)]
struct RunArgs {
    /// Which side to evaluate: `memory` runs the fact-retrieval suite,
    /// `sessions` runs the session-retrieval one.
    ///
    /// Two targets rather than one report: they share no table, no index and no
    /// ranking, so a number from one says nothing about the other, and folding
    /// them together would let a good memory score hide a broken archive.
    #[arg(long, value_enum, default_value_t = Target::Memory)]
    target: Target,

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

    /// `--target sessions` only: also build conversation vectors and fuse the
    /// vector leg with BM25 (P5-6). Off matches the production default; on is
    /// the A/B that decides whether the leg is worth its cost.
    #[arg(long)]
    session_vectors: bool,

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
enum Target {
    Memory,
    Sessions,
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
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn real_main() -> Result<ExitCode> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cormex_evals=info,warn".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Validate { suite, sessions } => {
            if sessions {
                let path = suite.unwrap_or_else(|| PathBuf::from(DEFAULT_SESSION_SUITE));
                let s = cormex_evals::sessions::SessionSuite::load(&path)?;
                s.validate()?;
                println!(
                    "session suite {} validated: {} sessions / {} events / {} questions",
                    s.name,
                    s.sessions.len(),
                    s.event_count(),
                    s.questions.len()
                );
                return Ok(ExitCode::SUCCESS);
            }
            let path = suite.unwrap_or_else(|| PathBuf::from(DEFAULT_SUITE));
            let s = Suite::load(&path)?;
            println!(
                "suite {} validated: {} scenarios / {} turns / {} seeded facts / {} questions",
                s.name,
                s.scenarios.len(),
                s.turn_count(),
                s.seed_fact_count(),
                s.questions.len()
            );
            for (kind, n) in s.count_by_kind() {
                println!("  {:<10} {n} questions", kind.label());
            }
            for (lang, n) in s.count_by_lang() {
                println!("  {:<10} {n} questions", lang.label());
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Run(args) => run_command(*args).await,
    }
}

async fn run_command(args: RunArgs) -> Result<ExitCode> {
    if args.target == Target::Sessions {
        return run_sessions(args).await;
    }
    let suite = Suite::load(&args.suite)?;

    let database_url = std::env::var("DATABASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .context(
            "DATABASE_URL is not set. The eval must run against a real database -- three of the \
             four retrieval legs are SQL, and what you measure without Postgres is not this \
             retrieval system",
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
        std::fs::write(path, &text).with_context(|| format!("cannot write {}", path.display()))?;
        eprintln!("human-readable report: {}", path.display());
    }
    if let Some(path) = &args.json {
        std::fs::write(path, report.to_json()?)
            .with_context(|| format!("cannot write {}", path.display()))?;
        eprintln!("machine-readable report: {}", path.display());
    }

    // The regression gate. If the number drops, it goes red — this is the only
    // place in this toolchain that can stop a regression automatically
    if let Some(min) = args.min_recall5 {
        let actual = report.overall.recall.get(&5).copied().unwrap_or(0.0);
        if actual < min {
            eprintln!(
                "regression gate failed: overall Recall@5 = {actual:.3}, below the {min:.3} threshold"
            );
            return Ok(ExitCode::FAILURE);
        }
    }

    Ok(ExitCode::SUCCESS)
}

/// The session-retrieval target.
///
/// Its own function rather than a branch inside `run_command`: the two targets
/// share almost nothing — no suite type, no report type, no gate metric — and
/// interleaving them would make both harder to read than the duplication costs.
async fn run_sessions(args: RunArgs) -> Result<ExitCode> {
    use cormex_evals::sessions::{SessionSuite, run_questions, seed_sessions};

    let path = if args.suite.as_os_str() == DEFAULT_SUITE {
        // The default points at the memory suite; a `--target sessions` run
        // without `--suite` means the session one
        PathBuf::from(DEFAULT_SESSION_SUITE)
    } else {
        args.suite.clone()
    };
    let suite = SessionSuite::load(&path)?;
    suite.validate()?;

    let database_url = std::env::var("DATABASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .context(
            "DATABASE_URL is not set. Both archive indexes are SQL, and what you measure              without Postgres is not this search system",
        )?;

    let db = cormex_evals::harness::EvalDb::create(&database_url).await?;
    let schema = db.schema().to_owned();
    let outcome = async {
        seed_sessions(&db.store, &suite).await?;
        if !args.session_vectors {
            return run_questions(&db.store, &suite).await;
        }
        let embedder = cormex_evals::harness::default_embedder().await?;
        let n = cormex_evals::sessions::seed_vectors(&db.store, &embedder).await?;
        eprintln!("wrote {n} conversation vectors ({})", embedder.model_id());
        cormex_evals::sessions::run_questions_with(
            &db.store,
            &suite,
            cormex_evals::sessions::VectorLeg::On,
            Some(&embedder),
        )
        .await
    }
    .await;
    let keep = args.keep_schema || outcome.is_err();
    db.cleanup(keep).await;

    let report = outcome?;
    let text = report.render();
    println!("{text}");
    eprintln!("(schema {schema})");

    if let Some(path) = &args.md {
        std::fs::write(path, &text).with_context(|| format!("cannot write {}", path.display()))?;
        eprintln!("human-readable report: {}", path.display());
    }
    if let Some(path) = &args.json {
        let json = serde_json::to_string_pretty(&report)
            .context("the session report could not be serialized")?;
        std::fs::write(path, json).with_context(|| format!("cannot write {}", path.display()))?;
        eprintln!("machine-readable report: {}", path.display());
    }

    // The gate is **accuracy over all questions**, negatives included: a run
    // that returns everything scores well on the positive questions and fails
    // the negatives, which is exactly what the gate should catch
    if let Some(min) = args.min_recall5 {
        let actual = report.accuracy();
        if actual < min {
            eprintln!(
                "regression gate failed: session accuracy = {actual:.3}, below the {min:.3} \
                 threshold"
            );
            return Ok(ExitCode::FAILURE);
        }
    }
    Ok(ExitCode::SUCCESS)
}
