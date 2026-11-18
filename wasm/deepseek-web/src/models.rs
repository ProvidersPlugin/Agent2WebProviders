//! 每个账号保存自己的网页配置；成功空目录与读取失败明确区分。
//! 只展示原有网页入口，不把已停用的专家名称当成另一额度池。

use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    AccountLock, Error, Result, Value,
};

use crate::{account::Session, network, util};

pub const MODEL_ID: &str = "deepseek-web-flash";
const KEY: &str = "deepseek.catalog";
const TTL: i64 = 300_000;

pub struct Catalog {
    pub list: Value,
    pub mode: Option<String>,
    pub images: bool,
    pub reasoning: bool,
}

fn entry(mode: Option<&str>, images: bool, reasoning: bool) -> Value {
    let mut value = json!({ "id": MODEL_ID, "name": "DeepSeek 网页", "isDefault": true,
        "supportsImages": images, "supportsToolCall": false, "supportsReasoning": reasoning,
        "toolEmulation": { "protocol": "tagged_json_v1", "maxPromptChars": 200000 },
        "inputModalities": if images { vec!["text", "image"] } else { vec!["text"] }, "synthetic": true });
    if reasoning {
        value["reasoningEffortLevels"] = json!(["none", "high"]);
    }
    if let Some(mode) = mode {
        value["webModelType"] = json!(mode);
    }
    value
}

fn entries(mode: &str, images: bool, reasoning: bool) -> Vec<Value> {
    let main = entry(Some(mode), images, reasoning);
    let mut result = vec![main.clone()];
    for id in ["deepseek-web", "deepseek-web-pro"] {
        let mut alias = main.clone();
        alias["id"] = json!(id);
        alias["name"] = json!(format!("DeepSeek 网页（兼容名称：{id}）"));
        alias["isDefault"] = json!(false);
        alias["upstreamModel"] = json!(MODEL_ID);
        alias["compatibilityAlias"] = json!(true);
        result.push(alias);
    }
    result
}

pub fn fallback() -> Value {
    json!({ "models": [entry(None, true, true)], "meta": { "remote": false, "refreshed_at": 0 } })
}

pub fn accepts(id: &str) -> bool {
    matches!(id, "" | MODEL_ID | "deepseek-web" | "deepseek-web-pro")
}

fn valid_mode(mode: &str) -> bool {
    !mode.is_empty()
        && mode.len() <= 64
        && mode
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn displayed(account: bool) -> Result<Value> {
    // catalog.accounts.v1 下不能从其它账号或全局展示目录借用真实模型权限。
    if !account {
        return Ok(fallback());
    }
    let value = sdk::catalog::cached()?;
    if value["meta"]["remote"] == true && value["models"].as_array().is_some() {
        return Ok(value);
    }
    Ok(fallback())
}

pub fn load(session: &Session, force: bool) -> Result<Catalog> {
    let _lock = AccountLock::acquire()?;
    load_locked(session, force)
}

/// 对话持有同一账号锁至原流结束，目录读取不能重复申请锁。
pub fn load_locked(session: &Session, force: bool) -> Result<Catalog> {
    session.check()?;
    let now = util::now()?;
    let binding = session.binding();
    let snapshot = state::get(Scope::Account, KEY, 2)?;
    let value = &snapshot.value;
    let age = now.saturating_sub(value["list"]["meta"]["refreshed_at"].as_i64().unwrap_or(0));
    if !force
        && snapshot.compatible
        && value["binding"] == binding
        && (0..TTL).contains(&age)
        && value["images"].is_boolean()
        && value["reasoning"].is_boolean()
        && value["list"]["meta"]["remote"] == true
    {
        if let Some(models) = value["list"]["models"].as_array() {
            let mode = value["mode"].as_str().filter(|mode| valid_mode(mode));
            if models.is_empty() && mode.is_none()
                || models.len() == 3 && models[0]["id"] == MODEL_ID && mode.is_some()
            {
                return Ok(Catalog {
                    list: value["list"].clone(),
                    mode: mode.map(str::to_string),
                    images: value["images"] == true,
                    reasoning: value["reasoning"] == true,
                });
            }
        }
    }
    let request_id = util::id("catalog")?;
    let response = network::get(
        session,
        &format!(
            "/api/v0/client/settings?scope=model&did={}",
            util::query(&request_id),
        ),
    )?;
    let raw = response
        .pointer("/data/biz_data/settings/model_configs/value")
        .ok_or_else(|| Error::http(502, "DeepSeek 未返回网页模型配置"))?;
    // 官方设置可能把 JSON 放在 value 字符串内；不把错型数据认作空清单。
    let decoded;
    let value = if let Some(text) = raw.as_str() {
        decoded = serde_json::from_str::<Value>(text)
            .map_err(|_| Error::http(502, "DeepSeek 网页模型配置不是有效 JSON"))?;
        &decoded
    } else {
        raw
    };
    let entries = value
        .as_array()
        .filter(|entries| entries.len() <= 256)
        .ok_or_else(|| Error::http(502, "DeepSeek 网页模型配置不是有效清单"))?;
    if entries.iter().any(|entry| {
        !entry.is_object()
            || !entry["enabled"].is_boolean()
            || entry
                .get("switchable")
                .is_some_and(|value| !value.is_boolean())
    }) {
        return Err(Error::http(
            502,
            "DeepSeek 网页模型配置字段已变化，保留旧目录",
        ));
    }
    let enabled: Vec<&Value> = entries
        .iter()
        .filter(|entry| {
            entry["enabled"] == true && (entry["switchable"] == true || entry["is_default"] == true)
        })
        .collect();
    let selected = enabled
        .iter()
        .copied()
        .find(|entry| entry["is_default"] == true)
        .or_else(|| enabled.first().copied());
    let (mode, images, reasoning) = match selected {
        Some(selected) => {
            let mode = selected["model_type"]
                .as_str()
                .filter(|mode| valid_mode(mode))
                .ok_or_else(|| Error::http(502, "DeepSeek 网页模型标识无效"))?;
            (
                Some(mode.to_string()),
                selected
                    .pointer("/file_feature/vision")
                    .and_then(Value::as_bool)
                    == Some(true),
                selected.get("think_feature").is_some_and(Value::is_object),
            )
        }
        None => (None, false, false),
    };
    let models = mode
        .as_deref()
        .map(|mode| self::entries(mode, images, reasoning))
        .unwrap_or_default();
    let list =
        json!({ "models": models, "meta": { "remote": true, "refreshed_at": util::now()? } });
    session.check()?;
    if !state::compare_exchange(
        Scope::Account,
        KEY,
        2,
        snapshot.revision,
        json!({ "binding": binding, "mode": mode, "images": images, "reasoning": reasoning, "list": list }),
        0,
    )? {
        return Err(Error::http(
            409,
            "DeepSeek 模型缓存已被其他操作更新，请重试",
        ));
    }
    session.check()?;
    Ok(Catalog {
        list,
        mode,
        images,
        reasoning,
    })
}

pub fn list(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        return displayed(false);
    }
    Ok(load(&Session::load()?, false)?.list)
}

pub fn refresh(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        return Ok(sdk::catalog::failed(
            "DeepSeek 网页目录需要当前账号，未借用其他账号的配置",
        ));
    }
    let session = Session::load()?;
    match load(&session, params["force"].as_bool().unwrap_or(false)) {
        Ok(catalog) => {
            session.check()?;
            let mut list = catalog.list;
            list["count"] = json!(list["models"].as_array().map(Vec::len).unwrap_or(0));
            // 账号快照不存在时即使命中插件缓存，也要让宿主保存本次真实目录。
            list["refreshed"] = json!(true);
            Ok(list)
        }
        Err(error)
            if matches!(
                error.code.as_deref(),
                Some("aborted" | "plugin_unavailable" | "deepseek_account_changed")
            ) =>
        {
            Err(error)
        }
        Err(error) => {
            session.check()?;
            // 失败不返回全局清单、不写成功快照，原宿主和本账号缓存保持不变。
            let mut value = sdk::catalog::failed(&session.redact(&error.message));
            value["count"] = json!(sdk::catalog::cached()?["models"]
                .as_array()
                .map(Vec::len)
                .unwrap_or(0));
            Ok(value)
        }
    }
}
