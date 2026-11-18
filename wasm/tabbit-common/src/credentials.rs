use std::collections::BTreeMap;

use agent2api_wasm_sdk::{json, Error, Result, Value};
use base64::Engine;

const WANTED_COOKIES: [&str; 5] = ["token", "user_id", "managed", "NEXT_LOCALE", "SAPISID"];

pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| {
            value[*key]
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
        })
        .unwrap_or("")
        .to_string()
}

pub fn boolean(value: &Value, key: &str, default: bool) -> bool {
    match value.get(key) {
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_i64().is_some_and(|value| value != 0),
        Some(Value::String(value)) => match value.trim().to_lowercase().as_str() {
            "0" | "false" | "off" | "no" | "关闭" | "否" => false,
            "1" | "true" | "on" | "yes" | "开启" | "是" => true,
            _ => default,
        },
        _ => default,
    }
}

pub fn jwt(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(payload))
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

pub fn jwt_expiry_ms(claims: &Value) -> Option<i64> {
    let value = &claims["exp"];
    let seconds = value.as_i64().or_else(|| {
        value
            .as_str()
            .filter(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|text| text.parse::<i64>().ok())
    })?;
    seconds
        .checked_mul(1000)
        .filter(|value| *value > 0 && *value <= (1_i64 << 53) - 1)
}

pub fn jwt_uid(token: &str) -> String {
    jwt(token)
        .map(|claims| text(&claims, &["id", "user_id", "uid", "sub", "userId"]))
        .unwrap_or_default()
}

/// 只取 Cookie 部分，不执行或跟随 cURL 中的命令、地址和其它头。
pub fn extract_cookie(raw: &str) -> String {
    let raw = raw.trim();
    let lower = raw.to_ascii_lowercase();
    if let Some(at) = lower.find("cookie:") {
        let rest = &raw[at + 7..];
        let end = rest.find(['\'', '"', '\r', '\n']).unwrap_or(rest.len());
        if rest[..end].contains('=') {
            return rest[..end].trim().to_string();
        }
    }
    let start = lower
        .find(" -b ")
        .map(|at| at + 4)
        .or_else(|| lower.starts_with("-b ").then_some(3))
        .or_else(|| lower.find(" --cookie ").map(|at| at + 10));
    if let Some(start) = start {
        let rest = raw[start..].trim_start();
        let value = match rest.chars().next().filter(|ch| *ch == '\'' || *ch == '"') {
            Some(quote) => rest[quote.len_utf8()..].split(quote).next().unwrap_or(""),
            None => rest.split_whitespace().next().unwrap_or(""),
        };
        if value.contains('=') {
            return value.trim().to_string();
        }
    }
    raw.to_string()
}

fn cookie_map(cookie: &str) -> BTreeMap<String, String> {
    cookie
        .split(';')
        .filter_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            let (name, value) = (name.trim(), value.trim());
            if !WANTED_COOKIES.contains(&name) || value.is_empty() {
                return None;
            }
            Some((name.to_string(), value.to_string()))
        })
        .collect()
}

pub fn cookie_token(cookie: &str) -> String {
    cookie_map(&extract_cookie(cookie))
        .remove("token")
        .unwrap_or_default()
}

fn strip_bearer(mut text: &str) -> &str {
    text = text.trim();
    while text
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Bearer "))
    {
        text = text[7..].trim();
    }
    text
}

pub fn from_text(raw: &str) -> Result<Value> {
    if raw.len() > 128 * 1024 {
        return Err(Error::http(400, "Tabbit 凭证文本过大"));
    }
    let raw = raw.trim();
    if let Ok(value) = serde_json::from_str::<Value>(raw) {
        if value.is_object() {
            return Ok(value);
        }
    }
    let cookie = extract_cookie(raw);
    if !cookie_token(&cookie).is_empty() {
        Ok(json!({ "cookie": cookie }))
    } else {
        Ok(json!({ "token": strip_bearer(raw) }))
    }
}

#[derive(Clone)]
pub struct Credentials {
    pub token: String,
    pub cookie: String,
    pub uid: String,
    pub unlock_pro: bool,
    pub client_version: String,
}

impl Credentials {
    pub fn from_secret(secret: &Value) -> Result<Self> {
        let mut token = text(secret, &["accessToken", "access_token", "token"]);
        let raw_cookie = text(secret, &["cookie"]);
        if token.len() > 128 * 1024 || raw_cookie.len() > 128 * 1024 {
            return Err(Error::http(400, "Tabbit 凭证文本过大"));
        }
        let mut cookies = cookie_map(&extract_cookie(&raw_cookie));
        // 用户粘贴的 Cookie 与独立 JWT 不一致时，沿用浏览器 Cookie 的 token。
        if let Some(value) = cookies.get("token") {
            token = value.clone();
        }
        if token.is_empty() && !raw_cookie.is_empty() && !raw_cookie.contains('=') {
            token = raw_cookie;
        }
        if token.contains("token=") || token.to_ascii_lowercase().contains("cookie:") {
            let imported = cookie_map(&extract_cookie(&token));
            token = imported.get("token").cloned().unwrap_or_default();
            for (key, value) in imported {
                cookies.entry(key).or_insert(value);
            }
        }
        // 登录接口的access_token与Cookie是同一JWT；存量字段含Bearer前缀时只在发送前剥离。
        token = strip_bearer(token.trim().trim_matches('"').trim()).to_string();
        if token.is_empty()
            || token.len() > 32768
            || !token.is_ascii()
            || token
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
            || token.contains(';')
        {
            return Err(Error::http(
                400,
                "请填写有效的 Tabbit Cookie、Copy as cURL 或登录 JWT",
            ));
        }
        let mut uid = text(secret, &["uid", "user_id", "userId"]);
        if uid.is_empty() {
            uid = cookies.get("user_id").cloned().unwrap_or_default();
        }
        if uid.is_empty() {
            uid = jwt_uid(&token);
        }
        if uid.len() > 256 || uid.contains(['\r', '\n', ';', '\0']) {
            return Err(Error::http(400, "Tabbit 账号标识格式无效"));
        }
        cookies.insert("token".into(), token.clone());
        if !uid.is_empty() {
            cookies
                .entry("user_id".into())
                .or_insert_with(|| uid.clone());
            cookies
                .entry("SAPISID".into())
                .or_insert_with(|| uid.clone());
        }
        cookies
            .entry("managed".into())
            .or_insert_with(|| "tab_browser".into());
        cookies
            .entry("NEXT_LOCALE".into())
            .or_insert_with(|| "zh".into());
        let cookie = WANTED_COOKIES
            .iter()
            .filter_map(|name| cookies.get(*name).map(|value| format!("{name}={value}")))
            .collect::<Vec<_>>()
            .join("; ");
        if cookie.len() > 32768 || cookie.contains(['\r', '\n']) {
            return Err(Error::http(400, "Tabbit Cookie 格式无效或过大"));
        }
        let version = text(secret, &["clientVersion"]);
        let client_version = if version.is_empty() {
            super::CLIENT_VERSION.to_string()
        } else {
            version
        };
        if client_version.len() > 128
            || !client_version
                .bytes()
                .all(|byte| (32..=126).contains(&byte))
        {
            return Err(Error::http(400, "Tabbit 客户端版本格式无效"));
        }
        Ok(Self {
            token,
            cookie,
            uid,
            client_version,
            unlock_pro: boolean(secret, "unlockPro", true),
        })
    }

    pub fn ensure_valid(&self) -> Result<()> {
        let expires = jwt(&self.token).and_then(|claims| jwt_expiry_ms(&claims));
        let now = crate::now()?;
        if expires.is_some_and(|expires| expires <= now) {
            return Err(
                Error::http(401, "Tabbit 登录态已过期，请重新登录对应地区并导入 Cookie")
                    .with_code("tabbit_session_expired"),
            );
        }
        Ok(())
    }

    pub fn redact(&self, raw: &str) -> String {
        let mut value = raw
            .replace(&self.cookie, "[已隐藏凭证]")
            .replace(&self.token, "[已隐藏凭证]");
        for cookie in self.cookie.split(';') {
            if let Some((_, secret)) = cookie.trim().split_once('=') {
                if secret.len() >= 8 {
                    value = value.replace(secret, "[已隐藏凭证]");
                }
            }
        }
        value
    }
}
