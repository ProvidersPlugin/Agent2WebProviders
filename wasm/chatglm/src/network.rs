use crate::{endpoints, errors, sign};
use agent2api_wasm_sdk::{http, json, Error, Result, Value};

pub fn open(
    method: &str,
    path: &str,
    token: &str,
    device: &str,
    body: Option<&Value>,
    accept: &str,
) -> Result<http::Response> {
    if !path.starts_with("/chatglm/") || path.contains(['\r', '\n', '#']) {
        return Err(Error::http(400, "智谱请求路径无效"));
    }
    http::open(
        method,
        &format!("{}{path}", endpoints::ORIGIN),
        sign::headers(token, device, accept)?,
        body,
    )
    .map_err(|mut error| {
        error.message = errors::redact(&error.message, &[token]);
        error
    })
}

pub fn json(
    method: &str,
    path: &str,
    token: &str,
    device: &str,
    body: Option<&Value>,
) -> Result<Value> {
    let response = open(method, path, token, device, body, "application/json")?;
    let status = response.status;
    let bytes = response.bytes(4 * 1024 * 1024).map_err(|mut error| {
        error.message = errors::redact(&error.message, &[token]);
        error
    })?;
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        return Err(errors::upstream(status, &value, &[token]));
    }
    if !value.is_object() {
        return Err(Error::http(502, "智谱清言响应不是有效业务信封"));
    }
    Ok(value)
}

pub fn envelope(value: &Value, secrets: &[&str]) -> Result<Value> {
    if errors::failed(value) {
        return Err(errors::upstream(200, value, secrets));
    }
    if !value.get("result").is_some() {
        return Err(Error::http(502, "智谱响应缺少业务结果，未按空值成功处理"));
    }
    Ok(value["result"].clone())
}

pub fn get(path: &str, token: &str, device: &str) -> Result<Value> {
    let value = json("GET", path, token, device, None)?;
    envelope(&value, &[token])
}

pub fn post(path: &str, token: &str, device: &str, body: &Value) -> Result<Value> {
    let value = json("POST", path, token, device, Some(body))?;
    envelope(&value, &[token])
}

pub fn tokens(device: &str, refresh: Option<&str>) -> Result<Value> {
    let token = refresh.unwrap_or("");
    let path = if refresh.is_some() {
        endpoints::REFRESH
    } else {
        endpoints::GUEST
    };
    let result = post(path, token, device, &json!({}))?;
    let access = errors::text(&result, &["access_token"]);
    if access.is_empty() {
        return Err(Error::http(401, "智谱换证未返回 access_token，请重新登录"));
    }
    let rotated = errors::text(&result, &["refresh_token"]);
    if !crate::credentials::valid_token(&access) || !crate::credentials::valid_token(&rotated) {
        return Err(Error::http(502, "智谱换证返回的令牌格式无效"));
    }
    Ok(
        json!({ "accessToken": access, "refreshToken": if rotated.is_empty() { token } else { &rotated },
        "uid": errors::identifier(&result, &["user_id", "uid"]) }),
    )
}
