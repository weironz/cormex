//! 跨源策略 —— 「哪个网页能读到 cortexd 的响应」。
//!
//! 在此之前这里是 [`CorsLayer::permissive`]，也就是**任意来源**都能读到
//! 任意端点的响应体。注释里写着「生产应收紧」，但那句话不会自己生效。
//!
//! # bearer token 下这为什么仍然是个问题
//!
//! 常见的辩护是「恶意页面拿不到 token，所以读不到东西」。对了一半：
//! 认证那一轮为浏览器引入了 **`?ticket=` 短命票据**（见 [`crate::auth`]），
//! 因为 `EventSource` / `WebSocket` / `<img src>` 加不了请求头。票据是要
//! **进 URL** 的东西 —— 它会进反代 access log、进浏览器历史、进 referrer。
//! 一旦某张票以任何方式落到第三方页面手里，permissive CORS 就意味着
//! 那个页面能拿它去 `fetch` 并且**读到响应**。收紧之后，同一张票在
//! 浏览器里只能被允许的来源用。
//!
//! **CORS 只是浏览器侧的控制**，这一点必须说清楚：拿到票的 `curl` 不受
//! 任何影响。它买到的是「随便一个网页不能读我的记忆库」，不是「票泄露了
//! 也没关系」。票据本身的短 TTL 才是后者的答案。
//!
//! # 生产形态：同源，因此默认一个跨源都不许
//!
//! 生产是单域名 + 路径分流（`/` → Flutter Web，`/api/*` → cortexd，
//! 反代把 `/api` 前缀剥掉）。Web 与 cortexd **同源**，浏览器根本不会发起
//! 跨源请求，也就不需要任何 CORS 头。这个同源是刻意选的。
//!
//! 于是默认值就有了唯一正确的取法：**什么都不允许**。开发机上
//! `flutter run -d chrome` 起在随机端口上，那时确实是跨源 —— 那种场合
//! 显式配一份允许列表，而不是让所有部署为了开发方便一直敞着。
//!
//! | `CORTEX_CORS_ALLOWED_ORIGINS` | 效果 |
//! |---|---|
//! | 不设 / 空 | 一个跨源都不许（生产默认） |
//! | `loopback` | 任意端口的 `http(s)://localhost` 与回环 IP（开发默认） |
//! | `https://a.example,https://b.example` | 只允许这两个来源 |
//! | `*` | **拒绝启动**，理由见 [`CorsPolicy::from_env`] |
//!
//! 逗号分隔的列表里可以混用 `loopback` 与具体来源。
//!
//! # 为什么不设 `Access-Control-Allow-Credentials`
//!
//! 本服务的凭据是 bearer token 与查询串里的票据，两者都由客户端**显式**
//! 附带，不是浏览器自动带上的 cookie。开了 credentials 只会引入 CSRF
//! 这一整类问题，而换不到任何东西。

use std::collections::BTreeSet;

use axum::http::{HeaderValue, Method, header};
use tower_http::cors::{AllowOrigin, CorsLayer};

/// 允许列表的环境变量。
pub const ALLOWED_ORIGINS_ENV: &str = "CORTEX_CORS_ALLOWED_ORIGINS";

/// 列表里表示「任意端口的本机回环」的字面量。
///
/// 存在的理由很具体：`flutter run -d chrome` 每次起在**随机端口**上，
/// 写死一个 `http://localhost:5000` 第二天就不对了，而那种失败在浏览器里
/// 表现为一个语焉不详的 CORS 报错，多数人会直接改回 permissive。
/// 给一个专门的字面量，比逼着人去写通配符要安全 —— 它匹配的东西是
/// **明确列举**的三个主机名，不是「任意来源」。
pub const LOOPBACK_TOKEN: &str = "loopback";

/// 允许列表里能出现的回环主机名。
///
/// 只认这三个字面量，不做「以 localhost 开头」这类前缀判断 ——
/// `http://localhost.evil.com` 正是靠那种判断混进来的。
const LOOPBACK_HOSTS: &[&str] = &["localhost", "127.0.0.1", "[::1]"];

/// 本部署允许哪些跨源来源读响应。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CorsPolicy {
    /// 精确匹配的来源（大小写敏感，含 scheme 与端口）
    exact: BTreeSet<String>,
    /// 任意端口的回环。见 [`LOOPBACK_TOKEN`]
    loopback: bool,
}

impl CorsPolicy {
    /// 从环境变量加载。
    ///
    /// # 为什么配错要报错而不是忽略
    ///
    /// 一条写错的来源（多了个尾斜杠、写成了 `example.com` 少了 scheme）
    /// 如果被静默丢掉，症状是「配了却不生效」—— 而这类问题在浏览器里只会
    /// 表现成一句通用的 CORS 失败，排查的人第一反应是把配置改得更宽。
    /// 当场失败让错误出现在它真正发生的地方。这与 [`crate::auth::AuthMode`]
    /// 的取舍同源。
    ///
    /// # 为什么 `*` 是错误而不是一个取值
    ///
    /// 它就是改回 permissive，只不过换了个写法。真要那么做的人应当去改
    /// 这段代码并且读一遍上面关于票据的论证，而不是在 `.env` 里敲一个星号。
    pub fn from_env() -> cortex_core::Result<Self> {
        Self::parse(
            std::env::var(ALLOWED_ORIGINS_ENV)
                .unwrap_or_default()
                .as_str(),
        )
    }

    /// 解析一份逗号分隔的允许列表。空串 = 一个跨源都不许。
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

    /// 一个跨源都不许 —— 生产形态。
    #[must_use]
    pub fn denies_everything(&self) -> bool {
        self.exact.is_empty() && !self.loopback
    }

    /// 这个 `Origin` 头的值能不能读响应。
    ///
    /// 单独一个纯函数而不是直接写在闭包里：这是整个模块唯一有判断逻辑的
    /// 地方，它必须能被直接单测，而穿过 `CorsLayer` 去测等于顺带测了
    /// tower-http。
    #[must_use]
    pub fn allows(&self, origin: &str) -> bool {
        if self.exact.contains(origin) {
            return true;
        }
        self.loopback && is_loopback_origin(origin)
    }

    /// 装配成中间件。
    pub fn layer(&self) -> CorsLayer {
        // 什么都不允许时**一个 CORS 头都不发**。这不是「空列表」那种
        // 需要浏览器去比对的东西，而是根本没有授权 —— 浏览器据此拒绝
        // 任何跨源读取，与生产的同源形态完全吻合
        if self.denies_everything() {
            return CorsLayer::new();
        }

        let policy = self.clone();
        CorsLayer::new()
            // predicate 而非 list：`loopback` 要匹配任意端口，列不出来。
            // tower-http 在这条路上会自动带 `Vary: Origin`，缓存不会串源
            .allow_origin(AllowOrigin::predicate(move |origin: &HeaderValue, _| {
                origin.to_str().is_ok_and(|o| policy.allows(o))
            }))
            // 逐个列出而不是 `Any`：`Any` 在这里没有省事，只是把「将来加了
            // DELETE 会怎样」这个问题藏起来
            .allow_methods([Method::GET, Method::POST, Method::PATCH, Method::OPTIONS])
            .allow_headers([
                header::AUTHORIZATION,
                header::CONTENT_TYPE,
                // 播放器拖进度条要发 Range。漏掉它的症状是「视频只能从头看」
                header::RANGE,
            ])
            // 不 expose 的话，跨源的 JS **读不到**这几个响应头 ——
            // 而 206 的分母就在 Content-Range 里，没有它没法做分段下载
            .expose_headers([
                header::CONTENT_RANGE,
                header::ACCEPT_RANGES,
                header::CONTENT_LENGTH,
            ])
        // 刻意没有 allow_credentials：见模块文档
    }

    /// 启动时打给运维看的那一行。与认证那一行同样的用途 ——
    /// 任何时候都必须能回答「我现在到底允许了谁」。
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

/// `http(s)://localhost:任意端口` 与两个回环 IP。
///
/// 不用 `url` crate：为一个「切两刀」的判断多一个依赖不划算，而且自己写
/// 反而能把「只认这三个主机名」这条规则摆在明处。
fn is_loopback_origin(origin: &str) -> bool {
    let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    // 端口是可选的（`http://localhost` 就是 80）。从**右边**切，
    // 否则 IPv6 字面量里的冒号会把主机名切碎
    let (host, port) = match rest.rsplit_once(':') {
        // `[::1]` 没有端口时 rsplit_once 会切在字面量内部，靠这个判断挡回去
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (h, Some(p)),
        _ => (rest, None),
    };
    if port.is_some_and(|p| p.parse::<u16>().is_err()) {
        return false;
    }
    LOOPBACK_HOSTS.contains(&host)
}

/// 允许列表里的一项必须是一个**来源**，不是一个 URL。
///
/// 浏览器发出的 `Origin` 头永远是 `scheme://host[:port]`，没有路径、
/// 没有尾斜杠、没有查询串。写成 `https://a.example/` 的那一项永远匹配不上
/// 任何东西，而它看起来完全正确 —— 所以在这里拦下来。
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
        CorsPolicy::parse(raw).expect("这份列表应当能解析")
    }

    /// 默认必须是「一个跨源都不许」。
    ///
    /// 这条守的是本模块存在的全部理由：默认值一旦滑回宽松，
    /// 其余所有断言都还是绿的，而记忆库对任意网页敞开。
    #[test]
    fn the_default_denies_every_cross_origin() {
        let p = policy("");
        assert!(p.denies_everything(), "不配置时必须一个跨源都不许");
        for o in [
            "https://evil.example",
            "http://localhost:5000",
            "http://127.0.0.1:8080",
            "null",
        ] {
            assert!(
                !p.allows(o),
                "默认策略下 {o} 竟然被允许 —— 生产是同源部署，不需要任何跨源"
            );
        }
    }

    /// `*` 必须是启动失败，而不是一个能配出来的取值。
    #[test]
    fn a_wildcard_is_refused_outright() {
        let err = CorsPolicy::parse("*").expect_err("`*` 必须被拒绝");
        assert!(
            err.to_string().contains("ticket"),
            "报错必须说清为什么不能全开（票据会进 URL），而不是只说「不允许」，实际：{err}"
        );
        // 混在列表里也不行 —— 只判整串等于 `*` 的实现会在这里漏
        assert!(
            CorsPolicy::parse("https://a.example,*").is_err(),
            "列表里混进一个 `*` 同样必须被拒绝"
        );
    }

    #[test]
    fn exact_origins_match_exactly_and_nothing_else() {
        let p = policy("https://cortex.cloudcele.com");
        assert!(p.allows("https://cortex.cloudcele.com"));
        // 下面每一条都是「看起来很像」的东西，一条都不能放行
        for o in [
            "http://cortex.cloudcele.com",            // scheme 不同
            "https://cortex.cloudcele.com:8443",      // 端口不同
            "https://cortex.cloudcele.com.evil.test", // 后缀攻击
            "https://evil.test/cortex.cloudcele.com", // 路径里带
            "https://sub.cortex.cloudcele.com",       // 子域
        ] {
            assert!(!p.allows(o), "{o} 不该被放行 —— 来源必须是精确匹配");
        }
    }

    /// `loopback` 匹配任意端口，但**只**匹配那三个主机名。
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
            assert!(p.allows(o), "{o} 是本机回环，开发时必须能用");
        }
        for o in [
            // 前缀判断会在这里漏 —— 这是 loopback 这条捷径唯一危险的地方
            "http://localhost.evil.test",
            "http://evil.test",
            "http://notlocalhost",
            "http://127.0.0.1.evil.test",
            "http://localhost@evil.test",
            "file://localhost",
            "null",
        ] {
            assert!(!p.allows(o), "{o} 不是本机回环，绝不能因为长得像就放行");
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

    /// 写错的来源必须当场失败，而不是变成一条永远匹配不上的死规则。
    #[test]
    fn a_malformed_origin_fails_loudly() {
        for bad in [
            "cortex.example.com",     // 少 scheme
            "https://a.example/",     // 尾斜杠
            "https://a.example/path", // 路径
            "https://",               // 没主机名
            "https://a.example?x=1",  // 查询串
            "https://a b.example",    // 空白
        ] {
            assert!(
                CorsPolicy::parse(bad).is_err(),
                "{bad:?} 是一条写错的来源，必须当场报错 —— \
                 静默丢掉的话，配了却不生效只会让人把配置改得更宽"
            );
        }
    }

    /// 状态行必须能回答「我现在到底允许了谁」。
    #[test]
    fn the_status_line_always_answers_who_is_allowed() {
        assert!(policy("").status_line().contains("不允许任何跨源"));
        let line = policy("https://a.example,loopback").status_line();
        assert!(line.contains("https://a.example"), "实际：{line}");
        assert!(line.contains(LOOPBACK_TOKEN), "实际：{line}");
    }

    // ───────────── 装配之后真的发了什么头 ─────────────
    //
    // 上面那些断言测的是判断本身。这一组测的是**接线**：`layer()` 完全可能
    // 判断得对却把 `AllowOrigin` 配错，那种错在纯函数测试里看不见。

    /// 发一个带 `Origin` 的请求，取回 `Access-Control-Allow-Origin`。
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
            .expect("构造请求不该失败");
        let resp = app.oneshot(req).await.expect("Infallible");
        resp.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .map(|v| v.to_str().expect("头应当是 ASCII").to_string())
    }

    #[tokio::test]
    async fn the_default_layer_authorises_nobody() {
        let p = policy("");
        for o in ["https://evil.example", "http://localhost:5000"] {
            assert_eq!(
                allow_origin_header(&p, o).await,
                None,
                "默认策略下不该发出任何 Access-Control-Allow-Origin（来源 {o}）"
            );
        }
    }

    /// 允许的来源要被**原样回显**，而且永远不能是 `*`。
    ///
    /// 回显具体来源而不是 `*` 不只是洁癖：`*` 会让任意页面都能读响应，
    /// 而 `?ticket=` 是会进 URL 的凭据（见模块文档）。
    #[tokio::test]
    async fn an_allowed_origin_is_echoed_verbatim_never_a_wildcard() {
        let p = policy("loopback,https://staging.example");
        for o in ["http://localhost:52341", "https://staging.example"] {
            let got = allow_origin_header(&p, o).await;
            assert_eq!(got.as_deref(), Some(o), "{o} 应当被原样回显");
        }
        for o in ["https://evil.example", "http://localhost.evil.test"] {
            assert_eq!(
                allow_origin_header(&p, o).await,
                None,
                "{o} 不在允许列表里，不该拿到授权头"
            );
        }
    }
}
