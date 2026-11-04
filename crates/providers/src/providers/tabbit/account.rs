//! Tabbit 账号：粘贴 Cookie / cURL / 裸 JWT → 可落库账号。
//!
//! ── 没有续期接口（实测）─────────────────────────────────────
//! 登录态是 RS256 JWT，有效期 7 天（`expires_in: 604800`），上游没有
//! refresh 端点。过期只能重新导出 Cookie。因此：
//! - `canRefresh: false`，账号行如实显示有效期；
//! - `refresh` 能力位为 false（不参与后台凭证维护）；
//! - 账号身份用 `user_id`（JWT 里也有），换一次令牌仍是同一个账号。

use agent2api_provider_protocol as proto;
use serde_json::{json, Value};

use crate::errors::GatewayError;

use super::client::{self, Credentials};
use super::config::Realm;
use super::sign;

fn bad(message: impl Into<String>) -> GatewayError {
    GatewayError::with_status(400, message)
}

fn pick_text(value: &Value, keys: &[&str]) -> String {
    crate::store::pick(value, keys)
}

fn bool_field(value: &Value, key: &str, default: bool) -> bool {
    match value.get(key) {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => {
            let text = text.trim().to_ascii_lowercase();
            if ["0", "false", "off", "no", "关闭", "否"].contains(&text.as_str()) {
                false
            } else if ["1", "true", "on", "yes", "开启", "是"].contains(&text.as_str()) {
                true
            } else {
                default
            }
        }
        Some(Value::Number(number)) => number.as_i64().unwrap_or(0) != 0,
        _ => default,
    }
}

pub fn token_expiry_ms(token: &str) -> Option<i64> {
    let claims = crate::crypto::decode_jwt_claims(token)?;
    let exp = claims.get("exp").and_then(client::as_i64)?;
    if exp <= 0 {
        return None;
    }
    Some(exp * 1000)
}

pub fn account_view(secret: &Value) -> Value {
    let token = pick_text(secret, &["accessToken", "token", "access_token"]);
    let uid = pick_text(secret, &["uid", "user_id", "userId"]);
    let nickname = pick_text(secret, &["nickname"]);
    let unlock_pro = bool_field(secret, "unlockPro", true);
    let name = {
        let explicit = pick_text(secret, &["name"]);
        if !explicit.is_empty() {
            explicit
        } else if !nickname.is_empty() {
            nickname
        } else if !uid.is_empty() {
            format!("Tabbit 账号 {uid}")
        } else {
            "Tabbit 账号".to_string()
        }
    };
    let mut view = json!({
        "name": name,
        "uid": uid,
        "tokenTail": sign::token_tail(&token),
        "hasSession": true,
        "canRefresh": false,
        "unlockPro": unlock_pro,
    });
    if let Some(expires_at) = token_expiry_ms(&token) {
        if let Some(object) = view.as_object_mut() {
            object.insert("expiresAt".to_string(), Value::from(expires_at));
        }
    }
    view
}

pub fn identity_of(secret: &Value) -> String {
    let uid = pick_text(secret, &["uid", "user_id", "userId"]);
    if !uid.is_empty() {
        return format!("uid-{uid}");
    }
    let token = pick_text(secret, &["accessToken", "token", "access_token"]);
    if token.is_empty() {
        String::new()
    } else {
        format!("token-{}", sign::sha256_hex(token.as_bytes()))
    }
}

fn fields_of(input: &proto::NormalizeInput) -> Result<Value, GatewayError> {
    match &input.source {
        proto::NormalizeSource::Manual { payload } => {
            // 动态方法：`{methodIndex, inputs:{...}}`；标准表单是平铺字段。
            if let Some(inputs) = payload.get("inputs").filter(|value| value.is_object()) {
                Ok(inputs.clone())
            } else {
                Ok(payload.clone())
            }
        }
        proto::NormalizeSource::FileText { content, .. } => {
            let (token, cookie, uid, unlock_pro, client_version) =
                client::normalize_input_text(content);
            Ok(json!({
                "token": token,
                "cookie": cookie,
                "uid": uid,
                "unlockPro": unlock_pro,
                "clientVersion": client_version,
            }))
        }
        proto::NormalizeSource::FileBase64 { .. } => Err(bad(
            "Tabbit 不支持二进制凭证文件，请粘贴 Cookie、cURL 整段或 JWT",
        )),
        proto::NormalizeSource::Desktop => Err(bad(
            "Tabbit 没有可读取的本机桌面登录态，请粘贴网页 Cookie 或 cURL",
        )),
    }
}

pub async fn normalize(
    ctx: &agent2api_provider_sdk::PluginCtx,
    input: proto::NormalizeInput,
    realm: Realm,
) -> Result<proto::NormalizedAccount, GatewayError> {
    let fields = fields_of(&input)?;
    let mut token = pick_text(&fields, &["accessToken", "access_token", "token"]);
    let raw_cookie = pick_text(&fields, &["cookie"]);
    let mut uid = pick_text(&fields, &["uid", "user_id", "userId"]);
    let mut unlock_pro = bool_field(&fields, "unlockPro", true);
    let client_version = pick_text(&fields, &["clientVersion"]);

    // Cookie 里有 token 时以它为准（与参考实现 `PrepareAccount` 同口径：
    // 同时粘了 Cookie 和 JWT 时，Cookie 才是浏览器真正在用的那份）。
    if !raw_cookie.is_empty() {
        let from_cookie = client::cookie_token(&client::extract_cookie(&raw_cookie));
        if !from_cookie.is_empty() {
            token = from_cookie;
        }
    }
    // 有人会把裸 JWT 粘进 Cookie 框：没有 `=` 就按令牌处理。
    if token.is_empty() && !raw_cookie.trim().is_empty() && !raw_cookie.contains('=') {
        token = raw_cookie.trim().trim_start_matches("Bearer ").trim().to_string();
    }
    if token.is_empty() {
        return Err(bad(
            "请粘贴 Tabbit 网页 Cookie（token=…; user_id=…）、DevTools 的 Copy as cURL 整段，或裸 JWT",
        ));
    }
    token = token
        .trim()
        .trim_start_matches("Bearer ")
        .trim()
        .to_string();
    if token.len() > 32768 || token.contains(['\r', '\n']) {
        return Err(bad("Tabbit 登录令牌格式无效"));
    }
    if uid.is_empty() {
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
    // 账号上显式写了 unlockPro 就以它为准（表单默认开启）。
    unlock_pro = bool_field(&fields, "unlockPro", unlock_pro);

    let provisional = Credentials {
        token: token.clone(),
        cookie: if raw_cookie.trim().is_empty() {
            String::new()
        } else {
            client::extract_cookie(&raw_cookie)
        },
        uid: uid.clone(),
        unlock_pro,
        client_version: if client_version.is_empty() {
            super::config::DEFAULT_CLIENT_VERSION.to_string()
        } else {
            client_version.clone()
        },
    };
    let mut provisional = provisional;
    if provisional.cookie.is_empty() {
        provisional.cookie = format!(
            "token={}{}",
            provisional.token,
            if provisional.uid.is_empty() {
                String::new()
            } else {
                format!("; user_id={}", provisional.uid)
            }
        );
    }

    let client = crate::http::client(input.proxy.as_ref(), &ctx.env.proxy);
    let (verified_uid, nickname) = match client::user_info(&provisional, realm, &client).await {
        Ok(value) => value,
        Err(error)
            if matches!(error.status_code, 301 | 302 | 303 | 307 | 308 | 401 | 403) =>
        {
            return Err(bad(format!(
                "Tabbit 登录态校验失败：这可能是另一个地区的账号，请改用「{}」添加",
                other_realm(realm).label()
            )))
        }
        Err(error) => {
            return Err(bad(format!("Tabbit 登录态校验失败：{}", error.message)));
        }
    };
    if !verified_uid.is_empty() {
        uid = verified_uid;
    }
    let cookie = if raw_cookie.trim().is_empty() {
        let mut parts = vec![format!("token={token}")];
        if !uid.is_empty() {
            parts.push(format!("user_id={uid}"));
            parts.push(format!("SAPISID={uid}"));
        }
        parts.push("managed=tab_browser".to_string());
        parts.push("NEXT_LOCALE=zh".to_string());
        parts.join("; ")
    } else {
        let extracted = client::extract_cookie(&raw_cookie);
        let mut parts: Vec<String> = Vec::new();
        let mut has_token = false;
        for part in extracted.split(';') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let mut pieces = part.splitn(2, '=');
            let name = pieces.next().unwrap_or("").trim();
            let value = pieces.next().unwrap_or("").trim();
            if !name.is_empty() && !value.is_empty() {
                if name == "token" {
                    has_token = true;
                }
                parts.push(format!("{name}={value}"));
            }
        }
        if !has_token {
            parts.insert(0, format!("token={token}"));
        }
        if !uid.is_empty() && !parts.iter().any(|part| part.starts_with("user_id=")) {
            parts.push(format!("user_id={uid}"));
        }
        parts.push("managed=tab_browser".to_string());
        parts.push("NEXT_LOCALE=zh".to_string());
        parts.join("; ")
    };

    if cookie.len() > 32768 || cookie.contains(['\r', '\n']) {
        return Err(bad("Tabbit Cookie 格式无效"));
    }
    let mut secret = json!({
        "accessToken": token,
        "cookie": cookie,
        "uid": uid,
        "unlockPro": unlock_pro,
        "clientVersion": if client_version.is_empty() {
            super::config::DEFAULT_CLIENT_VERSION.to_string()
        } else {
            client_version
        },
        "source": "manual",
    });
    if let Some(name) = input
        .name
        .as_deref()
        .or_else(|| fields.get("name").and_then(Value::as_str))
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        secret["name"] = Value::String(name.chars().take(100).collect());
    }
    if !nickname.is_empty() {
        secret["nickname"] = Value::String(nickname);
    }
    Ok(proto::NormalizedAccount {
        public: account_view(&secret),
        identity: identity_of(&secret),
        secret,
        desktop: false,
    })
}

fn other_realm(realm: Realm) -> Realm {
    match realm {
        Realm::Cn => Realm::Intl,
        Realm::Intl => Realm::Cn,
    }
}
