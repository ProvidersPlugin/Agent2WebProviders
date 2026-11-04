//! Tabbit 上游 HTTP：签名、sign-key、会话、目录/额度/签到请求。
//!
//! ── 上游长什么样（依据：`tabbit2api-panel` 2026-11 的 Go 实现与实测）──
//! - 认证：Cookie 里的 `token`（HttpOnly JWT）+ `user_id`；
//! - 写请求：`x-timestamp` / `x-signature` / `x-nonce` 三件套（见 `sign.rs`）；
//! - 对话：v3 房间协议 —— 先 `POST /api/v3/chat/rooms/{id}/join` 订阅 SSE，
//!   再 `POST .../runs` 投递消息；join 带签名，runs 不带；
//! - 会话：`POST /panel/session` 拿 `chat_session_id`（房间 id），30 分钟复用；
//! - 目录：`GET /proxy/v1/model_config/models?a=0&scene=chat`；
//! - 额度：`GET /api/commerce/quota/v1/usage?user_id=…&timezone=…`（百分比制）；
//! - 签到：`GET/POST /api/commerce/activity/v1/sign-in[/status]`。
//!
//! ── 为什么自己建客户端 ──────────────────────────────────────
//! 通用 `crate::http::client` 带 20 秒总超时，会把长回答的 SSE 中途掐断；
//! 这里只设连接超时，控制面请求各自带 `timeout`。

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{PluginCtx, StoreAccount, StoreProxy};
use serde_json::{json, Value};

use crate::errors::GatewayError;

use super::config::{
    Realm, DEFAULT_CLIENT_VERSION, SESSION_TTL_MS, SIGN_KEY_TTL_MS,
};
use super::sign;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(20);
/// 投递一次对话的预算：上游排队时首字节可能慢，给 60 秒（SSE 本身不设总超时）。
const RUN_TIMEOUT: Duration = Duration::from_secs(60);
pub(super) const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";
pub(super) const SEC_CH_UA: &str = r#""Tabbit Browser";v="149", "Chromium";v="149", "Not)A;Brand";v="24""#;
const WANTED_COOKIES: [&str; 5] = ["token", "user_id", "managed", "NEXT_LOCALE", "SAPISID"];

// ─── 凭证 ─────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Credentials {
    /// Cookie 里的 JWT（也是 `Authorization: Bearer` 的值）。
    pub token: String,
    /// 过滤后的 Cookie 请求头。
    pub cookie: String,
    /// 上游 user_id（额度接口必填）。
    pub uid: String,
    /// 是否发送「已设为默认浏览器」标记（解锁 Premium 模型）。
    pub unlock_pro: bool,
    /// 客户端版本号（`x-req-ctx` 的明文）。
    pub client_version: String,
}

impl Credentials {
    pub fn from_secret(secret: &Value) -> Result<Self, GatewayError> {
        let token = crate::store::pick(secret, &["accessToken", "access_token", "token"]);
        let raw_cookie = crate::store::pick(secret, &["cookie"]);
        let mut uid = crate::store::pick(secret, &["uid", "user_id", "userId"]);
        let unlock_pro = secret
            .get("unlockPro")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let client_version = {
            let value = crate::store::pick(secret, &["clientVersion"]);
            if value.is_empty() {
                DEFAULT_CLIENT_VERSION.to_string()
            } else {
                value
            }
        };

        let extracted = extract_cookie(&raw_cookie);
        let map = cookie_map(&extracted);
        let mut token = token;
        if token.is_empty() {
            token = map.get("token").cloned().unwrap_or_default();
        }
        if uid.is_empty() {
            uid = map.get("user_id").cloned().unwrap_or_default();
        }
        if uid.is_empty() && !token.is_empty() {
            uid = crate::crypto::decode_jwt_claims(&token)
                .as_ref()
                .and_then(|claims| {
                    ["user_id", "uid", "sub", "userId"]
                        .iter()
                        .find_map(|key| claims.get(*key).and_then(Value::as_str))
                })
                .unwrap_or("")
                .to_string();
        }
        if token.is_empty() {
            return Err(GatewayError::with_status(
                400,
                "Tabbit 账号缺少登录令牌（Cookie 里要有 token=…）",
            ));
        }
        let cookie = if raw_cookie.trim().is_empty() {
            synthetic_cookie(&token, &uid)
        } else if map.contains_key("token") {
            cookie_header(&map)
        } else {
            // 用户只粘了额外 Cookie、令牌在 `token` 字段里：补进去。
            let mut merged = map;
            merged.insert("token".to_string(), token.clone());
            cookie_header(&merged)
        };
        if cookie.is_empty() {
            return Err(GatewayError::with_status(
                400,
                "Tabbit Cookie 里没有可用的登录项（需要 token=…）",
            ));
        }
        if cookie.len() > 32768 || cookie.contains(['\r', '\n']) {
            return Err(GatewayError::with_status(400, "Tabbit Cookie 格式无效"));
        }
        Ok(Self { token, cookie, uid, unlock_pro, client_version })
    }

    fn jwt(&self) -> &str {
        &self.token
    }
}

/// 从「随手复制的一段东西」里挖出 Cookie 串：DevTools 的 Copy as cURL、
/// `Cookie:` 单行、`-b '…'`、或直接粘 cookie 都能认。
pub fn extract_cookie(raw: &str) -> String {
    let text = raw.trim();
    if text.is_empty() {
        return String::new();
    }
    let lower = text.to_ascii_lowercase();
    if let Some(index) = lower.find("cookie:") {
        let rest = &text[index + "cookie:".len()..];
        let end = rest
            .find(|c| matches!(c, '"' | '\'' | '\r' | '\n'))
            .unwrap_or(rest.len());
        let value = rest[..end].trim();
        if value.contains('=') {
            return value.to_string();
        }
    }
    // `-b 'cookie'` / `-b cookie`
    let marker = lower.find(" -b ").map(|index| index + 4).or_else(|| {
        if lower.starts_with("-b ") {
            Some(3)
        } else {
            None
        }
    });
    if let Some(start) = marker {
        let rest = text[start..].trim_start();
        let rest = rest.strip_prefix("-b ").unwrap_or(rest).trim_start();
        let value = if let Some(quote) = rest.chars().next().filter(|c| *c == '\'' || *c == '"') {
            let body = &rest[quote.len_utf8()..];
            body.split(quote).next().unwrap_or(body)
        } else {
            rest.split(|c: char| c.is_whitespace())
                .next()
                .unwrap_or(rest)
        };
        let value = value.trim();
        if value.contains('=') {
            return value.to_string();
        }
    }
    text.to_string()
}

fn cookie_map(cookie: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for part in cookie.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let mut pieces = part.splitn(2, '=');
        let name = pieces.next().unwrap_or("").trim();
        let value = pieces.next().unwrap_or("").trim();
        if !name.is_empty() && !value.is_empty() {
            map.insert(name.to_string(), value.to_string());
        }
    }
    map
}

fn cookie_header(map: &BTreeMap<String, String>) -> String {
    let mut parts = Vec::new();
    for name in WANTED_COOKIES {
        if let Some(value) = map.get(name).filter(|value| !value.is_empty()) {
            parts.push(format!("{name}={value}"));
        }
    }
    parts.join("; ")
}

fn synthetic_cookie(token: &str, uid: &str) -> String {
    let mut parts = vec![format!("token={token}")];
    if !uid.is_empty() {
        parts.push(format!("user_id={uid}"));
        parts.push(format!("SAPISID={uid}"));
    }
    parts.push("managed=tab_browser".to_string());
    parts.push("NEXT_LOCALE=zh".to_string());
    parts.join("; ")
}

/// 把一段文本归一成账号：JSON、cookie 串、cURL、裸 JWT 都认。
pub fn normalize_input_text(text: &str) -> (String, String, String, bool, String) {
    let trimmed = text.trim();
    let mut token;
    let cookie;
    let mut uid = String::new();
    let mut unlock_pro = true;
    let mut client_version = String::new();
    if let Some(value) = serde_json::from_str::<Value>(trimmed).ok() {
        if value.is_object() {
            token = crate::store::pick(&value, &["accessToken", "access_token", "token"]);
            cookie = crate::store::pick(&value, &["cookie"]);
            uid = crate::store::pick(&value, &["uid", "user_id", "userId"]);
            unlock_pro = value.get("unlockPro").and_then(Value::as_bool).unwrap_or(true);
            client_version = crate::store::pick(&value, &["clientVersion"]);
            if token.is_empty() {
                token = cookie_token(&cookie);
            }
            return (token, cookie, uid, unlock_pro, client_version);
        }
    }
    cookie = extract_cookie(trimmed);
    token = cookie_token(&cookie);
    if token.is_empty() && !trimmed.contains('=') {
        token = trimmed.trim_start_matches("Bearer ").trim().to_string();
    }
    (token, cookie, uid, unlock_pro, client_version)
}

pub fn cookie_token(cookie: &str) -> String {
    cookie_map(cookie).get("token").cloned().unwrap_or_default()
}

// ─── HTTP 客户端 ──────────────────────────────────────────────

pub fn client_for(proxy: Option<&proto::WireProxy>, gateway: &str) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15));
    let url = proxy
        .map(proto::WireProxy::to_url)
        .unwrap_or_else(|| gateway.to_string());
    if !url.trim().is_empty() && url != "direct" {
        builder = builder.proxy(
            reqwest::Proxy::all(&url).map_err(|_| "Tabbit 出网代理无效".to_string())?,
        );
    }
    builder
        .build()
        .map_err(|_| "Tabbit 网络客户端创建失败".to_string())
}

/// 账号记录里的代理 → 客户端。控制面（目录/额度/签到）没有 `req.proxy`，
/// 只能读账号记录上的解析结果。
pub fn record_client(record: &StoreAccount, gateway: &str) -> Result<reqwest::Client, String> {
    match &record.proxy {
        Some(StoreProxy::Resolved { proxy }) => client_for(Some(proxy), gateway),
        Some(StoreProxy::Static { config }) if config["source"] == "direct" => {
            client_for(None, "direct")
        }
        Some(StoreProxy::Static { config }) => {
            let proxy: proto::WireProxy = serde_json::from_value(config.clone())
                .map_err(|_| "Tabbit 账号代理格式无效".to_string())?;
            client_for(Some(&proxy), gateway)
        }
        Some(StoreProxy::Failed { .. }) => {
            Err("Tabbit 账号代理不可用，请重新选择".to_string())
        }
        Some(StoreProxy::Unset) | None => client_for(None, gateway),
    }
}

fn base_headers(creds: &Credentials, realm: Realm, referer_path: &str) -> Vec<(String, String)> {
    let base = realm.base();
    vec![
        ("User-Agent".to_string(), USER_AGENT.to_string()),
        ("sec-ch-ua".to_string(), SEC_CH_UA.to_string()),
        ("sec-ch-ua-platform".to_string(), "\"Windows\"".to_string()),
        ("sec-ch-ua-mobile".to_string(), "?0".to_string()),
        (
            "x-chrome-id-consistency-request".to_string(),
            sign::chrome_consistency(&creds.uid),
        ),
        ("referer".to_string(), format!("{base}{referer_path}")),
        ("Origin".to_string(), base.to_string()),
        (
            "x-req-ctx".to_string(),
            sign::base64_text(&creds.client_version),
        ),
        (
            "unique-uuid".to_string(),
            sign::unique_uuid(creds.unlock_pro, crate::http::now_ms()),
        ),
    ]
}

// ─── sign-key 缓存 ───────────────────────────────────────────

fn sign_keys() -> &'static Mutex<BTreeMap<String, (String, i64)>> {
    static CACHE: OnceLock<Mutex<BTreeMap<String, (String, i64)>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

pub async fn sign_key(
    creds: &Credentials,
    realm: Realm,
    client: &reqwest::Client,
) -> String {
    let key = realm.id().to_string();
    let now = crate::http::now_ms();
    if let Ok(guard) = sign_keys().lock() {
        if let Some((cached, at)) = guard.get(&key) {
            if now - *at < SIGN_KEY_TTL_MS {
                return cached.clone();
            }
        }
    }
    match fetch_sign_key(creds, realm, client).await {
        Ok(fresh) => {
            if let Ok(mut guard) = sign_keys().lock() {
                guard.insert(key, (fresh.clone(), now));
            }
            fresh
        }
        Err(_) => {
            if let Ok(guard) = sign_keys().lock() {
                if let Some((cached, _)) = guard.get(&realm.id().to_string()) {
                    return cached.clone();
                }
            }
            realm.fallback_sign_key().to_string()
        }
    }
}

async fn fetch_sign_key(
    creds: &Credentials,
    realm: Realm,
    client: &reqwest::Client,
) -> Result<String, String> {
    let mut request = client
        .get(format!("{}/chat/sign-key", realm.base()))
        .timeout(CONTROL_TIMEOUT)
        .header("Accept", "text/plain")
        .header("Cookie", &creds.cookie);
    for (name, value) in base_headers(creds, realm, "/") {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request.send().await.map_err(|_| "Tabbit 签名 key 请求失败")?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let key = text.trim();
    if status != 200
        || key.is_empty()
        || key.len() > 512
        || key.contains(['<', '>', ' ', '\n', '\r', '{', '}', '"'])
    {
        return Err(format!("Tabbit 签名 key 无效（HTTP {status}）"));
    }
    Ok(key.to_string())
}

async fn refresh_sign_key(creds: &Credentials, realm: Realm, client: &reqwest::Client) {
    if let Ok(fresh) = fetch_sign_key(creds, realm, client).await {
        if let Ok(mut guard) = sign_keys().lock() {
            guard.insert(realm.id().to_string(), (fresh, crate::http::now_ms()));
        }
    }
}

// ─── 请求 ────────────────────────────────────────────────────

fn signed_headers(
    creds: &Credentials,
    realm: Realm,
    referer_path: &str,
    body: &str,
    sign_key: &str,
    with_authorization: bool,
) -> Vec<(String, String)> {
    let mut headers = base_headers(creds, realm, referer_path);
    headers.push(("trace-id".to_string(), sign::random_uuid()));
    headers.push(("Cookie".to_string(), creds.cookie.clone()));
    if with_authorization && !creds.jwt().is_empty() {
        headers.push(("Authorization".to_string(), format!("Bearer {}", creds.jwt())));
    }
    for (name, value) in sign::signature_headers(sign_key, body, crate::http::now_ms()) {
        headers.push((name, value));
    }
    headers
}

async fn send(
    client: &reqwest::Client,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&str>,
    timeout: Duration,
) -> Result<(u16, Vec<u8>), GatewayError> {
    let mut request = match method {
        "POST" => client.post(url),
        "PUT" => client.put(url),
        "DELETE" => client.delete(url),
        _ => client.get(url),
    }
    .timeout(timeout);
    for (name, value) in headers {
        request = request.header(name.as_str(), value.as_str());
    }
    if let Some(body) = body {
        request = request.body(body.to_string());
    }
    let response = request
        .send()
        .await
        .map_err(|error| GatewayError::with_status(502, format!("Tabbit 请求失败：{error}")))?;
    let status = response.status().as_u16();
    let bytes = response
        .bytes()
        .await
        .map_err(|_| GatewayError::with_status(502, "读取 Tabbit 响应失败"))?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(GatewayError::with_status(502, "Tabbit 响应超过大小上限"));
    }
    Ok((status, bytes.to_vec()))
}

fn parse_json(bytes: &[u8]) -> Value {
    if bytes.is_empty() {
        return Value::Null;
    }
    match serde_json::from_slice::<Value>(bytes) {
        Ok(value) => value,
        Err(_) => Value::Null,
    }
}

pub fn error_text(value: &Value) -> String {
    let candidates = ["message", "msg", "detail", "error_message", "displayTips", "error"];
    for key in candidates {
        if let Some(text) = value.get(key).and_then(Value::as_str) {
            if !text.trim().is_empty() {
                return text.trim().to_string();
            }
        }
        if let Some(inner) = value.get(key) {
            if let Some(text) = inner.get("message").and_then(Value::as_str) {
                if !text.trim().is_empty() {
                    return text.trim().to_string();
                }
            }
        }
    }
    let text = value.to_string();
    if text == "null" {
        String::new()
    } else if text.chars().count() > 300 {
        text.chars().take(300).collect()
    } else {
        text
    }
}

pub fn classify(status: u16, value: &Value) -> GatewayError {
    let message = {
        let text = error_text(value);
        if text.is_empty() {
            match status {
                401 | 403 => "Tabbit 登录态已失效，请重新导出 Cookie".to_string(),
                409 => "Tabbit 会话正在处理中，请稍后重试".to_string(),
                429 => "Tabbit 请求过于频繁，请稍后重试".to_string(),
                492 => "Tabbit 当前账号没有该模型的权限（需要 Pro 或今日额度已用完）".to_string(),
                _ => format!("Tabbit 请求失败（HTTP {status}）"),
            }
        } else {
            text
        }
    };
    let status_code = match status {
        401 | 403 => 401,
        429 => 429,
        492 => 429,
        409 => 409,
        value if value >= 500 => 502,
        value => i32::from(value),
    };
    GatewayError::with_status(status_code, message)
}

pub async fn signed_json(
    creds: &Credentials,
    realm: Realm,
    client: &reqwest::Client,
    method: &str,
    path: &str,
    body: Option<Value>,
    referer: &str,
) -> Result<Value, GatewayError> {
    // `/panel/session` 的上游 body 是空串（不是 `null`），签名要签同一份字节。
    let body_text = match body {
        Some(Value::Null) | None => String::new(),
        Some(value) => value.to_string(),
    };
    let key = sign_key(creds, realm, client).await;
    let mut headers = signed_headers(creds, realm, referer, &body_text, &key, true);
    headers.push(("Accept".to_string(), "application/json".to_string()));
    if method == "POST" {
        headers.push(("Content-Type".to_string(), "application/json".to_string()));
    }
    let (status, bytes) = send(
        client,
        method,
        &format!("{}{}", realm.base(), path),
        &headers,
        if method == "POST" { Some(body_text.as_str()) } else { None },
        CONTROL_TIMEOUT,
    )
    .await?;
    let value = parse_json(&bytes);
    if !(200..300).contains(&status) {
        return Err(classify(status, &value));
    }
    Ok(value)
}

pub async fn user_info(
    creds: &Credentials,
    realm: Realm,
    client: &reqwest::Client,
) -> Result<(String, String), GatewayError> {
    let value = signed_json(
        creds,
        realm,
        client,
        "GET",
        "/api/v0/user/base-info",
        None,
        "/",
    )
    .await?;
    let info = value.get("user_info").cloned().unwrap_or(Value::Null);
    let uid = crate::store::pick(&info, &["user_id", "uid", "id"]);
    let nickname = crate::store::pick(&info, &["display_name", "nickname", "name", "user_name"]);
    let nickname = if nickname.is_empty() {
        crate::crypto::decode_jwt_claims(&creds.token)
            .as_ref()
            .and_then(|claims| claims.get("name").and_then(Value::as_str))
            .unwrap_or("")
            .to_string()
    } else {
        nickname
    };
    Ok((uid, nickname))
}

// ─── 会话 ────────────────────────────────────────────────────

fn sessions() -> &'static Mutex<BTreeMap<String, (String, i64)>> {
    static CACHE: OnceLock<Mutex<BTreeMap<String, (String, i64)>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

pub async fn session_id(
    creds: &Credentials,
    realm: Realm,
    client: &reqwest::Client,
) -> Result<String, GatewayError> {
    let cache_key = format!("{}|{}", realm.id(), creds.uid);
    let now = crate::http::now_ms();
    if let Ok(mut guard) = sessions().lock() {
        if let Some((id, at)) = guard.get_mut(&cache_key) {
            if now - *at < SESSION_TTL_MS {
                *at = now;
                return Ok(id.clone());
            }
        }
    }
    let value = signed_json(
        creds,
        realm,
        client,
        "POST",
        "/panel/session",
        Some(Value::Null),
        "/newtab",
    )
    .await?;
    let id = value
        .get("chat_session_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| GatewayError::with_status(502, "Tabbit 没有返回会话 id"))?
        .to_string();
    if let Ok(mut guard) = sessions().lock() {
        guard.insert(cache_key, (id.clone(), now));
    }
    Ok(id)
}

pub fn drop_session(creds: &Credentials, realm: Realm) {
    let cache_key = format!("{}|{}", realm.id(), creds.uid);
    if let Ok(mut guard) = sessions().lock() {
        guard.remove(&cache_key);
    }
}

// ─── 对话：join + runs ───────────────────────────────────────

fn path_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

pub fn build_join_payload(seed: &str) -> String {
    let payload = json!({
        "page_instance_id": sign::random_uuid(),
        "device_id_hash": sign::sha256_hex(format!("tabbit-page:{seed}").as_bytes()),
        "surface": "panel",
        "last_event_id": Value::Null,
    });
    payload.to_string()
}

pub fn build_run_payload(content: &str, model: &str) -> String {
    let run_id = format!("tab_{}", sign::random_uuid());
    let payload = json!({
        "client_run_id": run_id,
        "client_message_id": run_id,
        "agent_plan_mode_enabled": false,
        "input_payload": {
            "task_name": "chat",
            "content": content,
            "parent_message_id": Value::Null,
            "selected_model": [model],
            "references": [],
            "metadatas": { "html_content": format!("<p>{content}</p>") },
            "page_info_list": [],
            "is_mobile": false,
            "agent_mode": false,
            "entity": { "key": sign::md5_empty_hex(), "extras": { "type": "tab", "url": "" } },
        },
    });
    payload.to_string()
}

/// 订阅房间事件流（唯一带签名的对话请求）。499 时刷新 key 重试一次。
pub async fn open_join(
    creds: &Credentials,
    realm: Realm,
    room: &str,
    body: &str,
    client: &reqwest::Client,
) -> Result<reqwest::Response, GatewayError> {
    let target = format!(
        "{}/api/v3/chat/rooms/{}/join",
        realm.base(),
        path_escape(room)
    );
    for attempt in 0..2 {
        let mut headers = base_headers(creds, realm, &format!("/panel/{room}"));
        headers.push(("Accept".to_string(), "text/event-stream".to_string()));
        headers.push(("Content-Type".to_string(), "application/json".to_string()));
        headers.push(("Cache-Control".to_string(), "no-cache".to_string()));
        headers.push(("trace-id".to_string(), sign::random_uuid()));
        headers.push(("Cookie".to_string(), creds.cookie.clone()));
        let key = sign_key(creds, realm, client).await;
        for (name, value) in sign::signature_headers(&key, body, crate::http::now_ms()) {
            headers.push((name, value));
        }
        let mut request = client.post(&target);
        for (name, value) in &headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request
            .body(body.to_string())
            .send()
            .await
            .map_err(|error| GatewayError::with_status(502, format!("无法连接 Tabbit：{error}")))?;
        let status = response.status().as_u16();
        if status == 499 && attempt == 0 {
            refresh_sign_key(creds, realm, client).await;
            continue;
        }
        if (200..300).contains(&status) {
            // 正常是 SSE；JSON 说明上游用 200 包了一个业务错误（权益/会话）。
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_ascii_lowercase();
            if content_type.contains("application/json") {
                let bytes = response.bytes().await.unwrap_or_default();
                let value = parse_json(&bytes);
                return Err(classify(status, &value));
            }
            return Ok(response);
        }
        let bytes = response.bytes().await.unwrap_or_default();
        let value = parse_json(&bytes);
        if status == 409 {
            drop_session(creds, realm);
        }
        return Err(classify(status, &value));
    }
    Err(GatewayError::with_status(
        502,
        "Tabbit 签名 key 刷新后仍返回 499",
    ))
}

/// 投递一次对话（上游 runs 不带签名），返回 run_id。
pub async fn post_run(
    creds: &Credentials,
    realm: Realm,
    room: &str,
    body: &str,
    client: &reqwest::Client,
) -> Result<String, GatewayError> {
    let target = format!(
        "{}/api/v3/chat/rooms/{}/runs",
        realm.base(),
        path_escape(room)
    );
    let mut headers = base_headers(creds, realm, &format!("/panel/{room}"));
    headers.push(("Accept".to_string(), "application/json".to_string()));
    headers.push(("Content-Type".to_string(), "application/json".to_string()));
    headers.push(("trace-id".to_string(), sign::random_uuid()));
    headers.push(("Cookie".to_string(), creds.cookie.clone()));
    let (status, bytes) = send(
        client,
        "POST",
        &target,
        &headers,
        Some(body),
        RUN_TIMEOUT,
    )
    .await?;
    let value = parse_json(&bytes);
    if !(200..300).contains(&status) {
        if status == 409 {
            drop_session(creds, realm);
        }
        return Err(classify(status, &value));
    }
    let run_id = crate::store::pick(&value, &["run_id", "runId"]);
    if run_id.is_empty() {
        return Err(GatewayError::with_status(502, "Tabbit 未返回 run 信息"));
    }
    Ok(run_id)
}

pub fn now() -> i64 {
    crate::http::now_ms()
}

pub fn as_i64(value: &Value) -> Option<i64> {
    if let Some(number) = value.as_i64() {
        return Some(number);
    }
    if let Some(number) = value.as_f64() {
        return Some(number as i64);
    }
    value.as_str().and_then(|text| text.trim().parse::<i64>().ok())
}

pub fn as_f64(value: &Value) -> Option<f64> {
    if let Some(number) = value.as_f64() {
        return Some(number);
    }
    value
        .as_str()
        .and_then(|text| text.trim().trim_end_matches('%').parse::<f64>().ok())
}

/// 额度接口要的时区名。国内站按上海、国际站按 UTC；两者只影响重置时刻的
/// 展示，不影响百分比读数。
pub fn timezone(realm: Realm) -> &'static str {
    match realm {
        Realm::Cn => "Asia/Shanghai",
        Realm::Intl => "UTC",
    }
}

/// 从账号记录里取客户端（控制面共用）。
pub fn record_client_for(
    ctx: &PluginCtx,
    record: &StoreAccount,
) -> Result<reqwest::Client, GatewayError> {
    record_client(record, &ctx.env.proxy).map_err(|error| GatewayError::with_status(502, error))
}


