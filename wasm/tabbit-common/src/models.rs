//! 这是站点展示目录，不是账号授权白名单；两站快照均按账号隔离。
//! 对外 id 仍是原生版的 slug，上游 selected_model 必须使用 display_name。

use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};
use std::collections::BTreeSet;

use crate::{
    network::Client,
    state,
    usage::{integer, number},
    Site, CATALOG_TTL_MS,
};

const KEY: &str = "tabbit.catalog";

pub fn fallback(site: Site) -> Value {
    json!({ "models": [{ "id": format!("{}-default", site.id), "name": "Default（Tabbit 默认模型）",
        "upstreamModel": "default", "isDefault": true, "supportsImages": false,
        "supportsToolCall": false, "toolEmulation": { "protocol": "tagged_json_v1", "maxPromptChars": 16000 },
        "supportsReasoning": true, "inputModalities": ["text"], "synthetic": true }],
        "meta": { "remote": false, "refreshed_at": 0 } })
}

fn account_previous(client: &Client) -> Result<Value> {
    if let Some(list) = state::get("tabbit.catalog.last", &client.binding)?.filter(valid) {
        return Ok(normalize(list));
    }
    cached(client.site)
}

fn text(value: &Value, keys: &[&str]) -> String {
    crate::credentials::text(value, keys)
}

fn positive(value: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|key| {
        integer(&value[*key])
            .filter(|number| *number > 0)
            .or_else(|| integer(&value["params"][*key]).filter(|number| *number > 0))
    })
}

fn slug(name: &str) -> String {
    let mut value = String::new();
    for ch in name.trim().to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            value.push(ch);
        } else if matches!(ch, ' ' | '.' | '_' | '-' | '/') && !value.ends_with('-') {
            value.push('-');
        }
    }
    let value = value.trim_matches('-');
    if value.is_empty() {
        "default".to_string()
    } else {
        value.to_string()
    }
}

fn normalized(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .chars()
        .filter(|ch| !matches!(*ch, ' ' | '_' | '-' | '.'))
        .collect()
}

fn parse(value: &Value, site: Site) -> Result<Value> {
    if value
        .get("success")
        .is_some_and(|value| value.as_bool() != Some(true))
        || value
            .get("code")
            .is_some_and(|value| integer(value) != Some(0))
        || ["error", "error_message"]
            .iter()
            .any(|key| value.get(*key).is_some_and(|value| !value.is_null()))
    {
        return Err(Error::http(
            502,
            "Tabbit 模型查询没有返回成功结果，已保留旧清单",
        ));
    }
    let entries = value["models"]
        .as_array()
        .or_else(|| value["data"]["models"].as_array())
        .ok_or_else(|| Error::http(502, "Tabbit 未返回模型目录"))?;
    if entries.len() > 1024 {
        return Err(Error::http(
            502,
            "Tabbit 模型目录超过条目上限，未截断为完整清单",
        ));
    }
    let mut seen = BTreeSet::new();
    let mut models = Vec::new();
    for entry in entries {
        if !entry.is_object() {
            return Err(Error::http(502, "Tabbit 模型条目格式无效，已保留旧清单"));
        }
        let name = text(
            entry,
            &["display_name", "displayName", "model_name", "name"],
        );
        if name.is_empty() || name.len() > 512 || name.chars().any(char::is_control) {
            return Err(Error::http(502, "Tabbit 模型名称无效，已保留旧清单"));
        }
        let id = slug(&name);
        if !seen.insert(id.clone()) {
            return Err(Error::http(
                502,
                "Tabbit 模型名称归一化后冲突，未合并不同模型",
            ));
        }
        let supports_reasoning = ["support_thinking", "supports_thinking", "support_reasoning"]
            .iter()
            .any(|key| entry[*key] == true);
        let mut model = json!({ "id": id, "name": name, "upstreamModel": name,
            "isDefault": false, "supportsImages": false, "supportsToolCall": false,
            "toolEmulation": { "protocol": "tagged_json_v1", "maxPromptChars": 16000 },
            "supportsReasoning": supports_reasoning,
            "inputModalities": ["text"], "accessType": text(entry, &["model_access_type", "access_type"]),
            "realm": site.id, "synthetic": false });
        if entry["hidden"] == true || entry["display_enabled"] == false {
            model["visibility"] = json!("hidden");
            model["display_enabled"] = json!(false);
        }
        if let Some(context) = positive(
            entry,
            &["context_window", "contextWindow", "max_context_tokens"],
        ) {
            model["contextWindow"] = json!(context);
        }
        if let Some(output) = positive(entry, &["max_tokens", "maxTokens", "max_output_tokens"]) {
            model["maxOutput"] = json!(output);
        }
        if let Some(multiplier) =
            number(&entry["display_multiplier"]).filter(|number| *number > 0.0)
        {
            let value = format!("{multiplier:.2}");
            model["creditMultiplier"] = json!(format!(
                "x{}",
                value.trim_end_matches('0').trim_end_matches('.')
            ));
        }
        models.push(model);
    }
    let default = models
        .iter()
        .position(|model| {
            model["display_enabled"] != false
                && model["accessType"]
                    .as_str()
                    .is_some_and(|access| access.eq_ignore_ascii_case("free_unlimited"))
        })
        .unwrap_or(0);
    if let Some(model) = models.get_mut(default) {
        model["isDefault"] = json!(true);
    }
    Ok(json!({ "models": models, "meta": { "remote": true, "refreshed_at": crate::now()? } }))
}

fn valid(list: &Value) -> bool {
    list.to_string().len() <= 240 * 1024
        && list["models"].as_array().is_some_and(|models| {
            models.len() <= 1024
                && models.iter().all(|model| {
                    model["id"]
                        .as_str()
                        .is_some_and(|id| !id.is_empty() && id.len() <= 512)
                        && model["upstreamModel"]
                            .as_str()
                            .is_some_and(|name| !name.is_empty() && name.len() <= 512)
                })
        })
        && list["meta"].is_object()
}

fn normalize(mut list: Value) -> Value {
    for model in list["models"].as_array_mut().into_iter().flatten() {
        model["supportsToolCall"] = json!(false);
        model["toolEmulation"] = json!({ "protocol": "tagged_json_v1", "maxPromptChars": 16000 });
    }
    list
}

fn cached(site: Site) -> Result<Value> {
    let list = sdk::catalog::cached()?;
    Ok(if valid(&list) {
        normalize(list)
    } else {
        fallback(site)
    })
}

pub fn load(client: &mut Client, force: bool) -> Result<(Value, bool)> {
    if !force {
        if let Some(list) = state::get(KEY, &client.binding)?.filter(valid) {
            client.current()?;
            return Ok((normalize(list), false));
        }
    }
    let response = client.signed_json(
        "GET",
        "/proxy/v1/model_config/models?a=0&scene=chat",
        None,
        "/newtab",
    );
    let response = match response {
        Ok(response) => response,
        Err(error) if error.code.as_deref() == Some("tabbit_account_changed") => return Err(error),
        Err(error) => {
            if let Some(list) = state::get("tabbit.catalog.last", &client.binding)?.filter(valid) {
                client.current()?;
                if !force {
                    return Ok((normalize(list), false));
                }
            }
            return Err(error);
        }
    };
    let list = parse(&response, client.site)?;
    if !valid(&list) {
        return Err(Error::http(
            502,
            "Tabbit 目录超过宿主快照容量，未采用半份清单",
        ));
    }
    client.current()?;
    state::put(KEY, &client.binding, list.clone(), CATALOG_TTL_MS)?;
    state::put(
        "tabbit.catalog.last",
        &client.binding,
        list.clone(),
        365 * 24 * 60 * 60 * 1000,
    )?;
    Ok((list, true))
}

pub fn list(site: Site, params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        // catalog.get 在两站均绑定账号，握手或无账号查询不能借另一账号快照。
        return Ok(fallback(site));
    }
    let _lock = AccountLock::acquire()?;
    let mut client = Client::for_account(site)?;
    let result = match load(&mut client, false) {
        Ok((list, _)) => Ok(list),
        Err(error) if error.code.as_deref() == Some("tabbit_account_changed") => Err(error),
        Err(_) => account_previous(&client),
    };
    client.current()?;
    result
}

pub fn refresh(site: Site, params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        let mut previous = fallback(site);
        previous["refreshed"] = json!(false);
        previous["count"] = json!(previous["models"].as_array().map(Vec::len).unwrap_or(0));
        previous["message"] = json!("当前没有被主程序选定的 Tabbit 目录账号，请选择账号后刷新");
        return Ok(previous);
    }
    let _lock = AccountLock::acquire()?;
    let mut client = Client::for_account(site)?;
    match load(&mut client, params["force"].as_bool().unwrap_or(false)) {
        Ok((mut list, refreshed)) => {
            // 必须同次返回快照；WASM 新实例不能靠之后的 models.list 找回本地变量。
            // 若转发刚刷新了账号状态而 provider 目录尚未更新，也要把同份真实快照交回宿主。
            let visible = sdk::catalog::cached()?;
            list["refreshed"] = json!(
                refreshed || visible["meta"] != list["meta"] || visible["models"] != list["models"]
            );
            list["count"] = json!(list["models"].as_array().map(Vec::len).unwrap_or(0));
            client.current()?;
            Ok(list)
        }
        Err(error) => {
            client.current()?;
            let mut previous = account_previous(&client)?;
            previous["refreshed"] = json!(false);
            previous["count"] = json!(previous["models"].as_array().map(Vec::len).unwrap_or(0));
            previous["message"] = json!(error.message);
            Ok(previous)
        }
    }
}

pub fn select(list: &Value, requested: &str) -> Result<String> {
    let models = list["models"]
        .as_array()
        .ok_or_else(|| Error::http(502, "Tabbit 模型目录格式无效"))?;
    if models.is_empty() {
        return Err(
            Error::http(403, "Tabbit 当前目录明确为空，请稍后刷新或选择另一提供商")
                .with_code("tabbit_empty_catalog"),
        );
    }
    if requested.trim().is_empty()
        || requested.eq_ignore_ascii_case("default")
        || requested.ends_with("-default")
    {
        return Ok(models
            .iter()
            .find(|model| model["isDefault"] == true)
            .and_then(|model| model["upstreamModel"].as_str())
            .filter(|model| !model.is_empty())
            .unwrap_or("default")
            .to_string());
    }
    let wanted = normalized(requested);
    for model in models {
        if ["id", "name", "upstreamModel"].iter().any(|key| {
            model[*key]
                .as_str()
                .is_some_and(|name| normalized(name) == wanted)
        }) {
            return Ok(text(model, &["upstreamModel", "name"]));
        }
    }
    Err(Error::http(400, "未知的 Tabbit 模型，请先刷新模型目录").with_code("tabbit_unknown_model"))
}
