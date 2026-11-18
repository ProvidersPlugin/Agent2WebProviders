//! 主站只认最后一条 user，因此完整拍平历史；超过有界长度直接拒绝，不截掉后续消息。
//! 工具由宿主显式模拟为文本；原生图片与工具协议尚未接通，不能静默降级。

use agent2api_wasm_sdk::{json, Error, Result, Value};

const MAX_FLAT_CHARS: usize = 200_000;

pub fn validate(body: &Value) -> Result<()> {
    if !body.is_object() {
        return Err(Error::http(400, "智谱对话请求必须是对象"));
    }
    if ["tools", "functions"].iter().any(|key| {
        body.get(*key)
            .is_some_and(|value| !value.is_null() && !value.as_array().is_some_and(Vec::is_empty))
    }) || ["tool_choice", "function_call"].iter().any(|key| {
        body.get(*key).is_some_and(|value| {
            !value.is_null() && !matches!(value.as_str(), Some("none" | "auto"))
        })
    }) {
        return Err(Error::http(
            400,
            "智谱清言的工具调用尚未迁移，请使用支持工具调用的提供商",
        ));
    }
    if let Some(messages) = body.get("messages").filter(|value| !value.is_null()) {
        let messages = messages
            .as_array()
            .ok_or_else(|| Error::http(400, "智谱对话消息必须是数组"))?;
        for message in messages {
            validate_message(message)?;
        }
    }
    if let Some(prompt) = body.get("prompt").filter(|value| !value.is_null()) {
        match prompt {
            Value::String(_) => {}
            Value::Array(items) => {
                for item in items {
                    if !item.is_string() {
                        validate_message(item)?;
                    }
                }
            }
            _ => return Err(Error::http(400, "智谱对话提示词格式无效")),
        }
    }
    let messages = messages(body);
    if messages.is_empty()
        || !messages
            .iter()
            .any(|message| !text(&message["content"]).trim().is_empty())
    {
        return Err(Error::http(400, "智谱对话至少需要一条非空文本消息"));
    }
    flat(&messages)?;
    Ok(())
}

fn validate_message(message: &Value) -> Result<()> {
    if !message.is_object() {
        return Err(Error::http(400, "智谱对话消息格式无效"));
    }
    if !matches!(
        message["role"].as_str(),
        Some("system" | "developer" | "user" | "assistant")
    ) || message
        .get("tool_call_id")
        .is_some_and(|value| !value.is_null())
        || message
            .get("function_call")
            .is_some_and(|value| !value.is_null())
        || message
            .get("tool_calls")
            .is_some_and(|value| !value.is_null() && !value.as_array().is_some_and(Vec::is_empty))
    {
        return Err(Error::http(
            400,
            "智谱清言尚未接通工具结果与此消息角色，未转为普通用户文字发送",
        ));
    }
    match &message["content"] {
        Value::Null | Value::String(_) => Ok(()),
        Value::Array(parts) => {
            for part in parts {
                if part.is_string() {
                    continue;
                }
                if part["type"]
                    .as_str()
                    .is_some_and(|kind| kind.eq_ignore_ascii_case("text"))
                    && part["text"].is_string()
                {
                    continue;
                }
                return Err(Error::http(
                    400,
                    "智谱清言的图片、文件及其他非文本消息尚未迁移，未忽略这些内容发送",
                ));
            }
            Ok(())
        }
        _ => Err(Error::http(400, "智谱对话消息内容格式无效")),
    }
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                part.as_str().or_else(|| {
                    (part["type"]
                        .as_str()
                        .is_some_and(|kind| kind.eq_ignore_ascii_case("text")))
                    .then(|| part["text"].as_str())
                    .flatten()
                })
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn messages(body: &Value) -> Vec<Value> {
    if let Some(items) = body["messages"]
        .as_array()
        .filter(|items| !items.is_empty())
    {
        return items.clone();
    }
    match &body["prompt"] {
        Value::String(text) if !text.trim().is_empty() => {
            vec![json!({ "role": "user", "content": text })]
        }
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                if let Some(text) = item.as_str() {
                    Some(json!({ "role": "user", "content": text }))
                } else {
                    item.is_object().then(|| item.clone())
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn flat(messages: &[Value]) -> Result<String> {
    let nonempty: Vec<_> = messages
        .iter()
        .map(|message| {
            (
                message["role"].as_str().unwrap_or(""),
                text(&message["content"]),
            )
        })
        .filter(|(_, text)| !text.trim().is_empty())
        .collect();
    let length = nonempty.iter().try_fold(16usize, |length, (_, text)| {
        length
            .checked_add(text.chars().count())
            .and_then(|length| length.checked_add(20))
    });
    if length.is_none_or(|length| length > MAX_FLAT_CHARS) {
        return Err(Error::http(
            400,
            "智谱完整对话历史超过文本长度限制，未截断后发送",
        ));
    }
    if nonempty.len() == 1 && !matches!(nonempty[0].0, "system" | "developer" | "assistant") {
        return Ok(nonempty[0].1.clone());
    }
    let mut result = String::new();
    for (role, text) in nonempty {
        result.push_str(match role {
            "system" | "developer" => "<|sytstem|>",
            "assistant" => "<|assistant|>",
            _ => "<|user|>",
        });
        result.push('\n');
        result.push_str(&text);
        result.push('\n');
    }
    result.push_str("<|assistant|>\n");
    Ok(result)
}

fn level(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty() && text.chars().count() <= 32).then(|| text.to_string())
}

pub fn client_level(body: &Value) -> Option<String> {
    for key in [
        "reasoning_effort",
        "reasoningEffort",
        "effort",
        "reasoning",
        "thinking",
    ] {
        let value = &body[key];
        if let Some(text) = value.as_str() {
            if key == "thinking" {
                match text.trim().to_lowercase().as_str() {
                    "disabled" | "none" | "off" | "false" => return Some("off".into()),
                    "enabled" | "adaptive" | "true" => return Some("high".into()),
                    _ => {}
                }
            }
            if let Some(value) = level(text) {
                return Some(value);
            }
        } else if key == "reasoning" {
            if let Some(value) = value["effort"].as_str().and_then(level) {
                return Some(value);
            }
        } else if key == "thinking" {
            match value["type"]
                .as_str()
                .unwrap_or("")
                .trim()
                .to_lowercase()
                .as_str()
            {
                "disabled" | "none" | "off" => return Some("off".into()),
                "enabled" | "adaptive" => {
                    let budget = value["budget_tokens"].as_i64().unwrap_or(0);
                    return Some(
                        match budget {
                            i64::MIN..=0 => "medium",
                            1..=1024 => "low",
                            1025..=4096 => "medium",
                            4097..=10240 => "high",
                            10241..=32768 => "xhigh",
                            _ => "max",
                        }
                        .into(),
                    );
                }
                _ => {}
            }
        }
    }
    None
}

pub fn mode(level: &str) -> Option<&'static str> {
    match level.trim().to_lowercase().as_str() {
        "" | "fast" | "off" | "none" | "minimal" | "low" | "快速" => Some(""),
        "medium" | "high" | "thinking" | "深度" => Some("thinking"),
        "xhigh" | "max" | "deep_thinking" | "deep" | "极致" => Some("deep_thinking"),
        _ => None,
    }
}

pub fn build(body: &Value, model: &str) -> Result<Value> {
    let explicit = body["chat_mode"]
        .as_str()
        .map(str::trim)
        .filter(|mode| !mode.is_empty());
    let tier = client_level(body).as_deref().and_then(mode);
    let chat_mode = match explicit {
        Some(value) if mode(value).is_none() => value,
        Some(value) => tier.or_else(|| mode(value)).unwrap_or("thinking"),
        None => tier.unwrap_or("thinking"),
    };
    // 只传 chat_mode，不加 reasoning_effort：旧实测显示后者会把请求改路由到其它内部模型。
    Ok(
        json!({ "conversation_id": "", "messages": [{ "role": "user", "content": [{ "type": "text", "text": flat(&messages(body))? }] }],
        "meta_data": { "platform": "pc", "chat_mode": chat_mode, "selected_model": model,
            "is_networking": body.get("is_networking").or_else(|| body.get("enable_search")).and_then(Value::as_bool).unwrap_or(false) } }),
    )
}

pub fn reasoning_patch(level: &str, body: &Value) -> Value {
    if client_level(body).is_some()
        || body["chat_mode"]
            .as_str()
            .is_some_and(|mode| !mode.trim().is_empty())
    {
        return json!({ "action": "skip", "reason": "客户端已指定思考档位" });
    }
    match mode(level) {
        Some(value) => {
            json!({ "action": "set", "field": "reasoning_effort", "value": if value.is_empty() { "fast" } else { value } })
        }
        None => json!({ "action": "skip", "reason": "该等级不在智谱清言的思考档位内" }),
    }
}
