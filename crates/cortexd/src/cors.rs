//! Cross-origin policy — "which web page is allowed to read cortexd's
//! responses".
//!
//! This used to be [`CorsLayer::permissive`], meaning **any origin** could read
//! the response body of any endpoint. A comment said "production should tighten
//! this", but a sentence like that does not take effect on its own.
//!
//! # Why this is still a problem under bearer tokens
//!
//! The usual defence is "a malicious page cannot obtain the token, so it cannot
//! read anything". That is half right: the authentication work introduced a
//! **short-lived `?ticket=` credential** for browsers (see [`crate::auth`]),
//! because `EventSource` / `WebSocket` / `<img src>` cannot attach request
//! headers. A ticket is a thing that **goes into the URL** — it lands in the
//! reverse proxy's access log, in browser history, in the referrer. The moment
//! a ticket reaches a third-party page by any route, permissive CORS means that
//! page can `fetch` with it and **read the response**. Tightened, the same
//! ticket can only be used from an allowed origin inside a browser.
//!
//! **CORS is a browser-side control only**, and that has to be said plainly:
//! `curl` holding a ticket is unaffected. What this buys is "an arbitrary web
//! page cannot read my memory database", not "it is fine if a ticket leaks". The
//! ticket's own short TTL is the answer to the latter.
//!
//! # The production shape: same-origin, hence no cross-origin allowed by default
//!
//! Production is a single domain with path-based routing (`/` → Flutter web,
//! `/api/*` → cortexd, with the reverse proxy stripping the `/api` prefix). The
//! web app and cortexd are **same-origin**, so the browser never issues a
//! cross-origin request at all and no CORS header is needed. That same-origin
//! arrangement was chosen deliberately.
//!
//! Which leaves exactly one correct default: **allow nothing**. On a development
//! machine `flutter run -d chrome` comes up on a random port, and that genuinely
//! is cross-origin — for that situation, configure an allow-list explicitly
//! instead of leaving every deployment wide open for the sake of development
//! convenience.
//!
//! | `CORTEX_CORS_ALLOWED_ORIGINS` | Effect |
//! |---|---|
//! | unset / empty | no cross-origin request allowed (the production default) |
//! | `loopback` | `http(s)://localhost` and loopback IPs on any port (the development default) |
//! | `https://a.example,https://b.example` | only those two origins |
//! | `*` | **refuses to start**; reasoning in [`CorsPolicy::from_env`] |
//!
//! A comma-separated list may mix `loopback` with specific origins.
//!
//! # Why `Access-Control-Allow-Credentials` is not set
//!
//! This service's credentials are bearer tokens and query-string tickets, both
//! of which the client attaches **explicitly**; neither is a cookie the browser
//! sends automatically. Enabling credentials would introduce the entire CSRF
//! problem class in exchange for nothing.

use std::collections::BTreeSet;

use axum::http::{HeaderValue, Method, header};
use tower_http::cors::{AllowOrigin, CorsLayer};

/// Environment variable holding the allow-list.
pub const ALLOWED_ORIGINS_ENV: &str = "CORTEX_CORS_ALLOWED_ORIGINS";

/// The literal in the list meaning "local loopback on any port".
///
/// Its reason to exist is very concrete: `flutter run -d chrome` comes up on a
/// **random port** every time, so a hard-coded `http://localhost:5000` is wrong
/// by the next day — and that failure shows up in the browser as an inscrutable
/// CORS error, at which point most people simply switch back to permissive.
/// Providing a dedicated literal is safer than forcing people to write a
/// wildcard: what it matches is **three explicitly enumerated** hostnames, not
/// "any origin".
pub const LOOPBACK_TOKEN: &str = "loopback";

/// The loopback hostnames that may appear in the allow-list.
///
/// Only these three literals are recognised; there is no "starts with localhost"
/// prefix test — `http://localhost.evil.com` is exactly what sneaks through
/// that kind of test.
const LOOPBACK_HOSTS: &[&str] = &["localhost", "127.0.0.1", "[::1]"];

/// Which cross-origin origins this deployment allows to read responses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CorsPolicy {
    /// Exactly matched origins (case sensitive, including scheme and port)
    exact: BTreeSet<String>,
    /// Loopback on any port. See [`LOOPBACK_TOKEN`]
    loopback: bool,
}

impl CorsPolicy {
    /// Load from the environment.
    ///
    /// # Why a misconfiguration errors instead of being ignored
    ///
    /// If a malformed origin (a stray trailing slash, or `example.com` written
    /// without a scheme) were silently dropped, the symptom would be "I
    /// configured it and it does not take effect" — and in a browser that class
    /// of problem only ever surfaces as a generic CORS failure, whose first
    /// instinct is to widen the configuration. Failing on the spot puts the
    /// error where it actually happened. Same trade-off as
    /// [`crate::auth::AuthMode`].
    ///
    /// # Why `*` is an error rather than an accepted value
    ///
    /// It is simply permissive again, spelled differently. Anyone who genuinely
    /// wants that should edit this code and read the argument about tickets
    /// above, not type an asterisk into `.env`.
    pub fn from_env() -> cortex_core::Result<Self> {
        Self::parse(
            std::env::var(ALLOWED_ORIGINS_ENV)
                .unwrap_or_default()
                .as_str(),
        )
    }

    /// Parse a comma-separated allow-list. An empty string = no cross-origin
    /// request allowed.
    pub fn parse(raw: &str) -> cortex_core::Result<Self> {
        let mut policy = Self::default();
        for item in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if item == "*" {
                return Err(cortex_core::CortexError::Config(format!(
                    "{ALLOWED_ORIGINS_ENV} 不接受 `*`。\n\
                     \n\
                     允许任意来源意味着任何网页都能拿一张泄露的 ?ticket= 去读你的\n\
                     记忆库响应 —— 而票据本来就是要进 URL 的东西（见 src/auth.rs）。\n\
                     \n\
                     生产部署里 Web 与 cortexd 同源，压根不需要任何 CORS：把这个\n\
                     变量删掉即可。开发机上 `flutter run -d chrome` 用随机端口，\n\
                     写 {ALLOWED_ORIGINS_ENV}={LOOPBACK_TOKEN}。"
                )));
            }
            if item == LOOPBACK_TOKEN {
                policy.loopback = true;
                continue;
            }
            validate_origin(item)?;
            policy.exact.insert(item.to_string());
        }
        Ok(policy)
    }

    /// No cross-origin request allowed — the production shape.
    #[must_use]
    pub fn denies_everything(&self) -> bool {
        self.exact.is_empty() && !self.loopback
    }

    /// Whether this `Origin` header value may read the response.
    ///
    /// A standalone pure function rather than inline inside the closure: this is
    /// the only place in the whole module with any decision logic, it has to be
    /// unit-testable directly, and testing it through `CorsLayer` would mean
    /// incidentally testing tower-http as well.
    #[must_use]
    pub fn allows(&self, origin: &str) -> bool {
        if self.exact.contains(origin) {
            return true;
        }
        self.loopback && is_loopback_origin(origin)
    }

    /// Assemble the middleware.
    pub fn layer(&self) -> CorsLayer {
        // When nothing is allowed, **emit no CORS header at all**. This is not
        // an "empty list" that the browser has to compare against; it is the
        // absence of authorization — on which basis the browser refuses every
        // cross-origin read, matching the same-origin production shape exactly
        if self.denies_everything() {
            return CorsLayer::new();
        }

        let policy = self.clone();
        CorsLayer::new()
            // A predicate rather than a list: `loopback` has to match any port,
            // which cannot be enumerated. On this path tower-http adds
            // `Vary: Origin` automatically, so caches do not cross origins
            .allow_origin(AllowOrigin::predicate(move |origin: &HeaderValue, _| {
                origin.to_str().is_ok_and(|o| policy.allows(o))
            }))
            // Enumerated one by one rather than `Any`: `Any` saves no effort
            // here, it only hides the question "what happens once DELETE is
            // added"
            .allow_methods([Method::GET, Method::POST, Method::PATCH, Method::OPTIONS])
            .allow_headers([
                header::AUTHORIZATION,
                header::CONTENT_TYPE,
                // A player dragging the scrub bar sends Range. The symptom of
                // omitting it is "videos can only be watched from the start"
                header::RANGE,
            ])
            // Without exposing them, cross-origin JS **cannot read** these
            // response headers — and the denominator of a 206 lives in
            // Content-Range, without which ranged downloading is impossible
            .expose_headers([
                header::CONTENT_RANGE,
                header::ACCEPT_RANGES,
                header::CONTENT_LENGTH,
            ])
        // allow_credentials is deliberately absent: see the module docs
    }

    /// The line printed for ops at startup. Same purpose as the authentication
    /// line — at any moment it must be able to answer "who exactly am I
    /// allowing right now".
    #[must_use]
    pub fn status_line(&self) -> String {
        if self.denies_everything() {
            return format!(
                "跨源：不允许任何跨源请求（生产形态：Web 与 cortexd 同源）。\
                 开发机需要跨源时设 {ALLOWED_ORIGINS_ENV}={LOOPBACK_TOKEN}"
            );
        }
        let mut parts: Vec<String> = self.exact.iter().cloned().collect();
        if self.loopback {
            parts.push(format!("{LOOPBACK_TOKEN}（任意端口的本机回环）"));
        }
        format!("跨源：仅允许 {}", parts.join("、"))
    }
}

/// `http(s)://localhost:any-port` plus the two loopback IPs.
///
/// No `url` crate: one more dependency does not pay for a decision that amounts
/// to two string splits, and writing it out ourselves actually puts the rule
/// "only these three hostnames count" in plain sight.
fn is_loopback_origin(origin: &str) -> bool {
    let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    // The port is optional (`http://localhost` means 80). Split from the
    // **right**, or the colons inside an IPv6 literal shred the hostname
    let (host, port) = match rest.rsplit_once(':') {
        // With `[::1]` and no port, rsplit_once cuts inside the literal itself;
        // this test pushes that case back
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (h, Some(p)),
        _ => (rest, None),
    };
    if port.is_some_and(|p| p.parse::<u16>().is_err()) {
        return false;
    }
    LOOPBACK_HOSTS.contains(&host)
}

/// An entry in the allow-list must be an **origin**, not a URL.
///
/// The `Origin` header a browser sends is always `scheme://host[:port]` — no
/// path, no trailing slash, no query string. An entry written as
/// `https://a.example/` will never match anything, while looking entirely
/// correct — so it is caught here.
fn validate_origin(item: &str) -> cortex_core::Result<()> {
    let invalid = |why: &str| {
        cortex_core::CortexError::Config(format!(
            "{ALLOWED_ORIGINS_ENV} 里的 {item:?} 不是合法的来源：{why}。\
             来源的形状是 scheme://host[:port]，例如 https://cortex.example.com \
             或 http://localhost:5000（没有路径，也没有结尾的斜杠）"
        ))
    };

    let rest = item
        .strip_prefix("http://")
        .or_else(|| item.strip_prefix("https://"))
        .ok_or_else(|| invalid("缺少 http:// 或 https:// 前缀"))?;
    if rest.is_empty() {
        return Err(invalid("没有主机名"));
    }
    if rest.contains('/') {
        return Err(invalid("含有路径或结尾的斜杠"));
    }
    if item.contains('?') || item.contains('#') {
        return Err(invalid("含有查询串或片段"));
    }
    if item.chars().any(char::is_whitespace) {
        return Err(invalid("含有空白字符"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(raw: &str) -> CorsPolicy {
        CorsPolicy::parse(raw).expect("this list should parse")
    }

    /// The default must be "no cross-origin request allowed".
    ///
    /// This guards the entire reason this module exists: the moment the default
    /// slides back to permissive, every other assertion is still green while the
    /// memory database stands open to any web page.
    #[test]
    fn the_default_denies_every_cross_origin() {
        let p = policy("");
        assert!(
            p.denies_everything(),
            "with nothing configured, no cross-origin request may be allowed"
        );
        for o in [
            "https://evil.example",
            "http://localhost:5000",
            "http://127.0.0.1:8080",
            "null",
        ] {
            assert!(
                !p.allows(o),
                "{o} was allowed under the default policy —— production is a same-origin \
                 deployment and needs no cross-origin access at all"
            );
        }
    }

    /// `*` must be a startup failure, not a value anyone can configure.
    #[test]
    fn a_wildcard_is_refused_outright() {
        let err = CorsPolicy::parse("*").expect_err("`*` must be rejected");
        assert!(
            err.to_string().contains("ticket"),
            "the error must explain why wide-open is forbidden (tickets travel in the URL) \
             rather than merely saying \"not allowed\"; actual: {err}"
        );
        // Mixed into a list it is equally forbidden — an implementation that
        // only tests whether the whole string equals `*` leaks here
        assert!(
            CorsPolicy::parse("https://a.example,*").is_err(),
            "a `*` mixed into the list must be rejected just the same"
        );
    }

    #[test]
    fn exact_origins_match_exactly_and_nothing_else() {
        let p = policy("https://cortex.cloudcele.com");
        assert!(p.allows("https://cortex.cloudcele.com"));
        // Every entry below merely "looks a lot like" it; not one may pass
        for o in [
            "http://cortex.cloudcele.com",            // different scheme
            "https://cortex.cloudcele.com:8443",      // different port
            "https://cortex.cloudcele.com.evil.test", // suffix attack
            "https://evil.test/cortex.cloudcele.com", // carried in the path
            "https://sub.cortex.cloudcele.com",       // subdomain
        ] {
            assert!(
                !p.allows(o),
                "{o} must not pass —— origins have to match exactly"
            );
        }
    }

    /// `loopback` matches any port, but **only** those three hostnames.
    #[test]
    fn loopback_matches_any_port_but_only_real_loopback_hosts() {
        let p = policy(LOOPBACK_TOKEN);
        for o in [
            "http://localhost",
            "http://localhost:5000",
            "http://localhost:52341",
            "https://localhost:8443",
            "http://127.0.0.1:9010",
            "http://[::1]:3000",
            "http://[::1]",
        ] {
            assert!(
                p.allows(o),
                "{o} is local loopback and must work during development"
            );
        }
        for o in [
            // A prefix test leaks here — this is the one dangerous part of the
            // loopback shortcut
            "http://localhost.evil.test",
            "http://evil.test",
            "http://notlocalhost",
            "http://127.0.0.1.evil.test",
            "http://localhost@evil.test",
            "file://localhost",
            "null",
        ] {
            assert!(
                !p.allows(o),
                "{o} is not local loopback and must never pass merely for looking like it"
            );
        }
    }

    #[test]
    fn loopback_and_exact_origins_compose() {
        let p = policy("loopback, https://staging.example");
        assert!(p.allows("http://localhost:1234"));
        assert!(p.allows("https://staging.example"));
        assert!(!p.allows("https://evil.example"));
        assert!(!p.denies_everything());
    }

    /// A malformed origin must fail on the spot, rather than becoming a dead
    /// rule that never matches anything.
    #[test]
    fn a_malformed_origin_fails_loudly() {
        for bad in [
            "cortex.example.com",     // missing scheme
            "https://a.example/",     // trailing slash
            "https://a.example/path", // path
            "https://",               // no hostname
            "https://a.example?x=1",  // query string
            "https://a b.example",    // whitespace
        ] {
            assert!(
                CorsPolicy::parse(bad).is_err(),
                "{bad:?} is a malformed origin and must error immediately —— dropped silently, \
                 \"configured but not taking effect\" only drives people to widen the \
                 configuration"
            );
        }
    }

    /// The status line must always answer "who exactly am I allowing right now".
    #[test]
    fn the_status_line_always_answers_who_is_allowed() {
        assert!(policy("").status_line().contains("不允许任何跨源"));
        let line = policy("https://a.example,loopback").status_line();
        assert!(line.contains("https://a.example"), "actual: {line}");
        assert!(line.contains(LOOPBACK_TOKEN), "actual: {line}");
    }

    // ───────────── What headers actually go out once assembled ─────────────
    //
    // The assertions above test the decision itself. This group tests the
    // **wiring**: `layer()` could perfectly well decide correctly and still
    // configure `AllowOrigin` wrongly, and that kind of mistake is invisible to
    // a pure-function test.

    /// Send a request carrying `Origin` and read back
    /// `Access-Control-Allow-Origin`.
    async fn allow_origin_header(p: &CorsPolicy, origin: &str) -> Option<String> {
        use axum::{Router, routing::get};
        use tower::ServiceExt as _;

        let app = Router::new()
            .route("/x", get(|| async { "ok" }))
            .layer(p.layer());
        let req = axum::http::Request::builder()
            .uri("/x")
            .header(header::ORIGIN, origin)
            .body(axum::body::Body::empty())
            .expect("building the request must not fail");
        let resp = app.oneshot(req).await.expect("Infallible");
        resp.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .map(|v| v.to_str().expect("the header should be ASCII").to_string())
    }

    #[tokio::test]
    async fn the_default_layer_authorises_nobody() {
        let p = policy("");
        for o in ["https://evil.example", "http://localhost:5000"] {
            assert_eq!(
                allow_origin_header(&p, o).await,
                None,
                "the default policy must emit no Access-Control-Allow-Origin at all (origin {o})"
            );
        }
    }

    /// An allowed origin must be **echoed back verbatim**, and never as `*`.
    ///
    /// Echoing the concrete origin instead of `*` is not fastidiousness: `*`
    /// would let any page read the response, and `?ticket=` is a credential that
    /// travels in the URL (see the module docs).
    #[tokio::test]
    async fn an_allowed_origin_is_echoed_verbatim_never_a_wildcard() {
        let p = policy("loopback,https://staging.example");
        for o in ["http://localhost:52341", "https://staging.example"] {
            let got = allow_origin_header(&p, o).await;
            assert_eq!(
                got.as_deref(),
                Some(o),
                "{o} should be echoed back verbatim"
            );
        }
        for o in ["https://evil.example", "http://localhost.evil.test"] {
            assert_eq!(
                allow_origin_header(&p, o).await,
                None,
                "{o} is not in the allow-list and must not receive an authorization header"
            );
        }
    }
}
