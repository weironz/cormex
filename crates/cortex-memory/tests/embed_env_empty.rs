//! An **empty string** in `CORTEX_EMBED_*` must be treated as "unset".
//!
//! Its own file and its own single test function: `env::set_var` mutates
//! process-global state, so running in the same binary as other tests in
//! parallel means stepping on each other. (`cortex-core/tests/env_empty_is_unset.rs`
//! and `cortex-agent/tests/sandbox_degraded.rs` exist for the same reason.)
//!
//! # What this test blocks — an outage that nearly reached production
//!
//! `deploy/docker-compose.yml` says
//! `CORTEX_EMBED_BATCH: ${CORTEX_EMBED_BATCH:-}`. When `.env` has no such entry,
//! compose **sets it to an empty string** rather than leaving it unset, and
//! `env::var` returns `Ok("")` for an empty string.
//!
//! `FastEmbedderConfig::from_env` used to be a bare
//! `if let Ok(v) = env::var(...)`, so `"".parse::<usize>()` failed → a config
//! error was returned → **cortexd refused to start**, while on the operations
//! side nobody had ever configured this variable at all.
//!
//! Within the same crate, `embed_api::from_env` **had** this handling and
//! `embed_local` did not — two implementations giving two answers to the same
//! pitfall, only one of which is right.
//!
//! This is the second appearance of this pitfall (the first was
//! `CORTEX_LLM_MODEL`, written up in the 0.1.1 CHANGELOG). So what this test
//! guards is not one function, it is a class of error.

#![cfg(feature = "local-embed")]

use cortex_memory::embed_local::FastEmbedderConfig;

/// An empty string must be equivalent to "unset", and that holds for all three
/// variables.
///
/// Compared field by field against the result of leaving them unset, rather than
/// merely asserting `is_ok()` — the latter still passes green when "the empty
/// string was accepted as a legal value and quietly changed some field".
#[test]
fn empty_strings_are_treated_as_unset() {
    // First take a baseline: what it looks like with none of the three set
    for k in [
        "CORTEX_EMBED_MODEL",
        "CORTEX_MODEL_CACHE",
        "CORTEX_EMBED_BATCH",
    ] {
        unsafe { std::env::remove_var(k) };
    }
    let baseline = FastEmbedderConfig::from_env()
        .expect("construction must succeed when none of the three variables are set");

    // Now set all three to the empty string — exactly what compose's `${VAR:-}` does
    for k in [
        "CORTEX_EMBED_MODEL",
        "CORTEX_MODEL_CACHE",
        "CORTEX_EMBED_BATCH",
    ] {
        unsafe { std::env::set_var(k, "") };
    }
    let with_empties = FastEmbedderConfig::from_env().unwrap_or_else(|e| {
        panic!(
            "an empty string wrecked the config and cortexd would refuse to start — \
             while operations never configured these variables at all. Error: {e}"
        )
    });

    assert_eq!(
        with_empties.backfill_batch, baseline.backfill_batch,
        "an empty CORTEX_EMBED_BATCH was not treated as \"unset\""
    );
    assert_eq!(
        with_empties.cache_dir, baseline.cache_dir,
        "an empty CORTEX_MODEL_CACHE was not treated as \"unset\" — \
         it decides where to look for 560MB of weights, and pointing it wrong means \
         downloading them from Hugging Face right now"
    );
    assert_eq!(
        format!("{:?}", with_empties.model),
        format!("{:?}", baseline.model),
        "an empty CORTEX_EMBED_MODEL was not treated as \"unset\""
    );

    // Pure whitespace counts as empty too — a line reading `CORTEX_EMBED_BATCH= `
    // in `.env` is something people really write
    unsafe { std::env::set_var("CORTEX_EMBED_BATCH", "   ") };
    let with_blank = FastEmbedderConfig::from_env()
        .expect("pure whitespace must be treated as \"unset\" as well");
    assert_eq!(with_blank.backfill_batch, baseline.backfill_batch);

    // A real value must still take effect — do not turn "tolerate empty strings"
    // into "ignore this variable"
    unsafe { std::env::set_var("CORTEX_EMBED_BATCH", "7") };
    let with_value =
        FastEmbedderConfig::from_env().expect("a legal value should construct successfully");
    assert_eq!(
        with_value.backfill_batch, 7,
        "tolerating empty strings is not the same as ignoring the variable — \
         a value that really was set must take effect"
    );

    // An illegal value must still **fail loudly**. Tolerating empty strings must
    // not let typos through as well: `CORTEX_EMBED_BATCH=64条` quietly falling
    // back to the default is far harder to track down than an error
    unsafe { std::env::set_var("CORTEX_EMBED_BATCH", "abc") };
    let err = FastEmbedderConfig::from_env()
        .expect_err("an illegal value must error out, not silently fall back to the default");
    assert!(
        err.to_string().contains("CORTEX_EMBED_BATCH"),
        "the error must name the variable; got: {err}"
    );

    for k in [
        "CORTEX_EMBED_MODEL",
        "CORTEX_MODEL_CACHE",
        "CORTEX_EMBED_BATCH",
    ] {
        unsafe { std::env::remove_var(k) };
    }
}
