use agent2api_wasm_sdk::{http, json, Error, Result, Value};

use crate::{account::Session, errors, pow, BASE};

pub fn headers(session: &Session) -> Value {
    let mut value = json!({ "Authorization": format!("Bearer {}", session.token),
        "User-Agent": session.user_agent, "Accept": "application/json", "Origin": BASE,
        "Referer": format!("{BASE}/"), "x-client-platform": "web", "x-client-version": "2.7.0",
        "x-client-locale": "zh_CN", "x-client-bundle-id": "com.deepseek.chat" });
    if !session.cookie.is_empty() {
        value["Cookie"] = json!(session.cookie);
    }
    value
}

pub fn get(session: &Session, path: &str) -> Result<Value> {
    session.check()?;
    control(
        http::open_timeout(
            "GET",
            &format!("{BASE}{path}"),
            headers(session),
            None,
            30_000,
        )
        .map_err(errors::network)?,
        session,
    )
}

pub fn post(session: &Session, path: &str, body: &Value) -> Result<Value> {
    session.check()?;
    let mut headers = headers(session);
    headers["Content-Type"] = json!("application/json");
    control(
        http::open_timeout(
            "POST",
            &format!("{BASE}{path}"),
            headers,
            Some(body),
            30_000,
        )
        .map_err(errors::network)?,
        session,
    )
}

pub fn mime(response: &http::Response) -> String {
    response
        .header_values("content-type")
        .first()
        .copied()
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

pub fn challenge(response: &http::Response) -> bool {
    response
        .header_values("x-amzn-waf-action")
        .iter()
        .any(|value| {
            value.eq_ignore_ascii_case("challenge") || value.eq_ignore_ascii_case("captcha")
        })
}

pub fn control(response: http::Response, session: &Session) -> Result<Value> {
    let status = response.status;
    let challenge = challenge(&response);
    let html = mime(&response) == "text/html";
    if challenge || html && matches!(status, 200 | 202 | 403) {
        return Err(Error::http(403, format!("DeepSeek 网页返回验证或访问拦截页面（HTTP {status}），请在官方网页手动完成验证后重试"))
            .with_code("deepseek_challenge_required"));
    }
    // bytes 消费并关闭连接；失败提前返回也由 Response::drop 关闭。
    let bytes = response.bytes(1024 * 1024).map_err(errors::network)?;
    session.check()?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        if !(200..300).contains(&status) {
            errors::upstream(status, &Value::Null, session)
        } else {
            Error::http(502, format!("DeepSeek 网页回复格式异常（HTTP {status}）"))
        }
    })?;
    if !(200..300).contains(&status) || errors::has_error(&value) {
        return Err(errors::upstream(status, &value, session));
    }
    if !value.is_object() {
        return Err(Error::http(502, "DeepSeek 网页未返回有效对象"));
    }
    Ok(value)
}

pub fn proof(session: &Session, path: &str) -> Result<String> {
    let response = post(
        session,
        "/api/v0/chat/create_pow_challenge",
        &json!({ "target_path": path }),
    )?;
    let challenge = response
        .pointer("/data/biz_data/challenge")
        .ok_or_else(|| Error::http(502, "DeepSeek 没有返回工作量证明 challenge"))?;
    pow::solve_for(challenge, path)
}
