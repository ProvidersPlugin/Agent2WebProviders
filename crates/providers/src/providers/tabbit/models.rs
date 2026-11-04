//! Tabbit 模型目录。
//!
//! 上游 `GET /proxy/v1/model_config/models?a=0&scene=chat` 返回
//! `{"models":[{display_name, model_access_type, supports_images, support_thinking,
//! display_multiplier, params…}]}`（部分站点包一层 `data`）。
//!
//! ── 两条必须遵守的口径 ──────────────────────────────────────
//! 1. 上游 `selected_model` 只认 **display_name**：发我们对外用的 slug 会被
//!    静默换成别的模型（不报错，答非所问）。所以对外 id 是 slug，发上游时
//!    一律换回 `upstreamModel`。
//! 2. 本适配器只做纯文本：`supportsImages` 与 `supportsToolCall` 恒 false。
//!    网页 completion 没有原生工具调用契约，按 Web 包的口径明确拒绝，
//!    不用提示词模拟（与 DeepSeek 网页一致）。

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};
use agent2api_provider_protocol as proto;

use crate::errors::GatewayError;

use super::client::{self, Credentials};
use super::config::{Realm, CATALOG_TTL_MS, DEFAULT_MODEL};

#[derive(Clone)]
pub struct Catalog {
    pub list: proto::ModelList,
}

fn cache() -> &'static Mutex<BTreeMap<String, (Catalog, i64)>> {
    static CACHE: OnceLock<Mutex<BTreeMap<String, (Catalog, i64)>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

/// 目录拉不到时的兜底清单（网关 `models.list` 也要能返回一份）。
pub fn fallback_catalog(realm: Realm) -> Catalog {
    Catalog {
        list: proto::ModelList {
            models: fallback_for(realm),
            meta: proto::CatalogMeta::default(),
        },
    }
}

/// 目录拉不到时的兜底：只给一条「让上游选默认模型」的入口，不编造模型名。
pub fn fallback_for(realm: Realm) -> Vec<Value> {
    vec![json!({
        "id": format!("{}-default", realm.id()),
        "name": "Default（Tabbit 默认模型）",
        "upstreamModel": DEFAULT_MODEL,
        "isDefault": true,
        "supportsImages": false,
        "supportsToolCall": false,
        "supportsReasoning": true,
        "inputModalities": ["text"],
        "synthetic": true,
    })]
}

fn pick_number(value: &Value, keys: &[&str]) -> Option<i64> {
    for key in keys {
        if let Some(number) = client::as_i64(value.get(*key).unwrap_or(&Value::Null)) {
            if number > 0 {
                return Some(number);
            }
        }
        if let Some(params) = value.get("params") {
            if let Some(number) = client::as_i64(params.get(*key).unwrap_or(&Value::Null)) {
                if number > 0 {
                    return Some(number);
                }
            }
        }
    }
    None
}

fn flag(value: &Value, keys: &[&str]) -> bool {
    keys.iter().any(|key| {
        value
            .get(*key)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    })
}

fn text_of(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|text| !text.is_empty())
        .unwrap_or("")
        .to_string()
}

/// 展示名 → 对外 id：`GPT-5.5` → `gpt-5-5`，`Claude-Opus-4.7` → `claude-opus-4-7`。
fn slug(name: &str) -> String {
    let mut out = String::new();
    for ch in name.trim().to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if matches!(ch, ' ' | '.' | '_' | '-' | '/') {
            if !out.ends_with('-') {
                out.push('-');
            }
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        DEFAULT_MODEL.to_string()
    } else {
        trimmed
    }
}

fn normalize_key(value: &str) -> String {
    value
        .trim()
        .to_lowercase()
        .chars()
        .filter(|ch| !matches!(ch, ' ' | '_' | '-' | '.'))
        .collect()
}

fn parse_catalog(value: &Value, realm: Realm) -> Result<Catalog, GatewayError> {
    let mut raw = value
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if raw.is_empty() {
        raw = value
            .get("data")
            .and_then(|data| data.get("models"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
    }
    let mut seen = BTreeMap::new();
    let mut models = Vec::new();
    for entry in &raw {
        let name = text_of(entry, &["display_name", "displayName", "model_name", "name"]);
        if name.is_empty() {
            continue;
        }
        let id = slug(&name);
        if seen.contains_key(&id) {
            continue;
        }
        seen.insert(id.clone(), true);
        let access = text_of(entry, &["model_access_type", "access_type"]);
        let multiplier = entry
            .get("display_multiplier")
            .and_then(client::as_f64)
            .filter(|value| *value > 0.0)
            .map(|value| format!("x{}", trim_number(value)));
        let mut record = json!({
            "id": id,
            "name": name,
            "isDefault": false,
            "supportsImages": false,
            "supportsToolCall": false,
            "supportsReasoning": flag(entry, &["support_thinking", "supports_thinking", "support_reasoning"]),
            "inputModalities": ["text"],
            "upstreamModel": name,
            "accessType": access,
            "realm": realm.id(),
            "synthetic": false,
        });
        if let Some(context) = pick_number(entry, &["context_window", "contextWindow", "max_context_tokens"]) {
            record["contextWindow"] = Value::from(context);
        }
        if let Some(max_output) = pick_number(entry, &["max_tokens", "maxTokens", "max_output_tokens"]) {
            record["maxOutput"] = Value::from(max_output);
        }
        if let Some(multiplier) = multiplier {
            record["creditMultiplier"] = Value::String(multiplier);
        }
        models.push(record);
    }
    if models.is_empty() {
        return Err(GatewayError::with_status(502, "Tabbit 模型目录为空"));
    }
    // 默认模型：第一个免费模型；没有免费档就第一个。
    let default_index = models
        .iter()
        .position(|model| {
            model["accessType"]
                .as_str()
                .is_some_and(|access| access.eq_ignore_ascii_case("free_unlimited"))
        })
        .unwrap_or(0);
    if let Some(default) = models.get_mut(default_index) {
        default["isDefault"] = Value::Bool(true);
    }
    Ok(Catalog {
        list: proto::ModelList {
            models,
            meta: proto::CatalogMeta { remote: true, refreshed_at: crate::http::now_ms() },
        },
    })
}

fn trim_number(value: f64) -> String {
    let text = format!("{value:.2}");
    let text = text.trim_end_matches('0').trim_end_matches('.').to_string();
    if text.is_empty() {
        "0".to_string()
    } else {
        text
    }
}

pub async fn load(
    account: &str,
    creds: &Credentials,
    realm: Realm,
    client: &reqwest::Client,
    force: bool,
) -> Result<Catalog, GatewayError> {
    let now = crate::http::now_ms();
    if !force {
        if let Ok(guard) = cache().lock() {
            if let Some((catalog, at)) = guard.get(account) {
                if now - *at < CATALOG_TTL_MS {
                    return Ok(catalog.clone());
                }
            }
        }
    }
    let value = client::signed_json(
        creds,
        realm,
        client,
        "GET",
        "/proxy/v1/model_config/models?a=0&scene=chat",
        None,
        "/newtab",
    )
    .await?;
    let catalog = parse_catalog(&value, realm)?;
    if let Ok(mut guard) = cache().lock() {
        if guard.len() >= 128 && !guard.contains_key(account) {
            guard.retain(|_, (_, at)| now - *at < CATALOG_TTL_MS);
            if guard.len() >= 128 {
                if let Some(oldest) = guard.iter().min_by_key(|(_, (_, at))| *at).map(|(id, _)| id.clone()) {
                    guard.remove(&oldest);
                }
            }
        }
        guard.insert(account.to_string(), (catalog.clone(), now));
    }
    Ok(catalog)
}

pub fn resolve(catalog: &Catalog, requested: &str) -> Option<String> {
    let want = normalize_key(requested);
    if want.is_empty() {
        return None;
    }
    for model in &catalog.list.models {
        let id = model.get("id").and_then(Value::as_str).unwrap_or("");
        let name = model.get("name").and_then(Value::as_str).unwrap_or("");
        let upstream = model
            .get("upstreamModel")
            .and_then(Value::as_str)
            .unwrap_or("");
        if normalize_key(id) == want
            || normalize_key(name) == want
            || normalize_key(upstream) == want
        {
            return Some(if upstream.is_empty() { name } else { upstream }.to_string());
        }
    }
    None
}

/// 客户端请求的名字 → 上游 `selected_model`（display_name）。
pub fn select(catalog: &Catalog, requested: &str) -> Result<String, GatewayError> {
    if requested.trim().is_empty()
        || requested.eq_ignore_ascii_case(DEFAULT_MODEL)
        || requested.ends_with("-default")
    {
        for model in &catalog.list.models {
            if model.get("isDefault").and_then(Value::as_bool) == Some(true) {
                if let Some(upstream) = model.get("upstreamModel").and_then(Value::as_str) {
                    if !upstream.is_empty() {
                        return Ok(upstream.to_string());
                    }
                }
            }
        }
        return Ok(DEFAULT_MODEL.to_string());
    }
    resolve(catalog, requested).ok_or_else(|| {
        GatewayError::with_status(
            400,
            format!("未知的 Tabbit 模型「{requested}」，请先刷新模型目录"),
        )
    })
}

