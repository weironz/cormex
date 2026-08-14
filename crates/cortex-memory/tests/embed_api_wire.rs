//! `ApiEmbedder` 的**线上格式**验证 —— 起一个真的 HTTP 服务，让它真的发请求。
//!
//! # 为什么单元测试不够
//!
//! `embed_api.rs` 里的单元测试覆盖的是纯函数（拼 URL、校验维度、按 index 归位）。
//! 它们抓不到这一类：请求打到了哪个路径、body 是什么形状、鉴权头有没有带上。
//!
//! 而这正是已经出过一次事的地方：`resolve_url` 第一版用
//! `ends_with("/embeddings")` 判断整串，于是 `http://embeddings`
//! （**compose 里那个服务的默认地址**）被当成「已经是完整路径」，
//! 请求原样打到根路径拿 404 —— 症状看起来像「服务没起来」。
//! 单元测试是在补这条断言时才顺手抓到它的。
//!
//! 这个文件用 axum 起一个最小的假 embedding 服务，断言的是**协议层**：
//! 路径、方法、`Authorization`、请求体字段、以及乱序响应能不能被归位。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use cortex_memory::embed::{EMBEDDING_DIM, Embedder};
use cortex_memory::embed_api::{ApiEmbedder, ApiEmbedderConfig};
use serde_json::{Value, json};

/// 假服务记下来的那一次请求，供测试断言。
#[derive(Default, Clone)]
struct Seen {
    path: String,
    auth: Option<String>,
    body: Option<Value>,
}

type Shared = Arc<Mutex<Seen>>;

/// 造一条合法的单位向量：第 `i` 维为 1，其余为 0。
fn unit(i: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; EMBEDDING_DIM];
    v[i] = 1.0;
    v
}

async fn handler(
    State(seen): State<Shared>,
    headers: axum::http::HeaderMap,
    body: Json<Value>,
) -> Json<Value> {
    let n = body
        .get("input")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    {
        let mut g = seen.lock().expect("锁");
        g.auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        g.body = Some(body.0.clone());
    }
    // **故意倒序返回**：协议不保证顺序，而错位不会报错，
    // 只会让检索莫名其妙地差。客户端必须按 index 归位
    let data: Vec<Value> = (0..n)
        .rev()
        .map(|i| json!({ "index": i, "embedding": unit(i) }))
        .collect();
    Json(json!({ "object": "list", "data": data }))
}

/// 起服务，返回它的地址。路径刻意是标准的 `/v1/embeddings`。
async fn spawn(seen: Shared) -> SocketAddr {
    let app = Router::new()
        .route(
            "/v1/embeddings",
            post({
                let seen = seen.clone();
                move |s, h, b| {
                    seen.lock().expect("锁").path = "/v1/embeddings".into();
                    handler(s, h, b)
                }
            }),
        )
        .with_state(seen);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定端口");
    let addr = listener.local_addr().expect("取地址");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("假服务退出");
    });
    addr
}

fn cfg(endpoint: String, api_key: Option<String>) -> ApiEmbedderConfig {
    ApiEmbedderConfig {
        endpoint,
        model: "bge-m3".into(),
        api_key,
        batch: 10,
    }
}

#[tokio::test]
async fn talks_the_openai_wire_format() {
    let seen: Shared = Arc::default();
    let addr = spawn(seen.clone()).await;

    // 只给主机（没有路径）—— 客户端必须自己补出 /v1/embeddings，
    // 否则请求打到根路径拿 404。
    //
    // ⚠️ 这里**没有**覆盖「主机名字面上叫 embeddings」那个更险的 case：
    // 假服务只能绑到 127.0.0.1，而那个 bug 需要 `http://embeddings`
    // 里 `//embeddings` 的斜杠才触发。故障注入验证过：把 resolve_url
    // 改回看整串，这条测试**照样绿**，只有 embed_api.rs 里那条单元测试会红。
    // 别因为这里有一条「只给主机」的断言就以为那个坑被这里守着。
    let e =
        ApiEmbedder::new(cfg(format!("http://{addr}"), Some("sk-test-key".into()))).expect("构造");

    let texts = vec!["第一句".to_string(), "第二句".to_string()];
    let got = e.embed(&texts).await.expect("请求应当成功");

    let g = seen.lock().expect("锁").clone();
    assert_eq!(
        g.path, "/v1/embeddings",
        "只给主机时必须补全成 /v1/embeddings —— 打到根路径是 404，\
         而那个症状看起来像「服务没起来」"
    );
    assert_eq!(
        g.auth.as_deref(),
        Some("Bearer sk-test-key"),
        "配了 api_key 就必须带 Authorization 头"
    );

    let body = g.body.expect("应当记下请求体");
    assert_eq!(body["model"], "bge-m3", "model 字段要按配置发出去");
    assert_eq!(
        body["encoding_format"], "float",
        "必须显式要 float —— 不写的话有些服务默认回 base64，\
         而我们的反序列化只认数组，那会表现成一条看不懂的解析错误"
    );
    assert_eq!(
        body["input"],
        json!(["第一句", "第二句"]),
        "input 应当是原样的文本数组"
    );

    // 假服务是倒序返回的，客户端必须归位
    assert_eq!(got.len(), 2, "两条输入应当得到两条向量");
    assert_eq!(
        (got[0][0], got[0][1]),
        (1.0, 0.0),
        "第一条应当是 index=0 那条 —— 返回顺序不保证，必须按 index 归位"
    );
    assert_eq!(
        (got[1][0], got[1][1]),
        (0.0, 1.0),
        "第二条应当是 index=1 那条"
    );
}

#[tokio::test]
async fn omits_the_auth_header_when_no_key_is_configured() {
    // 自建的 TEI 不带鉴权。多发一个 Bearer 头通常无害，但「没配 key 却
    // 发了一个空 Bearer」在有些网关上会被判成鉴权失败 —— 那是一条
    // 很难往「我没配 key」这个方向想的 401
    let seen: Shared = Arc::default();
    let addr = spawn(seen.clone()).await;
    let e = ApiEmbedder::new(cfg(format!("http://{addr}/v1/embeddings"), None)).expect("构造");

    e.embed(&["x".to_string()]).await.expect("请求应当成功");

    assert_eq!(
        seen.lock().expect("锁").auth,
        None,
        "没配 api_key 时不该发 Authorization 头"
    );
}

#[tokio::test]
async fn batches_are_split_by_the_configured_size() {
    // 各家对 batch 上限差得很远（TEI 默认 32，阿里百炼 text-embedding-v3 是 10）。
    // 切分错了的表现是一条 400，而不是慢 —— 所以这条要断言的是「真的切了」
    let seen: Shared = Arc::default();
    let addr = spawn(seen.clone()).await;
    let mut c = cfg(format!("http://{addr}"), None);
    c.batch = 2;
    let e = ApiEmbedder::new(c).expect("构造");

    let texts: Vec<String> = (0..5).map(|i| format!("第{i}句")).collect();
    let got = e.embed(&texts).await.expect("请求应当成功");

    assert_eq!(got.len(), 5, "五条输入应当得到五条向量（跨三次请求拼起来）");
    // 最后一次请求只剩一条（5 = 2 + 2 + 1）
    let last = seen
        .lock()
        .expect("锁")
        .body
        .clone()
        .expect("应当记下请求体");
    assert_eq!(
        last["input"].as_array().map(Vec::len),
        Some(1),
        "batch=2 时 5 条应当切成 2+2+1，最后一次只有一条"
    );
}

#[tokio::test]
async fn a_wrong_dimension_fails_loudly_rather_than_reaching_postgres() {
    // 接错一家（OpenAI 的 text-embedding-3-small 是 1536 维）时，
    // 最坏的结果不是报错，是把问题推给 Postgres 的 VECTOR(1024) 约束，
    // 让调用方把它当成一次普通的写失败
    let app = Router::new().route(
        "/v1/embeddings",
        post(|| async {
            Json(json!({ "data": [{ "index": 0, "embedding": vec![0.1f32; 1536] }] }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定端口");
    let addr = listener.local_addr().expect("取地址");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("假服务退出") });

    let e = ApiEmbedder::new(ApiEmbedderConfig {
        endpoint: format!("http://{addr}"),
        model: "text-embedding-3-small".into(),
        api_key: None,
        batch: 10,
    })
    .expect("构造");

    let err = e
        .embed(&["x".to_string()])
        .await
        .expect_err("1536 维必须在客户端就被拒掉");
    let msg = err.to_string();
    assert!(
        msg.contains("1536") && msg.contains("1024"),
        "错误信息要同时给出实际维度与期望维度：{msg}"
    );
}

#[tokio::test]
async fn a_4xx_is_not_retried() {
    // 模型名不对 / 鉴权失败 / 请求形状不对，重试多少次都是同样的结果。
    // 重试只会把一条清楚的错误延迟三倍才让人看见
    let hits = Arc::new(Mutex::new(0usize));
    let h = hits.clone();
    let app = Router::new().route(
        "/v1/embeddings",
        post(move || {
            let h = h.clone();
            async move {
                *h.lock().expect("锁") += 1;
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "model not found" })),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定端口");
    let addr = listener.local_addr().expect("取地址");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("假服务退出") });

    let e = ApiEmbedder::new(cfg(format!("http://{addr}"), None)).expect("构造");
    let err = e
        .embed(&["x".to_string()])
        .await
        .expect_err("400 应当直接失败");

    assert_eq!(
        *hits.lock().expect("锁"),
        1,
        "4xx 只该请求一次 —— 重试它既救不回来，又把错误信息延后三倍"
    );
    assert!(
        err.to_string().contains("model not found"),
        "服务端的错误正文要带回来，否则排查只能靠猜：{err}"
    );
}
