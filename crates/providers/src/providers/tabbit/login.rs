//! Tabbit 国内站手机号 + 短信验证码登录。
//!
//! ── 上游流程（实测，国内站与国际站同构；国际站灰度里被 403，本适配只开国内）──
//! ```text
//!   POST /proxy/v0/oauth/send-verification-code
//!        {"uuid":"<64 位随机串>","platform":"1","version":"","app":"1000","mobile":"<手机号>"}
//!        → {"code":0,"error":null,"data":null}
//!   POST /proxy/v0/oauth/login
//!        同上前缀 + {"smsCode":"<验证码>","device_info":{…}}
//!        → {"success":true,"code":"<授权码>","expires_in":604800,
//!           "user_id":"<uid>","access_token":"<JWT>"}
//!        响应头的 Set-Cookie 就是浏览器那份登录态（约 3.4KB）
//! ```
//! **登录态在 Set-Cookie 里**：body 的 `access_token` 只是同一份 JWT 的副本，
//! 所以这里必须保留响应头，不能像普通控制面请求那样只读 body。
//!
//! ── 为什么不需要签名 ────────────────────────────────────────
//! 登录发生在拿到账号之前，没有可签的 cookie；上游这两个端点只认
//! `x-req-ctx` / `unique-uuid` 与设备信息。与参考实现一致。
//!
//! ── 中间态（uuid）为什么不存服务端 ──────────────────────────
//! 发码与登录是两次 HTTP 请求，中间可能被用户放弃；把 uuid 回给前端、
//! 登录时带回来，服务端就不必维护一份要清理的登录会话。与 AutoClaw 的
//! `deviceId`、Loomy 的 `msgid` 同一处置。

use std::collections::BTreeMap;
use std::time::Duration;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use crate::errors::GatewayError;

use super::account;
use super::client::{self, SEC_CH_UA, USER_AGENT};
use super::config::{Realm, DEFAULT_CLIENT_VERSION};
use super::sign;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(30);
const LOGIN_APP_ID: &str = "1000";
const LOGIN_PLATFORM: &str = "1";
const WANTED_COOKIES: [&str; 5] = ["token", "user_id", "SAPISID", "managed", "NEXT_LOCALE"];

fn bad(message: impl Into<String>) -> GatewayError {
    GatewayError::with_status(400, message)
}

/// 64 位 `[A-Za-z0-9]` 的登录会话标识。
fn login_uuid() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut raw = [0u8; 64];
    if getrandom::getrandom(&mut raw).is_err() {
        let seed = sign::random_uuid().replace('-', "");
        for (index, slot) in raw.iter_mut().enumerate() {
            *slot = seed.as_bytes()[index % seed.len()];
        }
    }
    raw.iter()
        .map(|byte| ALPHABET[(usize::from(*byte)) % ALPHABET.len()] as char)
        .collect()
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .map(|value| value.trim().to_string())
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn device_info() -> Value {
    let host = hostname();
    let platform = match std::env::consts::OS {
        "windows" => "win",
        "macos" => "mac",
        _ => "linux",
    };
    let os_version = if cfg!(windows) {
        "10.0"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };
    json!({
        "device_id_hash": sign::sha256_hex(format!("tabbit-login-device:{host}").as_bytes()),
        "device_name": host,
        "platform": platform,
        "os_version": os_version,
        "browser_version": "1.17.16.0",
        "tabbit_version": DEFAULT_CLIENT_VERSION,
    })
}

fn login_headers(realm: Realm) -> Vec<(String, String)> {
    vec![
        ("User-Agent".to_string(), USER_AGENT.to_string()),
        ("sec-ch-ua".to_string(), SEC_CH_UA.to_string()),
        ("sec-ch-ua-platform".to_string(), "\"Windows\"".to_string()),
        ("sec-ch-ua-mobile".to_string(), "?0".to_string()),
        ("Accept".to_string(), "*/*".to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Origin".to_string(), realm.base().to_string()),
        (
            "Referer".to_string(),
            format!("{}/login?callback=close", realm.base()),
        ),
        (
            "x-req-ctx".to_string(),
            sign::base64_text(DEFAULT_CLIENT_VERSION),
        ),
        // 登录前没有账号，Pro 标记按「非默认浏览器」如实填 false。
        (
            "unique-uuid".to_string(),
            sign::unique_uuid(false, crate::http::now_ms()),
        ),
        ("trace-id".to_string(), sign::random_uuid()),
        ("Cookie".to_string(), "NEXT_LOCALE=zh".to_string()),
    ]
}

async fn post_login(
    ctx: &PluginCtx,
    realm: Realm,
    path: &str,
    body: &Value,
) -> Result<(u16, Value, String, Vec<String>), GatewayError> {
    let client = client::client_for(None, &ctx.env.proxy)
        .map_err(|error| GatewayError::with_status(502, error))?;
    let mut request = client
        .post(format!("{}{}", realm.base(), path))
        .timeout(LOGIN_TIMEOUT)
        .body(body.to_string());
    for (name, value) in login_headers(realm) {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request
        .send()
        .await
        .map_err(|error| GatewayError::with_status(502, format!("无法连接 Tabbit 登录服务：{error}")))?;
    let status = response.status().as_u16();
    let cookies = response
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok().map(str::to_string))
        .collect::<Vec<_>>();
    let bytes = response
        .bytes()
        .await
        .map_err(|_| GatewayError::with_status(502, "读取 Tabbit 登录响应失败"))?;
    let text = String::from_utf8_lossy(&bytes).to_string();
    let value = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
    Ok((status, value, text, cookies))
}

fn tidy_upstream(message: &str) -> String {
    let trimmed = message.trim();
    for prefix in ["Value error, ", "value error, ", "Validation error, "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest.trim().to_string();
        }
    }
    trimmed.to_string()
}

fn login_error_text(status: u16, value: &Value, raw: &str) -> String {
    if let Some(text) = ["error", "message", "msg", "detail_text"]
        .iter()
        .filter_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|text| !text.is_empty())
    {
        return tidy_upstream(text);
    }
    if let Some(detail) = value.get("detail").and_then(Value::as_array) {
        if let Some(text) = detail
            .first()
            .and_then(|entry| entry.get("msg").or_else(|| entry.get("message")))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            return tidy_upstream(text);
        }
    }
    if let Some(text) = value
        .get("detail")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        return tidy_upstream(text);
    }
    let raw = raw.trim();
    if !raw.is_empty() && raw.chars().count() < 300 {
        raw.to_string()
    } else {
        format!("登录接口返回 HTTP {status}")
    }
}

fn cookie_from_headers(headers: &[String]) -> String {
    let mut values = BTreeMap::new();
    for line in headers {
        let first = line.split(';').next().unwrap_or("");
        let mut pieces = first.splitn(2, '=');
        let name = pieces.next().unwrap_or("").trim();
        let value = pieces
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('"')
            .to_string();
        if !name.is_empty() && !value.is_empty() {
            values.insert(name.to_string(), value);
        }
    }
    let mut parts = Vec::new();
    for name in WANTED_COOKIES {
        if let Some(value) = values.get(name) {
            parts.push(format!("{name}={value}"));
        }
    }
    parts.join("; ")
}

fn jwt_claim(token: &str, keys: &[&str]) -> String {
    crate::crypto::decode_jwt_claims(token)
        .as_ref()
        .and_then(|claims| {
            keys.iter()
                .find_map(|key| claims.get(*key).and_then(Value::as_str))
        })
        .unwrap_or("")
        .to_string()
}

pub async fn sms_send(ctx: &PluginCtx, payload: &Value) -> Result<Value, GatewayError> {
    let phone = crate::store::pick(payload, &["phone", "mobile"]);
    if phone.trim().is_empty() {
        return Err(bad("请填写手机号"));
    }
    let uuid = login_uuid();
    let body = json!({
        "uuid": uuid,
        "platform": LOGIN_PLATFORM,
        "version": "",
        "app": LOGIN_APP_ID,
        "mobile": phone.trim(),
    });
    let (status, value, raw, _) =
        post_login(ctx, Realm::Cn, "/proxy/v0/oauth/send-verification-code", &body).await?;
    if !(200..300).contains(&status) {
        return Err(bad(login_error_text(status, &value, &raw)));
    }
    if let Some(code) = value.get("code").and_then(client::as_i64).filter(|code| *code != 0) {
        let message = login_error_text(status, &value, &raw);
        return Err(bad(format!("发送验证码失败（错误码 {code}）：{message}")));
    }
    Ok(json!({ "uuid": uuid, "deviceId": uuid }))
}

pub async fn sms_verify(
    ctx: &PluginCtx,
    payload: &Value,
    realm: Realm,
) -> Result<Value, GatewayError> {
    if realm != Realm::Cn {
        return Err(bad("Tabbit 国际站暂不支持短信登录，请粘贴网页 Cookie"));
    }
    let phone = crate::store::pick(payload, &["phone", "mobile"]);
    let code = crate::store::pick(payload, &["code", "smsCode"]);
    let uuid = crate::store::pick(payload, &["uuid", "ticket", "deviceId"]);
    if phone.trim().is_empty() || code.trim().is_empty() {
        return Err(bad("请填写手机号和验证码"));
    }
    if uuid.trim().is_empty() {
        return Err(bad("登录会话已失效，请先点「获取验证码」"));
    }
    let body = json!({
        "uuid": uuid.trim(),
        "platform": LOGIN_PLATFORM,
        "version": "",
        "app": LOGIN_APP_ID,
        "mobile": phone.trim(),
        "smsCode": code.trim(),
        "device_info": device_info(),
    });
    let (status, value, raw, cookies) =
        post_login(ctx, realm, "/proxy/v0/oauth/login", &body).await?;
    if !(200..300).contains(&status) {
        return Err(bad(login_error_text(status, &value, &raw)));
    }
    if value.get("success").and_then(Value::as_bool) == Some(false) {
        return Err(bad(login_error_text(status, &value, &raw)));
    }
    let access_token = crate::store::pick(&value, &["access_token", "accessToken"]);
    let mut uid = crate::store::pick(&value, &["user_id", "userId", "id"]);
    let mut cookie = cookie_from_headers(&cookies);
    if !cookie.contains("token=") && !access_token.is_empty() {
        let mut parts = vec![format!("token={access_token}")];
        if !uid.is_empty() {
            parts.push(format!("user_id={uid}"));
            parts.push(format!("SAPISID={uid}"));
        }
        parts.push("managed=tab_browser".to_string());
        parts.push("NEXT_LOCALE=zh".to_string());
        cookie = parts.join("; ");
    }
    if !cookie.contains("token=") {
        return Err(bad(
            "登录成功但没拿到登录态 Cookie，请改用「填写凭证」粘贴 Cookie",
        ));
    }
    let token = client::cookie_token(&cookie);
    let token = if token.is_empty() { access_token } else { token };
    if token.is_empty() {
        return Err(bad("登录成功但没有拿到访问令牌，请改用粘贴 Cookie"));
    }
    if uid.is_empty() {
        uid = jwt_claim(&token, &["user_id", "uid", "sub", "userId"]);
    }
    // 上游有些响应只给 token，不带 user_id / SAPISID；后续请求要求整串
    // Cookie（参考实现实测：只带 token 会被判 401「无权限」）。知道 uid
    // 之后把缺的几项补齐。
    if !uid.is_empty() {
        for name in ["user_id", "SAPISID"] {
            let present = cookie
                .split(';')
                .any(|part| part.trim().starts_with(&format!("{name}=")));
            if !present {
                cookie.push_str(&format!("; {name}={uid}"));
            }
        }
    }
    for (name, value) in [("managed", "tab_browser"), ("NEXT_LOCALE", "zh")] {
        let present = cookie
            .split(';')
            .any(|part| part.trim().starts_with(&format!("{name}=")));
        if !present {
            cookie.push_str(&format!("; {name}={value}"));
        }
    }
    let mut nickname = crate::store::pick(&value, &["display_name", "nickname", "name", "user_name"]);
    if nickname.is_empty() {
        nickname = jwt_claim(&token, &["name", "display_name"]);
    }
    let mut secret = json!({
        "accessToken": token,
        "cookie": cookie,
        "uid": uid,
        "unlockPro": true,
        "clientVersion": DEFAULT_CLIENT_VERSION,
        "source": "sms",
    });
    if !nickname.is_empty() {
        secret["nickname"] = Value::String(nickname);
    }
    if let Some(name) = payload
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        secret["name"] = Value::String(name.chars().take(100).collect());
    }
    let account = proto::NormalizedAccount {
        public: account::account_view(&secret),
        identity: account::identity_of(&secret),
        secret,
        desktop: false,
    };
    serde_json::to_value(account)
        .map_err(|_| GatewayError::with_status(500, "Tabbit 登录结果无法序列化"))
}
