use agent2api_wasm_sdk::{json, Error, Value};

use crate::account::Session;

pub fn number(value: Option<&Value>) -> Option<i64> {
    value.and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_str()?.trim().parse().ok())
    })
}

pub fn business(value: &Value) -> Option<i64> {
    [
        value.get("code"),
        value.pointer("/data/biz_code"),
        value.get("upstreamCode"),
        value.pointer("/error/code"),
    ]
    .into_iter()
    .filter_map(number)
    .find(|code| *code != 0)
}

pub fn has_error(value: &Value) -> bool {
    business(value).is_some()
        || value.get("error").is_some_and(|error| !error.is_null())
        || value["type"] == "error"
}

fn detail(value: &Value) -> &str {
    let outer = number(value.get("code")).filter(|code| *code != 0);
    let business = number(value.pointer("/data/biz_code")).filter(|code| *code != 0);
    let fields = if outer.is_none() && business.is_some() {
        [
            "/data/biz_msg",
            "/data/biz_message",
            "/data/message",
            "/message",
            "/msg",
            "/error/message",
        ]
    } else {
        [
            "/msg",
            "/message",
            "/error/message",
            "/data/biz_msg",
            "/data/biz_message",
            "/data/message",
        ]
    };
    fields
        .into_iter()
        .filter_map(|path| value.pointer(path).and_then(Value::as_str))
        .map(str::trim)
        .find(|message| {
            !message.is_empty()
                && !message.eq_ignore_ascii_case("ok")
                && !message.eq_ignore_ascii_case("success")
        })
        .or_else(|| {
            (value["type"] == "error")
                .then(|| value["content"].as_str())
                .flatten()
        })
        .unwrap_or("")
}

fn muted(value: &Value) -> bool {
    business(value) == Some(5)
        || detail(value).to_ascii_lowercase().contains("user is muted")
        || detail(value)
            .to_ascii_lowercase()
            .contains("account is muted")
}

fn reset(value: &Value) -> Option<i64> {
    if !muted(value) {
        return None;
    }
    let value = value
        .pointer("/data/biz_data/mute_until")
        .or_else(|| value.get("mute_until"))?;
    let seconds = value
        .as_f64()
        .or_else(|| value.as_str()?.parse::<f64>().ok())?;
    (seconds.is_finite() && seconds > 0.0 && seconds < i64::MAX as f64 / 1000.0)
        .then(|| (seconds * 1000.0).round() as i64)
}

fn content_blocked(value: &Value) -> bool {
    value["providerCode"] == "deepseek_content_blocked"
        || value["code"] == "deepseek_content_blocked"
        || matches!(value["finish_reason"].as_str(), Some("content_filter"))
        || matches!(value["status"].as_str(), Some("CONTENT_FILTER"))
}

fn status_for(status: u16, value: &Value) -> u16 {
    let code = business(value);
    let detail = detail(value);
    if code == Some(40303) || detail.contains("CHALLENGE_REQUIRED") {
        403
    } else if status == 401 || matches!(code, Some(40001 | 40002 | 40003)) {
        401
    } else if status == 429
        || matches!(code, Some(40029 | 429))
        || muted(value)
        || detail
            .to_ascii_lowercase()
            .contains("a message is being generated")
        || value["finish_reason"] == "rate_limit_reached"
    {
        429
    } else if (400..600).contains(&status) {
        status
    } else if (200..300).contains(&status) {
        400
    } else {
        502
    }
}

pub fn upstream(status: u16, value: &Value, session: &Session) -> Error {
    let resolved = status_for(status, value);
    let code = business(value);
    let description = if code == Some(40303) || detail(value).contains("CHALLENGE_REQUIRED") {
        "DeepSeek 网页要求人机验证，请在官方网页手动完成验证后重试"
    } else if muted(value) {
        "DeepSeek 网页账号暂时被限制，登录态未必过期；请等待上游解除限制"
    } else {
        match resolved {
            401 => "DeepSeek 网页登录态无效或已过期，请重新登录",
            429 => "DeepSeek 网页请求过于频繁，请稍后重试",
            500..=599 => "DeepSeek 网页服务暂时不可用，请稍后重试",
            403 => "DeepSeek 网页拒绝访问，请在官方网页确认账号状态",
            _ => "DeepSeek 网页请求失败",
        }
    };
    let mut message = format!("{description}（HTTP {status}");
    if let Some(code) = code {
        message.push_str(&format!("，错误码 {code}"));
    }
    message.push('）');
    let detail: String = session
        .redact(detail(value))
        .chars()
        .filter(|character| !character.is_control())
        .take(240)
        .collect();
    if !detail.is_empty() {
        message.push_str(&format!("：{detail}"));
    }
    let mut error = Error::http(resolved, message).with_code("deepseek_upstream");
    error.upstream_code = code;
    error.reset_at = reset(value);
    error
}

pub fn classify(status: u16, value: &Value) -> Value {
    let resolved = status_for(status, value);
    let message = detail(value);
    let message = if message.is_empty() {
        "DeepSeek 网页请求失败"
    } else {
        message
    };
    let kind = match resolved {
        _ if content_blocked(value) => "content_blocked",
        401 => "token_expired",
        // 账号限制使用上游实际解除时刻；普通速率窗口不猜次日重置。
        429 => "quota_limited",
        _ => "fatal",
    };
    json!({ "kind": kind, "status": resolved, "message": message,
        "upstream_code": business(value), "reset_at": reset(value) })
}

pub fn network(mut error: Error) -> Error {
    // 不回显宿主网络错误中的 URL、代理地址或响应正文。
    match error.code.as_deref() {
        Some("aborted" | "plugin_unavailable" | "forbidden" | "plugin_resource_limit") => error,
        Some("timeout") => {
            error.status = Some(504);
            error.message = "DeepSeek 网页请求超时，请检查网络与出网代理".into();
            error
        }
        _ => Error::http(502, "DeepSeek 网页网络请求失败，请检查网络与出网代理")
            .with_code("deepseek_network"),
    }
}

pub fn event(error: &Error) -> Value {
    json!({ "error": { "message": error.message, "status": error.status.unwrap_or(502),
        "code": error.code, "upstream_code": error.upstream_code, "reset_at": error.reset_at,
        "type": if error.status == Some(429) { "rate_limit_error" } else { "upstream_error" } } })
}
