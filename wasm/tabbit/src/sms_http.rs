//! 沿用旧 Tabbit login.rs 的登录头、JSON 字段与响应；不执行浏览器、不调用猜测的验证码端点。
//! 已知滑块来源仅 https://verify.meituan.com（旧源码明确的 verifyUrl 样本）。

use std::collections::BTreeMap;

use agent2api_wasm_sdk::{http, json, Error, Result, Value};
use agent2api_wasm_tabbit_common::{self as common, CLIENT_VERSION};
use sha2::{Digest, Sha256};

pub const SEND_PATH: &str = "/proxy/v0/oauth/send-verification-code";
pub const VERIFY_PATH: &str = "/proxy/v0/oauth/login";
const VERIFY_ORIGIN: &str = "https://verify.meituan.com";
const WANTED_COOKIES: [&str; 5] = ["token", "user_id", "SAPISID", "managed", "NEXT_LOCALE"];

pub struct Response {
    pub status: u16,
    pub value: Value,
    pub cookies: Vec<String>,
}

/// login.device.v1 的事实只能来自主程序根上下文，不能让 inputs/payload 覆盖或读任意环境变量。
pub fn device(params: &Value) -> Result<Value> {
    let value = &params["device"];
    let name = value["name"].as_str().filter(|value| {
        !value.trim().is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
    });
    let platform = value["platform"].as_str();
    let version = value["os_version"].as_str().filter(|value| {
        !value.trim().is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
    });
    if name.is_none() || version.is_none() || !matches!(platform, Some("win" | "mac" | "linux")) {
        return Err(Error::http(
            503,
            "主程序尚未提供有效的短信设备上下文，请更新主程序后再试",
        )
        .with_code("tabbit_sms_device_missing"));
    }
    let name = name
        .ok_or_else(|| Error::http(503, "短信设备名称缺失"))?
        .trim();
    let platform = platform.ok_or_else(|| Error::http(503, "短信设备平台缺失"))?;
    let version = version.ok_or_else(|| Error::http(503, "短信设备版本缺失"))?;
    Ok(
        json!({ "device_id_hash": format!("{:x}", Sha256::digest(format!("tabbit-login-device:{name}"))),
        "device_name": name, "platform": platform, "os_version": version,
        "browser_version": "1.17.16.0", "tabbit_version": CLIENT_VERSION }),
    )
}

pub fn post(path: &str, body: &Value) -> Result<Response> {
    if !matches!(path, SEND_PATH | VERIFY_PATH) {
        return Err(Error::http(400, "Tabbit 短信请求路径无效"));
    }
    let response = http::open(
        "POST",
        &format!("{}{path}", crate::SITE.origin),
        common::prelogin_headers(crate::SITE)?,
        Some(body),
    )
    .map_err(|_| Error::http(502, "无法连接 Tabbit 登录服务"))?;
    let status = response.status;
    // Set-Cookie 中的 Expires 包含逗号，必须从原始多值头逐条读取。
    let cookies = response
        .header_values("set-cookie")
        .into_iter()
        .map(str::to_string)
        .collect();
    let bytes = response
        .bytes(512 * 1024)
        .map_err(|_| Error::http(502, "读取 Tabbit 登录响应失败"))?;
    let value = serde_json::from_slice::<Value>(&bytes)
        .map_err(|_| Error::http(502, "Tabbit 登录响应不是有效 JSON"))?;
    Ok(Response {
        status,
        value,
        cookies,
    })
}

pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|key| value[*key].as_str())
        .map(str::trim)
        .find(|value| !value.is_empty())
        .unwrap_or("")
        .to_string()
}

pub fn code(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
}

pub fn cookie(headers: &[String]) -> Result<String> {
    let mut map = BTreeMap::new();
    for header in headers {
        let first = header.split(';').next().unwrap_or("");
        let Some((name, value)) = first.split_once('=') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim().trim_matches('"');
        if !WANTED_COOKIES.contains(&name) || value.is_empty() {
            continue;
        }
        if value.contains(['\r', '\n', ';', '\0']) {
            return Err(Error::http(502, "Tabbit 返回的登录 Cookie 格式无效"));
        }
        map.insert(name, value);
    }
    let cookie = WANTED_COOKIES
        .iter()
        .filter_map(|name| map.get(*name).map(|value| format!("{name}={value}")))
        .collect::<Vec<_>>()
        .join("; ");
    if cookie.len() > 32768 {
        return Err(Error::http(502, "Tabbit 登录 Cookie 过大"));
    }
    Ok(cookie)
}

pub fn token(cookie: &str) -> String {
    cookie
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find(|(name, _)| *name == "token")
        .map(|(_, value)| value.to_string())
        .unwrap_or_default()
}

/// 只返回确定可读且脱敏的错误文本，绝不把上游响应 JSON 或 Set-Cookie 原样端给前端。
pub fn failure(response: &Response, phone: &str, sms_code: &str, uuid: &str) -> Error {
    let value = &response.value;
    let mut message = text(value, &["error", "message", "msg", "detail_text"]);
    if message.is_empty() {
        message = text(&value["error"], &["msg", "message"]);
    }
    if message.is_empty() {
        if let Some(detail) = value["detail"].as_array().and_then(|items| items.first()) {
            message = text(detail, &["msg", "message"]);
        } else {
            message = text(value, &["detail"]);
        }
    }
    if message.is_empty() {
        message = format!("Tabbit 登录服务返回 HTTP {}", response.status);
    }
    for prefix in ["Value error, ", "value error, ", "Validation error, "] {
        if let Some(rest) = message.strip_prefix(prefix) {
            message = rest.trim().to_string();
            break;
        }
    }
    for secret in [phone, sms_code, uuid] {
        if !secret.is_empty() {
            message = message.replace(secret, "[已隐藏]");
        }
    }
    let access = text(value, &["access_token", "accessToken", "token"]);
    if !access.is_empty() {
        message = message.replace(&access, "[已隐藏]");
    }
    for header in &response.cookies {
        if let Some((_, secret)) = header.split(';').next().unwrap_or("").split_once('=') {
            if !secret.is_empty() {
                message = message.replace(secret, "[已隐藏]");
            }
        }
    }
    let mut error = Error::http(
        if response.status == 429 { 429 } else { 400 },
        message.chars().take(300).collect::<String>(),
    )
    .with_code("tabbit_sms_upstream");
    error.upstream_code = code(&value["code"]);
    error
}

pub fn challenge(value: &Value) -> Result<Option<Value>> {
    let data = &value["data"];
    let request_code = text(data, &["requestCode"]);
    let verify_url = text(data, &["verifyUrl"]);
    if request_code.is_empty() && verify_url.is_empty() {
        return Ok(None);
    }
    let valid = !request_code.is_empty()
        && request_code.len() <= 2048
        && !request_code.chars().any(char::is_control)
        && verify_url.len() <= 8192
        && !verify_url
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
        && url::Url::parse(&verify_url).is_ok_and(|url| {
            url.origin().ascii_serialization() == VERIFY_ORIGIN
                && url.scheme() == "https"
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
        });
    if !valid {
        return Err(Error::http(
            502,
            "Tabbit 人机验证地址不在已确认的美团来源内，请到官方网站手动登录",
        ));
    }
    Ok(Some(
        json!({ "requestCode": request_code, "verifyUrl": verify_url }),
    ))
}
