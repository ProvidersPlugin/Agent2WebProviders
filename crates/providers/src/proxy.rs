use agent2api_provider_protocol::WireProxy;
use agent2api_provider_sdk::{StoreAccount, StoreProxy};
use serde_json::Value;

use crate::errors::GatewayError;

pub fn account_config(account: &StoreAccount) -> Value {
    match &account.proxy {
        Some(StoreProxy::Unset) => Value::Null,
        Some(StoreProxy::Resolved { proxy }) => serde_json::json!(proxy),
        Some(StoreProxy::Static { config }) => config.clone(),
        Some(StoreProxy::Failed { .. }) => serde_json::json!({ "error": "账号代理不可用" }),
        None => account.secret.get("proxy").cloned().unwrap_or(Value::Null),
    }
}

pub fn account_proxy(config: Option<&Value>) -> Result<Option<WireProxy>, GatewayError> {
    let Some(config) = config.filter(|value| !value.is_null()) else { return Ok(None) };
    if let Some(inner) = config.get("config") { return account_proxy(Some(inner)); }
    if config.get("source").and_then(Value::as_str) == Some("direct") { return Ok(None); }
    if config.get("error").is_some()
        || matches!(config.get("source").and_then(Value::as_str), Some("pool" | "rotate" | "clash" | "system"))
    {
        return Err(unavailable());
    }
    let proxy = if config.get("host").and_then(Value::as_str).is_some_and(|host| !host.trim().is_empty()) {
        let mut value = config.clone();
        let fields = value.as_object_mut().ok_or_else(unavailable)?;
        fields.entry("protocol").or_insert_with(|| Value::String("http".to_string()));
        if let Some(Value::String(port)) = fields.get("port") {
            let port = port.parse::<u16>().ok().filter(|port| *port > 0).ok_or_else(unavailable)?;
            fields.insert("port".to_string(), Value::from(port));
        }
        serde_json::from_value::<WireProxy>(value).map_err(|_| unavailable())?
    } else {
        let text = ["url", "proxy", "address", "server"].iter()
            .find_map(|key| config.get(key).and_then(Value::as_str).filter(|text| !text.trim().is_empty()))
            .ok_or_else(unavailable)?;
        let url = url::Url::parse(text.trim()).map_err(|_| unavailable())?;
        if !matches!(url.path(), "" | "/") || url.query().is_some() || url.fragment().is_some() {
            return Err(unavailable());
        }
        let decode = |text: &str| percent_encoding::percent_decode_str(text).decode_utf8()
            .map(|text| text.into_owned()).map_err(|_| unavailable());
        WireProxy {
            protocol: url.scheme().to_string(),
            host: url.host_str().ok_or_else(unavailable)?.trim_matches(|ch| ch == '[' || ch == ']').to_string(),
            port: url.port(),
            username: decode(url.username())?,
            password: decode(url.password().unwrap_or(""))?,
        }
    };
    if !matches!(proxy.protocol.as_str(), "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h")
        || proxy.host.trim().is_empty() || proxy.port == Some(0)
    {
        return Err(unavailable());
    }
    let url = url::Url::parse(&proxy_url(&proxy)).map_err(|_| unavailable())?;
    if url.host_str().is_none() || !matches!(url.path(), "" | "/")
        || url.query().is_some() || url.fragment().is_some()
    {
        return Err(unavailable());
    }
    Ok(Some(proxy))
}

pub fn proxy_url(proxy: &WireProxy) -> String {
    let mut proxy = proxy.clone();
    if proxy.protocol == "socks5" { proxy.protocol = "socks5h".to_string(); }
    if proxy.host.contains(':') && !proxy.host.starts_with('[') {
        proxy.host = format!("[{}]", proxy.host);
    }
    proxy.to_url()
}

fn unavailable() -> GatewayError {
    GatewayError::with_status(502, "账号代理不可用，请检查账号设置；本次操作未改走直连")
        .with_code("account_proxy_unavailable")
}
