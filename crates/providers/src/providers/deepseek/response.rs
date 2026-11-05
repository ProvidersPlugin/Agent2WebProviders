use agent2api_provider_sdk::control_error;
use futures::StreamExt;
use serde_json::Value;

fn number(value: Option<&Value>) -> Option<i64> {
    value.and_then(|value| value.as_i64().or_else(|| value.as_str()?.trim().parse().ok()))
}

fn safe_message(message: &str, headers: &[(String, String)]) -> String {
    let mut message = message.to_string();
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("authorization") {
            let token = value.strip_prefix("Bearer ").unwrap_or(value);
            if !token.is_empty() { message = message.replace(token, "[已隐藏]"); }
        } else if name.eq_ignore_ascii_case("cookie") {
            if !value.is_empty() { message = message.replace(value, "[已隐藏]"); }
            for pair in value.split(';') {
                if let Some((_, secret)) = pair.trim().split_once('=') {
                    if !secret.is_empty() { message = message.replace(secret, "[已隐藏]"); }
                }
            }
        }
    }
    message.chars().filter(|character| !character.is_control()).take(240).collect()
}

fn failure(status: u16, value: &Value, headers: &[(String, String)]) -> String {
    let outer = number(value.get("code")).filter(|code| *code != 0);
    let business = number(value.pointer("/data/biz_code")).filter(|code| *code != 0);
    let upstream_code = outer.or(business);
    let fields = if outer.is_none() && business.is_some() {
        ["/data/biz_msg", "/data/biz_message", "/data/message", "/message", "/msg", "/error/message"]
    } else {
        ["/msg", "/message", "/error/message", "/data/biz_msg", "/data/biz_message", "/data/message"]
    };
    let detail = fields.into_iter().filter_map(|path| value.pointer(path).and_then(Value::as_str))
        .map(str::trim).find(|message| !message.is_empty() && !message.eq_ignore_ascii_case("ok") && !message.eq_ignore_ascii_case("success"))
        .map(|message| safe_message(message, headers)).unwrap_or_default();
    let challenge = upstream_code == Some(40303) || detail.contains("CHALLENGE_REQUIRED");
    let auth = status == 401 || matches!(outer, Some(40001 | 40002 | 40003));
    let rate_limited = status == 429 || matches!(outer, Some(40029 | 429));
    let description = if challenge {
        "DeepSeek 网页要求人机验证，请在官方网页手动完成验证后重试"
    } else if auth {
        "DeepSeek 网页登录态无效或已过期，请重新登录"
    } else if rate_limited {
        "DeepSeek 网页请求过于频繁，请稍后重试"
    } else if status >= 500 {
        "DeepSeek 网页服务暂时不可用，请稍后重试"
    } else if status == 403 {
        "DeepSeek 网页拒绝访问，请在官方网页确认账号状态"
    } else {
        "DeepSeek 网页请求失败"
    };
    let mut message = format!("{description}（HTTP {status}");
    if let Some(code) = upstream_code { message.push_str(&format!("，错误码 {code}")); }
    message.push('）');
    if !detail.is_empty() { message.push_str(&format!("：{detail}")); }
    let status = if challenge { 403 } else if auth { 401 } else if rate_limited { 429 }
        else if (400..600).contains(&status) { i32::from(status) }
        else if (200..300).contains(&status) { 400 } else { 502 };
    control_error(status, message, None, upstream_code)
}

pub(super) fn network_error(error: reqwest::Error) -> String {
    let (status, message) = if error.is_timeout() {
        (504, "DeepSeek 网页请求超时，请检查网络与出网代理")
    } else if error.is_connect() {
        (502, "无法连接 DeepSeek 网页，请检查网络与出网代理")
    } else if error.is_builder() {
        (400, "DeepSeek 网页请求信息无效，请重新登录或检查导入的凭证")
    } else {
        (502, "DeepSeek 网页网络请求失败，请检查网络后重试")
    };
    control_error(status, message, None, None)
}

pub(super) async fn control(response: reqwest::Response, headers: &[(String, String)]) -> Result<Value, String> {
    let status = response.status().as_u16();
    let challenge = response.headers().get("x-amzn-waf-action").and_then(|value| value.to_str().ok())
        .is_some_and(|value| matches!(value, "challenge" | "captcha"));
    let html = response.headers().get("content-type").and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/html"));
    let mut bytes = Vec::new();
    let mut source = response.bytes_stream();
    while let Some(chunk) = source.next().await {
        let chunk = chunk.map_err(network_error)?;
        if bytes.len().saturating_add(chunk.len()) > 1024 * 1024 { return Err("DeepSeek 控制响应过大".into()); }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        if challenge || (html && matches!(status, 202 | 403)) {
            control_error(403, format!("DeepSeek 网页返回验证或访问拦截页面（HTTP {status}），请在官方网页手动完成验证后重试"), None, None)
        } else if !(200..300).contains(&status) {
            failure(status, &Value::Null, headers)
        } else {
            control_error(502, format!("DeepSeek 网页回复格式异常（HTTP {status}），请稍后重试"), None, None)
        }
    })?;
    if challenge || !(200..300).contains(&status) || number(value.get("code")).is_some_and(|code| code != 0)
        || number(value.pointer("/data/biz_code")).is_some_and(|code| code != 0) {
        if challenge {
            return Err(control_error(403, format!("DeepSeek 网页要求人机验证（HTTP {status}），请在官方网页手动完成验证后重试"), None, None));
        }
        return Err(failure(status, &value, headers));
    }
    if !value.is_object() { return Err(control_error(502, "DeepSeek 网页回复格式异常：未返回有效对象", None, None)); }
    Ok(value)
}
