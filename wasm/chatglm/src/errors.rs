use agent2api_wasm_sdk::{json, Error, Value};

pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|key| value[*key].as_str())
        .map(str::trim)
        .find(|text| !text.is_empty())
        .unwrap_or("")
        .to_string()
}

pub fn identifier(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| match &value[*key] {
            Value::String(value)
                if !value.trim().is_empty()
                    && value.len() <= 512
                    && !value.chars().any(char::is_control) =>
            {
                Some(value.trim().to_string())
            }
            Value::Number(value) if value.is_u64() => Some(value.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

pub fn numeric(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
}

pub fn code(value: &Value) -> Option<i64> {
    let codes: Vec<i64> = ["status", "code", "error_code", "upstreamCode"]
        .iter()
        .filter_map(|key| numeric(&value[*key]))
        .collect();
    codes
        .iter()
        .copied()
        .find(|code| !matches!(*code, 0 | 200))
        .or_else(|| codes.first().copied())
}

pub fn message(value: &Value) -> String {
    let message = text(value, &["message", "msg", "error"]);
    if message.is_empty() {
        text(&value["error"], &["message"])
    } else {
        message
    }
}

pub fn failed(value: &Value) -> bool {
    code(value).is_some_and(|code| !matches!(code, 0 | 200))
        || value["success"] == false
        || value["type"]
            .as_str()
            .is_some_and(|kind| kind.eq_ignore_ascii_case("error"))
        || value.get("error").is_some_and(|error| {
            !error.is_null() && error.as_str().is_none_or(|value| !value.trim().is_empty())
        })
}

pub fn classify(status: u16, body: &Value) -> Value {
    let code = code(body);
    let raw = message(body);
    let mut message = if raw.is_empty() {
        "智谱清言上游请求失败".to_string()
    } else {
        raw
    };
    let (kind, status) = match code {
        Some(40003 | 40103) => ("token_expired", 401),
        Some(-1001) => ("quota_limited", 429),
        Some(40013 | 40014 | 40001 | 40004) => {
            message.push_str("（签名或设备校验失败，请先检查系统时间）");
            ("fatal", 400)
        }
        _ => match status {
            401 | 403 => ("token_expired", 401),
            429 => ("quota_limited", 429),
            400 | 422
                if [
                    "blocked by security policy",
                    "unapproved channel",
                    "illegal api invocation",
                ]
                .iter()
                .any(|pattern| message.to_lowercase().contains(pattern)) =>
            {
                ("content_blocked", status)
            }
            400..=499 => ("fatal", status),
            _ => ("fatal", 502),
        },
    };
    json!({ "kind": kind, "status": status, "message": message, "upstream_code": code, "reset_at": null })
}

pub fn upstream(status: u16, value: &Value, tokens: &[&str]) -> Error {
    let class = classify(status, value);
    let message = redact(
        class["message"].as_str().unwrap_or("智谱上游请求失败"),
        tokens,
    );
    let mut error = Error::http(
        class["status"].as_u64().unwrap_or(502) as u16,
        message.chars().take(500).collect::<String>(),
    )
    .with_code("chatglm_upstream");
    error.upstream_code = code(value);
    error
}

pub fn redact(value: &str, secrets: &[&str]) -> String {
    let mut result = value.to_string();
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        result = result.replace(*secret, "[已隐藏凭证]");
    }
    result
}

pub fn sanitize(value: &Value, secrets: &[&str]) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| {
                    let name = key.to_lowercase().replace(['_', '-'], "");
                    ![
                        "accesstoken",
                        "refreshtoken",
                        "authorization",
                        "password",
                        "cookie",
                        "secret",
                    ]
                    .iter()
                    .any(|key| name.contains(key))
                })
                .map(|(key, value)| (key.clone(), sanitize(value, secrets)))
                .collect(),
        ),
        Value::Array(items) => {
            Value::Array(items.iter().map(|value| sanitize(value, secrets)).collect())
        }
        Value::String(text) => Value::String(redact(text, secrets)),
        _ => value.clone(),
    }
}

pub fn event(error: &Error) -> Value {
    json!({ "error": { "message": error.message, "status": error.status.unwrap_or(502),
        "code": error.upstream_code, "type": if error.status == Some(429) { "rate_limit_error" } else { "upstream_error" } } })
}
