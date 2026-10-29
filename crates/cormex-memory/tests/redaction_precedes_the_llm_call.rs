//! Negative filtering must sit **before the LLM call**, not merely "exist".
//!
//! `redaction::scan`'s own unit tests (`src/redaction.rs`) only prove that it
//! recognises credentials. It recognises them — but if it is **wired in at the
//! wrong place**, everything stays green all the same: the raw text still goes
//! out over the wire in full, exactly once, and all that happens is that the
//! extracted facts get thrown away. And that is precisely the Mem0 approach
//! that `memory-content.md` §7 principle 2 calls out by name — "the filter
//! lives in the extraction layer, the raw text has already made a full trip
//! across the network".
//!
//! # How "before" gets proven
//!
//! With an `Extractor` built from a **fake key**:
//!
//! - text containing a credential → must return `Ok(empty)` **immediately**.
//!   No network round trip, so the fake key is never even used.
//! - ordinary text → must **attempt to go out** (getting a 401, or failing to
//!   connect on a machine with no network).
//!
//! The difference between the two admits only one explanation: the filter
//! short-circuits before the network call. Move that `scan` down into
//! `write_candidates` and the first assertion goes red on the spot.
//!
//! No database: `extract()` only needs the LLM, and going through `ingest`
//! would instead tie this test to whether Postgres is up.

use std::sync::Arc;
use std::time::Duration;

use cormex_core::Id;
use cormex_core::config::LlmConfig;
use cormex_llm::LlmClient;
use cormex_memory::embed::HashEmbedder;
use cormex_memory::extract::{ExtractContext, Extractor};

fn offline_extractor() -> Extractor {
    let cfg = LlmConfig {
        provider: "deepseek".to_string(),
        model: "deepseek-v4-pro".to_string(),
        cheap_model: "deepseek-v4-flash".to_string(),
        base_url: None,
    };
    // Deliberately a fake key: the whole point of this test is that the path
    // which gets blocked never touches it at all
    let llm = LlmClient::from_config(&cfg, "not-a-real-key").expect("should construct offline");
    Extractor::new(llm, Arc::new(HashEmbedder::new()), "test-device")
}

fn ctx() -> ExtractContext {
    ExtractContext::new(Id::new(), chrono::Utc::now())
}

#[tokio::test]
async fn a_credential_short_circuits_before_any_network_call() {
    let extractor = offline_extractor();

    // One second is a very generous upper bound: there is nothing on this path
    // but a handful of regexes. Had it really gone out, DNS + TLS alone would
    // take longer than that
    let out = tokio::time::timeout(
        Duration::from_secs(1),
        extractor.extract(
            "用户：这个 key 还能用吗 AKIAIOSFODNN7EXAMPLE\n助手：我看看。",
            &ctx(),
        ),
    )
    .await
    .expect("a credential means no network round trip at all — a timeout means it went out");

    assert_eq!(
        out.expect("a filter hit is the normal path, not an error")
            .len(),
        0,
        "a turn containing a credential must yield no candidate facts whatsoever"
    );
}

#[tokio::test]
async fn ordinary_text_really_does_reach_the_provider() {
    let extractor = offline_extractor();

    // The negative control. Without it, the test above would be green under an
    // implementation where `extract` always returns empty too — and such an
    // implementation amounts to switching the whole extraction pipeline off
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        extractor.extract(
            "用户：我们生产库定了用 Postgres 17。\n助手：记下了。",
            &ctx(),
        ),
    )
    .await;

    match out {
        // fake key → the provider's 401; no network → connection failure. Either one
        // proves it **tried** to go out
        Ok(Err(_)) => {}
        Ok(Ok(candidates)) => panic!(
            "a fake key somehow extracted {} candidates — meaning the ordinary-text path \
             is short-circuited as well, the negative filter caught it by mistake",
            candidates.len()
        ),
        // a timeout equally means it is waiting on a network response, i.e. it really
        // did reach the call
        Err(_) => {}
    }
}
