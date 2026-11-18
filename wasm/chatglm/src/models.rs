//! 在线 available_models 按账号保存；展示目录不替代上游的模型使用资格判断。

use crate::{account, endpoints, errors, memory, sign};
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};
use sha2::{Digest, Sha256};

const KEY: &str = "chatglm.catalog";
const TTL_MS: i64 = 600_000;
const LEVELS: [&str; 3] = ["快速", "深度", "极致"];

fn efforts() -> Value {
    json!([{ "effort": "", "am_effort": "low", "display_name": "快速" },
        { "effort": "thinking", "am_effort": "high", "display_name": "深度" },
        { "effort": "deep_thinking", "am_effort": "max", "display_name": "极致" }])
}

fn levels(record: &mut Value, efforts: &Value) {
    let mut levels = Vec::<String>::new();
    for item in efforts.as_array().into_iter().flatten() {
        let label = if let Some(value) = item.as_str() {
            value.trim().to_string()
        } else {
            match item["effort"].as_str().unwrap_or("").trim() {
                "" => LEVELS[0].into(),
                "thinking" => LEVELS[1].into(),
                "deep_thinking" => LEVELS[2].into(),
                other => {
                    let name = errors::text(item, &["display_name"]);
                    if name.is_empty() {
                        other.to_string()
                    } else {
                        name
                    }
                }
            }
        };
        if !label.is_empty() && !levels.contains(&label) {
            levels.push(label);
        }
    }
    if levels.is_empty() {
        levels = LEVELS.iter().map(|value| value.to_string()).collect();
    }
    let default = levels
        .iter()
        .find(|value| value.as_str() == "深度")
        .or_else(|| levels.get(1))
        .or_else(|| levels.first())
        .cloned();
    record["reasoningLevels"] = json!(levels);
    if let Some(default) = default {
        record["reasoningDefaultLevel"] = json!(default);
    }
}

fn record(id: &str, name: &str, efforts: Value) -> Value {
    let mut model = json!({ "id": id, "name": name, "description": "", "reasoningEfforts": efforts,
        "supportsImages": false, "supportsToolCall": false, "supportsReasoning": true, "inputModalities": ["text"],
        "toolEmulation": { "protocol": "tagged_json_v1", "maxPromptChars": 190000 } });
    levels(&mut model, &efforts);
    model
}

pub fn fallback() -> Value {
    json!({ "models": [record("glm-5.3", "GLM-5.3", efforts()), record("glm-5.3-flash", "GLM-Flash", efforts())],
        "meta": { "remote": false, "refreshed_at": 0 } })
}

pub fn cached() -> Result<Value> {
    let mut value = sdk::catalog::cached()?;
    if value["models"].is_array()
        && (value["meta"]["remote"] == true
            || value["models"]
                .as_array()
                .is_some_and(|models| !models.is_empty()))
    {
        let entries = value["models"]
            .as_array()
            .ok_or_else(|| Error::http(502, "智谱缓存目录格式无效"))?;
        value["models"] = json!(normalize(entries)?);
        Ok(value)
    } else {
        Ok(fallback())
    }
}

fn normalize(entries: &[Value]) -> Result<Vec<Value>> {
    if entries.len() > 1024 {
        return Err(Error::http(502, "智谱模型目录条数超过限制"));
    }
    let mut models = Vec::new();
    let mut ids = std::collections::BTreeSet::new();
    for item in entries {
        let id = errors::text(item, &["id", "selected_model"]);
        if id.is_empty()
            || id.len() > 512
            || id.chars().any(char::is_control)
            || !item.is_object()
            || !ids.insert(id.to_ascii_lowercase())
        {
            return Err(Error::http(
                502,
                "智谱模型条目缺少有效唯一标识，保留现有目录",
            ));
        }
        let name = errors::text(item, &["name", "display_name"]);
        let effort = item
            .get("reasoningEfforts")
            .or_else(|| item.get("reasoning_efforts"))
            .filter(|value| value.is_array())
            .cloned()
            .unwrap_or_else(efforts);
        let mut model = record(&id, if name.is_empty() { &id } else { &name }, effort);
        model["description"] = json!(errors::text(item, &["description"]));
        for key in ["display_tag", "chat_switch", "am_switch", "am_description"] {
            if let Some(value) = item.get(key) {
                model[key] = value.clone();
            }
        }
        models.push(model);
    }
    Ok(models)
}

fn parse(value: &Value) -> Result<Vec<Value>> {
    let entries = value["models"]
        .as_array()
        .ok_or_else(|| Error::http(502, "智谱模型目录缺少 models 数组，保留现有清单"))?;
    normalize(entries)
}

pub fn refresh(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        return Ok(sdk::catalog::unchanged(0));
    }
    let _lock = AccountLock::acquire()?;
    let session = account::ready_locked()?;
    let binding = format!(
        "{:x}",
        Sha256::digest(format!("{}\0{}", session.device, session.token))
    );
    if !params["force"].as_bool().unwrap_or(false) {
        if let Some(value) = memory::get(KEY)? {
            let age = sign::now()? - value["list"]["meta"]["refreshed_at"].as_i64().unwrap_or(0);
            if value["binding"] == binding && (0..TTL_MS).contains(&age) {
                session.check_current()?;
                let mut list = value["list"].clone();
                let current = cached()?;
                list["refreshed"] =
                    json!(current["models"] != list["models"] || current["meta"] != list["meta"]);
                list["count"] = json!(list["models"].as_array().map(Vec::len).unwrap_or(0));
                return Ok(list);
            }
        }
    }
    let result = session.get(endpoints::MODELS);
    let payload = match result {
        Ok(value) => value,
        Err(error) => return Ok(sdk::catalog::failed(&session.redact(&error.message))),
    };
    let models = match parse(&payload) {
        Ok(models) => models,
        Err(error) => return Ok(sdk::catalog::failed(&session.redact(&error.message))),
    };
    session.check_current()?;
    let list =
        json!({ "models": models, "meta": { "remote": true, "refreshed_at": sign::now()? } });
    memory::put(KEY, json!({ "binding": binding, "list": list }))?;
    let mut result = list;
    result["refreshed"] = json!(true);
    result["count"] = json!(models.len());
    Ok(result)
}
