use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use agent2api_provider_protocol as proto;
use serde_json::Value;

pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_USER_AGENT: &str = "undici";

fn cache() -> &'static Mutex<HashMap<String, reqwest::Client>> {
    static CACHE: OnceLock<Mutex<HashMap<String, reqwest::Client>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn client(proxy: Option<&proto::WireProxy>, gateway_proxy: &str) -> reqwest::Client {
    let key = match proxy {
        Some(proxy) => proxy.to_url(),
        None => gateway_proxy.trim().to_string(),
    };
    if let Ok(guard) = cache().lock() {
        if let Some(client) = guard.get(&key) {
            return client.clone();
        }
    }
    let built = build(&key);
    if let Ok(mut guard) = cache().lock() {
        guard.insert(key, built.clone());
    }
    built
}

fn build(proxy_url: &str) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .use_rustls_tls()
        .timeout(CONTROL_TIMEOUT)
        .user_agent(DEFAULT_USER_AGENT);
    let trimmed = proxy_url.trim();
    if trimmed.is_empty() || trimmed == "direct" {
        builder = builder.no_proxy();
    } else {
        match reqwest::Proxy::all(trimmed) {
            Ok(proxy) => builder = builder.proxy(proxy),
            Err(_) => builder = builder.no_proxy(),
        }
    }
    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

pub struct ApiResponse {
    pub status: u16,
    pub ok: bool,
    pub payload: Option<Value>,
}

// 控制面调用不传代理时沿用显式直连，不能改成跟随系统或网关默认代理。
pub async fn send_raw(
    method: &str,
    url: &str,
    body: Option<&Value>,
    headers: &[(String, String)],
    proxy: Option<&proto::WireProxy>,
    timeout_ms: Option<u64>,
) -> Result<ApiResponse, reqwest::Error> {
    let client = match proxy {
        Some(proxy) => client(Some(proxy), ""),
        None => client(None, "direct"),
    };
    let mut builder = match method {
        "POST" => client.post(url),
        "PUT" => client.put(url),
        "DELETE" => client.delete(url),
        "PATCH" => client.patch(url),
        _ => client.get(url),
    };
    builder = builder.header("Accept", "application/json");
    for (key, value) in headers {
        builder = builder.header(key.as_str(), value.as_str());
    }
    if let Some(payload) = body {
        builder = builder
            .header("Content-Type", "application/json")
            .body(payload.to_string());
    }
    if let Some(timeout) = timeout_ms {
        builder = builder.timeout(Duration::from_millis(timeout));
    }
    let response = builder.send().await?;
    let status = response.status().as_u16();
    let ok = response.status().is_success();
    let text = response.text().await?;
    let payload = if text.trim().is_empty() {
        None
    } else {
        serde_json::from_str::<Value>(&text).ok()
    };
    Ok(ApiResponse { status, ok, payload })
}

pub fn describe_chain(error: &reqwest::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    let head = error.to_string();
    if !head.is_empty() {
        parts.push(head);
    }
    let mut current: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(error);
    while let Some(source) = current {
        let text = source.to_string();
        if !text.is_empty() && !parts.iter().any(|part| part == &text) {
            parts.push(text);
        }
        current = source.source();
    }
    if parts.is_empty() {
        return "未知错误".to_string();
    }
    parts.join(" → ")
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}
