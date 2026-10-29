//! When an environment variable is set to the **empty string**, the default
//! must apply; the empty string must not be taken as the user's choice.
//!
//! Its own file, its own single test function: `env::set_var` mutates
//! process-global state, and running in the same binary as other tests in
//! parallel means stepping on each other. (`crates/cormex-agent/tests/
//! sandbox_degraded.rs` exists for the same reason.)
//!
//! # What this test blocks
//!
//! `env::var(k)` returns `Ok("")` rather than `Err` for a variable set to the
//! empty string. So the default in `env::var(k).unwrap_or(default)` **never
//! takes effect** — as long as the variable was set at all, even to empty.
//!
//! This bug is invisible during local development: the way you write `.env` is
//! by omitting the line. It only shows up inside a container — docker compose's
//! `CORMEX_LLM_MODEL: ${CORMEX_LLM_MODEL:-}` **sets the variable to the empty
//! string** when `.env` is missing that entry. This is exactly how the first
//! production deploy blew up: the process started normally, `/health` returned
//! ok, the logs looked entirely fine, right up until the first real
//! conversation came back with DeepSeek's `... but you passed .`.

use cormex_core::Config;

/// One case: the environment variable's name, the getter that pulls it back out
/// of [`Config`], and the expected default.
type Getter = for<'a> fn(&'a Config) -> &'a str;
type Case = (&'static str, Getter, &'static str);

/// What these keys have in common: the empty string is not a legal value for
/// any of them. A setting where empty really is a legal value should not go
/// through `optional`.
const CASES: &[Case] = &[
    ("CORMEXD_BIND", |c| &c.bind, "127.0.0.1:8080"),
    ("S3_BUCKET", |c| &c.s3.bucket, "cormex-blobs"),
    ("S3_REGION", |c| &c.s3.region, "us-east-1"),
    ("CORMEX_LLM_MODEL", |c| &c.llm.model, "deepseek-v4-pro"),
    (
        "CORMEX_LLM_CHEAP_MODEL",
        |c| &c.llm.cheap_model,
        "deepseek-v4-flash",
    ),
    ("CORMEX_DEVICE_ID", |c| &c.device_id, "dev-local"),
];

#[test]
fn empty_env_falls_back_to_default() {
    // SAFETY: this file has only this one test function, so no other thread
    // reads or writes these variables.
    unsafe {
        std::env::set_var("DATABASE_URL", "postgres://u:p@127.0.0.1:5432/x");
        std::env::set_var("CORMEX_LLM_PROVIDER", "deepseek");
        for (key, _, _) in CASES {
            std::env::set_var(key, "");
        }
        // Whitespace counts as empty too: `FOO=  ` and `FOO=` are the same
        // thing in compose
        std::env::set_var("CORMEX_DEVICE_ID", "   ");
    }

    let cfg = Config::from_env()
        .expect("loading should still work with everything set to empty (via the defaults)");

    for (key, get, want) in CASES {
        assert_eq!(
            get(&cfg),
            *want,
            "{key} set to the empty string should fall back to the default \
             {want}, but got {:?} — env::var returns Ok(\"\") for an empty \
             string, and the default got knocked out",
            get(&cfg)
        );
    }

    // ── The required side: an empty string should report "missing" rather
    // than letting an empty string through. Otherwise DATABASE_URL="" travels
    // all the way to sqlx before blowing up, with an unrecognizable error ──
    unsafe { std::env::set_var("DATABASE_URL", "") };
    let err = Config::from_env().expect_err("an empty DATABASE_URL should be an error");
    assert!(
        err.to_string().contains("DATABASE_URL"),
        "an empty DATABASE_URL should report the missing-variable error for \
         DATABASE_URL, but got: {err}"
    );
}
