use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use agent2api_provider_protocol as proto;
use serde_json::{json, Value};

pub const MODEL_ID: &str = "deepseek-web-flash";
const CACHE_MS: i64 = 5 * 60 * 1000;

#[derive(Clone)]
pub struct Catalog {
    pub list: proto::ModelList,
    pub mode: String,
    pub images: bool,
}

fn cache() -> &'static Mutex<BTreeMap<String, Catalog>> {
    static CACHE: OnceLock<Mutex<BTreeMap<String, Catalog>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

pub fn fallback() -> Vec<Value> {
    vec![json!({
        "id": MODEL_ID, "name": "DeepSeek 网页", "isDefault": true,
        "supportsImages": true, "supportsToolCall": false, "supportsReasoning": true,
        "inputModalities": ["text", "image"], "synthetic": true,
    })]
}

pub fn accepts(id: &str) -> bool {
    matches!(id, "" | MODEL_ID | "deepseek-web" | "deepseek-web-pro")
}

pub async fn load(account: &str, client: &reqwest::Client, headers: &[(String, String)], force: bool) -> Result<Catalog, String> {
    let now = crate::http::now_ms();
    if !force {
        let found = cache().lock().map_err(|_| "DeepSeek 模型缓存不可用")?.get(account).cloned();
        if let Some(found) = found.filter(|found| now - found.list.meta.refreshed_at < CACHE_MS) {
            return Ok(found);
        }
    }
    let request_id = super::id("catalog");
    let response = super::get(client, "/api/v0/client/settings", headers, &[
        ("scope", "model"), ("did", &request_id),
    ]).await?;
    let entries = response.pointer("/data/biz_data/settings/model_configs/value")
        .and_then(Value::as_array).ok_or("DeepSeek 未返回网页模型配置")?;
    let enabled: Vec<&Value> = entries.iter().filter(|entry| {
        entry["enabled"].as_bool() == Some(true) && entry["switchable"].as_bool() == Some(true)
    }).collect();
    let selected = enabled.iter().copied().find(|entry| entry["is_default"].as_bool() == Some(true))
        .or_else(|| enabled.first().copied()).ok_or("DeepSeek 当前账号没有可用的网页模型")?;
    let mode = selected["model_type"].as_str().filter(|mode| {
        !mode.is_empty() && mode.len() <= 64 && mode.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    }).ok_or("DeepSeek 网页模型标识无效")?.to_string();
    let images = selected.pointer("/file_feature/vision").and_then(Value::as_bool) == Some(true);
    let reasoning = selected.get("think_feature").is_some_and(Value::is_object);
    let catalog = Catalog {
        list: proto::ModelList {
            models: vec![json!({
                "id": MODEL_ID, "name": "DeepSeek 网页", "isDefault": true,
                "supportsImages": images, "supportsToolCall": false, "supportsReasoning": reasoning,
                "inputModalities": if images { vec!["text", "image"] } else { vec!["text"] },
                "webModelType": mode, "synthetic": true,
            })],
            meta: proto::CatalogMeta { remote: true, refreshed_at: crate::http::now_ms() },
        },
        mode,
        images,
    };
    let mut state = cache().lock().map_err(|_| "DeepSeek 模型缓存不可用")?;
    if state.len() >= 128 && !state.contains_key(account) {
        state.retain(|_, entry| now - entry.list.meta.refreshed_at < CACHE_MS);
        if state.len() >= 128 {
            if let Some(oldest) = state.iter().min_by_key(|(_, entry)| entry.list.meta.refreshed_at).map(|(id, _)| id.clone()) {
                state.remove(&oldest);
            }
        }
    }
    state.insert(account.to_string(), catalog.clone());
    Ok(catalog)
}
