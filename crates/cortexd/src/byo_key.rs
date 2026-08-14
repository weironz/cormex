//! 自带 API key —— 用自己的 key，不占配额。
//!
//! # 这一段是来还债的
//!
//! 配额超限的消息里写着「可以在设置里填自己的 API key」，而那个入口
//! **不存在**。一句已经发到用户眼前的空头支票，比没有这个功能更糟：
//! 它让一个正被拦住的人去翻一个根本没有的设置项。
//!
//! # 加密
//!
//! 密钥来自部署的 `CORTEX_SECRET_KEY`（32 字节，hex），**不进数据库**。
//! 于是一份被拖走的备份里没有可用的 key —— 这是加密存储的全部意义，
//! 也是为什么没配这个变量时端点直接 501 而不是明文存下去：
//! 一个以为自己被加密保护着的人，比一个知道没有保护的人更危险。
//!
//! 用 AES-256-GCM（`ring`）。nonce 每次现随机，与密文拼在一起 ——
//! GCM 重用 nonce 会同时毁掉机密性与完整性，而「每次现随机」是这里
//! 唯一不需要额外状态的做法。

use axum::{Json, extract::State};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use serde::{Deserialize, Serialize};

use crate::routes::ApiError;
use crate::state::AppState;

/// 部署的主密钥。hex 编码的 32 字节。
const SECRET_KEY_ENV: &str = "CORTEX_SECRET_KEY";

/// 取主密钥。
///
/// # Errors
/// 没配，或者不是 32 字节的 hex。**不生成一把临时的** ——
/// 那会让重启之后所有已存的 key 变成解不开的字节，而症状是
/// 「昨天还好好的，今天全部调用失败」。
fn kek() -> Result<LessSafeKey, ApiError> {
    let raw = std::env::var(SECRET_KEY_ENV).map_err(|_| {
        ApiError::unsupported(format!(
            "这个部署没有配 {SECRET_KEY_ENV}，不能保存 API key。\
             管理员可以用 `openssl rand -hex 32` 生成一个填进 .env"
        ))
    })?;
    let bytes = hex::decode(raw.trim())
        .map_err(|_| ApiError::internal(format!("{SECRET_KEY_ENV} 不是合法的 hex")))?;
    let unbound = UnboundKey::new(&AES_256_GCM, &bytes)
        .map_err(|_| ApiError::internal(format!("{SECRET_KEY_ENV} 必须是 32 字节（64 位 hex）")))?;
    Ok(LessSafeKey::new(unbound))
}

/// 加密：`nonce(12) || 密文`。
fn seal(plain: &str) -> Result<Vec<u8>, ApiError> {
    seal_with(&kek()?, plain)
}

/// 同上，但密钥由调用方给。
///
/// 拆出来是为了**测试不必碰环境变量**：`CORTEX_SECRET_KEY` 是进程全局的，
/// 而 cargo 默认并行跑测试 —— 四条各自 set_var 的测试会互相把对方的密钥
/// 换掉，表现是随机失败。实测撞到过。
fn seal_with(key: &LessSafeKey, plain: &str) -> Result<Vec<u8>, ApiError> {
    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce_bytes)
        .map_err(|_| ApiError::internal("内核熵源不可用，拒绝用可预测的 nonce 加密"))?;
    let mut buf = plain.as_bytes().to_vec();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce_bytes),
        Aad::empty(),
        &mut buf,
    )
    .map_err(|_| ApiError::internal("加密失败"))?;
    let mut out = nonce_bytes.to_vec();
    out.extend_from_slice(&buf);
    Ok(out)
}

/// 解密。
fn open(blob: &[u8]) -> Result<String, ApiError> {
    open_with(&kek()?, blob)
}

/// 同上，但密钥由调用方给。理由见 [`seal_with`]。
fn open_with(key: &LessSafeKey, blob: &[u8]) -> Result<String, ApiError> {
    if blob.len() <= NONCE_LEN {
        return Err(ApiError::internal("密文太短，库里那一行是坏的"));
    }
    let (nonce_bytes, rest) = blob.split_at(NONCE_LEN);
    let nonce: [u8; NONCE_LEN] = nonce_bytes.try_into().expect("刚刚按 NONCE_LEN 切的");
    let mut buf = rest.to_vec();
    let plain = key
        .open_in_place(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut buf)
        .map_err(|_| {
            // 最可能的原因是主密钥换了。说清楚，否则这条错误看起来像库坏了
            ApiError::internal(format!(
                "解不开已存的 API key —— 多半是 {SECRET_KEY_ENV} 变了。\
                 重新填一次即可"
            ))
        })?;
    String::from_utf8(plain.to_vec()).map_err(|_| ApiError::internal("解出来的不是合法 UTF-8"))
}

/// 存着的那把 key（已解密）。
pub struct OwnKey {
    pub provider: String,
    pub api_key: String,
    /// 自己的端点。`None` = 用内置定义里那个（官方）。
    ///
    /// 一个人「自己的 key」很少是官方那把 —— 更常见的是公司网关、
    /// one-api / LiteLLM 中转、某个更便宜的兼容服务。存了 key 却不能存端点，
    /// 那把 key 照样打不到他自己的地方
    pub base_url: Option<String>,
}

#[derive(Deserialize)]
pub struct SetKeyRequest {
    /// 供应商 id，与 `cortex-llm` 的 provider 名一致
    pub provider: String,
    pub api_key: String,
    /// 可选：自己的端点。留空就用内置定义里那个
    #[serde(default)]
    pub base_url: Option<String>,
}

/// 下发给界面的形状 —— **只有尾巴**，永远不回明文。
///
/// 回明文会让这把 key 出现在浏览器缓存、日志、以及任何一次
/// 「把请求粘给别人看」里。用户要换 key 时重新填一遍即可。
#[derive(Serialize)]
pub struct KeyStatus {
    pub configured: bool,
    pub provider: Option<String>,
    /// 明文 key 的后 4 位，用来认出「填的是哪一把」
    pub key_tail: Option<String>,
    /// 填的自建端点。没填就是 `None`（走官方）
    pub base_url: Option<String>,
    pub updated_at: Option<String>,
    /// 这个部署支持保存自带 key 吗（没配主密钥就是 false）
    pub supported: bool,
}

impl AppState {
    /// 这个租户存着的自带 key。没有就是 `None`。
    ///
    /// **失败一律当作「没有」并记日志**：自带 key 是一条优化路径，
    /// 读不出来时回落到服务端那把 key 是安全的（只是要占配额），
    /// 而让整次对话失败不是。
    pub async fn own_key(&self, tenant: &crate::request_tenant::Tenant) -> Option<OwnKey> {
        let store = tenant.store().ok()?;
        let row = sqlx::query_as::<_, (String, Option<Vec<u8>>, bool, Option<String>)>(
            "SELECT provider, ciphertext, deleted, base_url
               FROM llm_keys ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_optional(store.pool())
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "读不出自带 key，这一轮走服务端的 key");
            None
        })?;

        let (provider, ciphertext, deleted, base_url) = row;
        if deleted {
            return None;
        }
        match open(&ciphertext?) {
            Ok(api_key) => Some(OwnKey {
                provider,
                api_key,
                base_url,
            }),
            Err(e) => {
                tracing::warn!(error = ?e, "自带 key 解不开，这一轮走服务端的 key");
                None
            }
        }
    }
}

/// `GET /settings/llm-key` —— 填没填过、填的是哪一把。
///
/// # Errors
/// 查不动库。
pub async fn get(
    State(st): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<KeyStatus>, ApiError> {
    let supported = kek().is_ok();
    let tenant = st.tenant(&headers).await?;
    let Ok(store) = tenant.store() else {
        return Ok(Json(KeyStatus {
            configured: false,
            provider: None,
            key_tail: None,
            base_url: None,
            updated_at: None,
            supported,
        }));
    };

    let row = sqlx::query_as::<
        _,
        (
            String,
            Option<String>,
            bool,
            chrono::DateTime<chrono::Utc>,
            Option<String>,
        ),
    >(
        "SELECT provider, key_tail, deleted, created_at, base_url
           FROM llm_keys ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(store.pool())
    .await
    .map_err(|e| ApiError::internal(format!("查不出自带 key 的状态：{e}")))?;

    Ok(Json(match row {
        Some((provider, key_tail, false, at, base_url)) => KeyStatus {
            configured: true,
            provider: Some(provider),
            key_tail,
            base_url,
            updated_at: Some(at.to_rfc3339()),
            supported,
        },
        _ => KeyStatus {
            configured: false,
            provider: None,
            key_tail: None,
            base_url: None,
            updated_at: None,
            supported,
        },
    }))
}

/// `PUT /settings/llm-key` —— 存一把自己的 key。
///
/// # Errors
/// 部署没配主密钥（501）、供应商名不认识、或者写不进去。
pub async fn put(
    State(st): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<SetKeyRequest>,
) -> Result<Json<KeyStatus>, ApiError> {
    let api_key = req.api_key.trim();
    if api_key.is_empty() {
        return Err(ApiError::bad_request("API key 不能为空"));
    }
    // 供应商名当场校验。存下一个拼错的名字，症状要等到下一次对话
    // 才出现，而那时用户已经不记得自己填了什么
    if !cortex_llm::provider::available()
        .iter()
        .any(|a| a == &req.provider)
    {
        return Err(ApiError::bad_request(format!(
            "不认识的供应商 {}，可选：{}",
            req.provider,
            cortex_llm::provider::available().join(" / ")
        )));
    }

    let ciphertext = seal(api_key)?;
    let tail: String = api_key
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let tenant = st.tenant(&headers).await?;
    let store = tenant
        .store()
        .map_err(|e| ApiError::unsupported(format!("这个部署存不了自带 key：{e}")))?;

    sqlx::query(
        "INSERT INTO llm_keys (id, provider, ciphertext, key_tail, deleted, base_url)
         VALUES ($1, $2, $3, $4, false, $5)",
    )
    .bind(cortex_core::Id::new().to_string())
    .bind(&req.provider)
    .bind(&ciphertext)
    .bind(&tail)
    .bind(
        req.base_url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty()),
    )
    .execute(store.pool())
    .await
    .map_err(|e| ApiError::internal(format!("存不进去：{e}")))?;

    tracing::info!(provider = %req.provider, "用户存了自带 API key");
    get(State(st), headers).await
}

/// `DELETE /settings/llm-key` —— 不再用自带 key。
///
/// 追加一行墓碑而不是删那一行：与全库 append-only 一致，
/// 而且「什么时候撤下的」在排查时是有用的。
///
/// # Errors
/// 写不进去。
pub async fn delete(
    State(st): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<KeyStatus>, ApiError> {
    let tenant = st.tenant(&headers).await?;
    let store = tenant
        .store()
        .map_err(|e| ApiError::unsupported(e.to_string()))?;

    sqlx::query(
        "INSERT INTO llm_keys (id, provider, ciphertext, key_tail, deleted)
         VALUES ($1, '', NULL, NULL, true)",
    )
    .bind(cortex_core::Id::new().to_string())
    .execute(store.pool())
    .await
    .map_err(|e| ApiError::internal(format!("撤不掉：{e}")))?;

    tracing::info!("用户撤下了自带 API key");
    get(State(st), headers).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一把测试密钥。**不碰环境变量** —— 见 [`seal_with`] 的注释。
    fn test_key(byte: u8) -> LessSafeKey {
        LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &[byte; 32]).expect("32 字节"))
    }

    /// 加密再解密要拿回原文。
    #[test]
    fn a_key_survives_the_round_trip() {
        let k = test_key(0x11);
        let secret = "sk-deadbeef-0123456789";
        let blob = seal_with(&k, secret).expect("加密");
        assert!(
            !blob.windows(secret.len()).any(|w| w == secret.as_bytes()),
            "密文里能直接找到明文 —— 加密根本没发生"
        );
        assert_eq!(open_with(&k, &blob).expect("解密"), secret);
    }

    /// 同一份明文两次加密**不能**产生相同密文。
    ///
    /// 相同就意味着 nonce 被复用了，而 GCM 重用 nonce 会同时毁掉机密性
    /// 与完整性 —— 这是这类实现最经典的一种错法。
    #[test]
    fn the_same_secret_never_encrypts_to_the_same_bytes() {
        let k = test_key(0x22);
        let a = seal_with(&k, "sk-same").expect("第一次");
        let b = seal_with(&k, "sk-same").expect("第二次");
        assert_ne!(a, b, "两次加密结果相同 —— nonce 被复用了");
    }

    /// 换了主密钥就解不开，而且**不能**解出一段垃圾当成 key 用。
    #[test]
    fn a_rotated_master_key_fails_instead_of_returning_garbage() {
        let blob = seal_with(&test_key(0x33), "sk-rotate-me").expect("加密");
        assert!(
            open_with(&test_key(0x44), &blob).is_err(),
            "换了密钥居然解开了 —— GCM 的完整性校验没起作用，             那意味着一段被篡改的密文也能通过"
        );
    }

    /// 被篡改过的密文必须解不开（AEAD 的完整性那一半）。
    #[test]
    fn a_tampered_ciphertext_is_rejected() {
        let k = test_key(0x55);
        let mut blob = seal_with(&k, "sk-tamper").expect("加密");
        let last = blob.len() - 1;
        blob[last] ^= 0x01;
        assert!(
            open_with(&k, &blob).is_err(),
            "改了一个 bit 还能解开 —— 那说明用的不是 AEAD，             而库里那一行是任何能写库的人都可以替换的"
        );
    }

    /// 没配主密钥时**不落明文**，直接说不支持。
    ///
    /// 这条要读环境变量，所以刻意只断言「读不出来时是错」这一件事，
    /// 不去 set_var —— 并行跑的别的测试不该被它影响。
    #[test]
    fn without_a_master_key_there_is_no_storage_at_all() {
        if std::env::var(SECRET_KEY_ENV).is_ok() {
            // 开发机的 .env 里配了。那这条测的东西在这里不成立，跳过
            return;
        }
        assert!(
            seal("sk-plain").is_err(),
            "没有主密钥时居然加密成功了 —— 那只可能是把明文存了下去，             而一个以为自己被加密保护着的人比知道没有保护的人更危险"
        );
    }
}
