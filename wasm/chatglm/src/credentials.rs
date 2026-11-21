use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use base64::Engine;

use crate::{errors::text, sign};

pub fn jwt(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(payload))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn field(secret: &Value, names: &[&str]) -> String {
    let top = text(secret, names);
    if top.is_empty() {
        text(&secret["auth"], names)
    } else {
        top
    }
}

pub fn strip_bearer(value: &str) -> String {
    let mut value = value.trim();
    while value
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer "))
    {
        value = value[7..].trim();
    }
    value.to_string()
}

pub fn access_only(secret: &Value) -> String {
    strip_bearer(&field(
        secret,
        &["accessToken", "access_token", "token", "chatglm_token"],
    ))
}

pub fn access(secret: &Value) -> String {
    let access = access_only(secret);
    if access.is_empty() {
        refresh(secret)
    } else {
        access
    }
}

pub fn refresh(secret: &Value) -> String {
    strip_bearer(&field(
        secret,
        &["refreshToken", "refresh_token", "chatglm_refresh_token"],
    ))
}

pub fn is_guest(secret: &Value) -> bool {
    secret["isGuest"]
        .as_bool()
        .or_else(|| jwt(&access(secret)).and_then(|value| value["is_guest"].as_bool()))
        .unwrap_or_else(|| refresh(secret).is_empty())
}

pub fn expires(token: &str) -> Option<i64> {
    let value = jwt(token)?;
    value["exp"]
        .as_i64()
        .or_else(|| value["exp"].as_str().and_then(|value| value.parse().ok()))
        .filter(|value| *value > 0)
        .map(|value| value.saturating_mul(1000))
}

pub fn expiring_within(secret: &Value, min_lead_ms: i64) -> bool {
    let lead = 3_600_000_i64.max(min_lead_ms.max(0));
    (is_guest(secret) || !refresh(secret).is_empty())
        && expires(&access(secret))
            .is_some_and(|expiry| expiry.saturating_sub(lead) <= sdk::now())
}

pub fn expiring(secret: &Value) -> bool {
    expiring_within(secret, 0)
}

pub fn device(secret: &Value) -> Result<String> {
    let explicit = field(secret, &["deviceId", "device_id"]);
    if !explicit.is_empty() {
        return if sign::valid_device(&explicit) {
            Ok(explicit)
        } else {
            Err(Error::http(400, "智谱账号设备号无效，请重新导入"))
        };
    }
    for token in [access(secret), refresh(secret)] {
        if let Some(value) = jwt(&token) {
            let id = text(&value, &["device_id"]);
            if sign::valid_device(&id) {
                return Ok(id);
            }
        }
    }
    sign::uuid()
}

pub fn valid_token(token: &str) -> bool {
    token.len() <= 32768 && !token.chars().any(char::is_control)
}

pub fn read_text(input: &str) -> Result<Value> {
    if input.len() > 128 * 1024 {
        return Err(Error::http(400, "智谱凭证文本过大"));
    }
    let input = input.trim();
    if input.is_empty() {
        return Err(Error::http(400, "智谱凭证文本为空"));
    }
    if let Ok(value) = serde_json::from_str::<Value>(input) {
        if value.is_object() {
            return Ok(value);
        }
        if let Some(value) = value.as_str() {
            return Ok(json!({ "accessToken": value }));
        }
        return Err(Error::http(400, "智谱凭证 JSON 必须是字段对象或令牌字符串"));
    }
    if input.starts_with('{') || input.starts_with('[') || input.starts_with('"') {
        return Err(Error::http(400, "智谱凭证 JSON 格式无效"));
    }
    let mut parsed = json!({});
    for part in input
        .strip_prefix("Cookie:")
        .or_else(|| input.strip_prefix("cookie:"))
        .unwrap_or(input)
        .split(';')
    {
        if let Some((key, value)) = part.trim().split_once('=') {
            match key.trim() {
                "chatglm_token" | "access_token" => parsed["accessToken"] = json!(value.trim()),
                "chatglm_refresh_token" | "refresh_token" => {
                    parsed["refreshToken"] = json!(value.trim())
                }
                _ => {}
            }
        }
    }
    if parsed["accessToken"].is_string() || parsed["refreshToken"].is_string() {
        return Ok(parsed);
    }
    Ok(json!({ "accessToken": strip_bearer(input) }))
}

pub fn uid(secret: &Value) -> String {
    let uid = field(secret, &["uid", "userId", "user_id"]);
    if !uid.is_empty() {
        return uid;
    }
    jwt(&access(secret))
        .and_then(|value| value.get("uid").or_else(|| value.get("sub")).cloned())
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| value.to_string())
        })
        .unwrap_or_default()
}
