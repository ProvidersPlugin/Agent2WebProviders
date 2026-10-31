use serde_json::{json, Value};

pub struct Prompt {
    pub text: String,
    pub images: Vec<super::images::ImageInput>,
}

pub fn thinking(body: &Value) -> bool {
    if let Some(value) = body.get("thinking_enabled").and_then(Value::as_bool) { return value; }
    if let Some(value) = body.get("thinking").and_then(Value::as_bool) { return value; }
    if body.pointer("/thinking/type").and_then(Value::as_str) == Some("disabled") { return false; }
    let effort = body.get("reasoning_effort").or_else(|| body.pointer("/reasoning/effort"))
        .and_then(Value::as_str).unwrap_or("none");
    !matches!(effort, "none" | "off" | "disabled" | "0" | "false")
        || body.pointer("/thinking/type").and_then(Value::as_str) == Some("enabled")
}

pub fn build(body: &Value) -> Result<Prompt, String> {
    let has_tools = body.get("tools").and_then(Value::as_array).is_some_and(|tools| !tools.is_empty());
    let has_functions = body.get("functions").and_then(Value::as_array).is_some_and(|tools| !tools.is_empty());
    let choice = body.get("tool_choice").or_else(|| body.get("function_call"));
    if (has_tools || has_functions) && !choice.is_some_and(|choice| choice == "none")
        || choice.is_some_and(|choice| choice.is_object() || choice == "required" || choice == "any") {
        return Err(super::bad_input("DeepSeek 网页没有原生工具调用接口，请移除 tools 或使用支持工具调用的提供商"));
    }
    let messages = body.get("messages").and_then(Value::as_array).ok_or_else(|| super::bad_input("缺少 messages"))?;
    let mut transcript = Vec::new();
    let mut images = Vec::new();
    for message in messages {
        let role = message["role"].as_str().unwrap_or("user");
        if role == "tool" || role == "function" || message.get("tool_calls").and_then(Value::as_array).is_some_and(|calls| !calls.is_empty()) {
            return Err(super::bad_input("DeepSeek 网页不接受工具调用历史或工具结果"));
        }
        let content = match message.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(parts)) => {
                let mut content = Vec::new();
                for part in parts {
                    if let Some(image) = super::images::input(part)? {
                        if role != "user" { return Err(super::bad_input("DeepSeek 图片只能放在 user 消息中")); }
                        if images.len() >= super::images::MAX_IMAGES { return Err(super::bad_input("DeepSeek 单次最多上传 50 张图片")); }
                        images.push(image);
                        content.push(format!("[附件图片 {}]", images.len()));
                    } else if let Some(text) = part["text"].as_str() {
                        content.push(text.to_string());
                    } else if !part.is_null() {
                        return Err(super::bad_input("DeepSeek 网页仅支持文本与图片内容块"));
                    }
                }
                content.join("\n")
            }
            None | Some(Value::Null) => String::new(),
            _ => return Err(super::bad_input("DeepSeek 消息内容必须是文本或内容块数组")),
        };
        transcript.push(json!({ "role": role, "content": content }));
    }
    if transcript.is_empty() { return Err(super::bad_input("DeepSeek messages 不能为空")); }
    let text = if transcript.len() == 1 && transcript[0]["role"] == "user" {
        transcript[0]["content"].as_str().unwrap_or("").to_string()
    } else {
        format!("请根据以下完整对话继续回答最后的用户请求。system 角色是对话行为约束，附件图片按编号对应各条用户消息。\n对话记录：{}", json!(transcript))
    };
    if text.trim().is_empty() { return Err(super::bad_input("DeepSeek 消息内容不能为空")); }
    Ok(Prompt { text, images })
}
