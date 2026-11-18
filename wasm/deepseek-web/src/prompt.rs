use agent2api_wasm_sdk::{json, Error, Result, Value};

use crate::image_input::{self, ImageInput};

pub struct Prompt<'a> {
    pub text: String,
    pub images: Vec<ImageInput<'a>>,
}

pub fn thinking(body: &Value) -> bool {
    if let Some(value) = body.get("thinking_enabled").and_then(Value::as_bool) {
        return value;
    }
    if let Some(value) = body.get("thinking").and_then(Value::as_bool) {
        return value;
    }
    if body.pointer("/thinking/type").and_then(Value::as_str) == Some("disabled") {
        return false;
    }
    let effort = body
        .get("reasoning_effort")
        .or_else(|| body.pointer("/reasoning/effort"))
        .and_then(Value::as_str)
        .unwrap_or("none");
    !matches!(effort, "none" | "off" | "disabled" | "0" | "false")
        || body.pointer("/thinking/type").and_then(Value::as_str) == Some("enabled")
}

pub fn search(body: &Value) -> Result<bool> {
    for key in ["search_enabled", "searchEnabled", "web_search"] {
        if let Some(value) = body.get(key) {
            return value
                .as_bool()
                .ok_or_else(|| invalid("DeepSeek 网页搜索开关必须是布尔值"));
        }
    }
    Ok(false)
}

fn invalid(message: &str) -> Error {
    Error::http(400, message)
}

pub fn build(body: &Value) -> Result<Prompt<'_>> {
    let has_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    let has_functions = body
        .get("functions")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    let choice = body
        .get("tool_choice")
        .or_else(|| body.get("function_call"));
    if (has_tools || has_functions) && !choice.is_some_and(|choice| choice == "none")
        || choice
            .is_some_and(|choice| choice.is_object() || choice == "required" || choice == "any")
    {
        return Err(invalid(
            "DeepSeek 网页没有原生工具调用接口，请移除工具或选择其他提供商",
        ));
    }
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("DeepSeek 请求缺少 messages"))?;
    let mut transcript = Vec::with_capacity(messages.len());
    let mut images = Vec::new();
    for message in messages {
        let role = message["role"]
            .as_str()
            .ok_or_else(|| invalid("DeepSeek 消息缺少有效角色"))?;
        if !matches!(
            role,
            "user" | "assistant" | "system" | "developer" | "tool" | "function"
        ) {
            return Err(invalid("DeepSeek 消息角色无效"));
        }
        if role == "tool"
            || role == "function"
            || message
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| !calls.is_empty())
            || message
                .get("function_call")
                .is_some_and(|call| !call.is_null())
        {
            return Err(invalid("DeepSeek 网页不接受工具调用历史或工具结果"));
        }
        let content = match message.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(parts)) => {
                let mut content = Vec::with_capacity(parts.len());
                for part in parts {
                    if let Some(image) = image_input::input(part)? {
                        if role != "user" {
                            return Err(invalid("DeepSeek 图片只能放在 user 消息中"));
                        }
                        if images.len() >= image_input::MAX_IMAGES {
                            return Err(invalid("DeepSeek 单次最多上传 50 张图片"));
                        }
                        images.push(image);
                        content.push(format!("[附件图片 {}]", images.len()));
                    } else if matches!(
                        part["type"].as_str(),
                        Some("tool_use" | "tool_result" | "function_call" | "function_call_output")
                    ) {
                        return Err(invalid("DeepSeek 网页不接受工具调用或工具结果内容块"));
                    } else if matches!(
                        part["type"].as_str(),
                        Some("text" | "input_text" | "output_text")
                    ) || part.get("type").is_none()
                        && part
                            .as_object()
                            .is_some_and(|fields| fields.len() == 1 && fields.contains_key("text"))
                    {
                        content.push(
                            part["text"]
                                .as_str()
                                .ok_or_else(|| invalid("DeepSeek 文本内容块缺少有效文字"))?
                                .to_string(),
                        );
                    } else if !part.is_null() {
                        // 未识别图片/文件块即使附有 text 也不能静默丢图后继续对话。
                        return Err(invalid(
                            "DeepSeek 网页仅支持文本与已接入的图片内容块，不会把其他附件降级为文字",
                        ));
                    }
                }
                content.join("\n")
            }
            None | Some(Value::Null) => String::new(),
            _ => return Err(invalid("DeepSeek 消息内容必须是文本或内容块数组")),
        };
        let mut entry = json!({ "role": role, "content": content });
        if role == "assistant" {
            if let Some(value) = message
                .get("reasoning_content")
                .filter(|value| !value.is_null())
            {
                let reasoning = value
                    .as_str()
                    .ok_or_else(|| invalid("DeepSeek 历史思考内容必须是文本"))?;
                if !reasoning.is_empty() {
                    entry["reasoning_content"] = json!(reasoning);
                }
            }
        }
        transcript.push(entry);
    }
    if transcript.is_empty() {
        return Err(invalid("DeepSeek messages 不能为空"));
    }
    let text = if transcript.len() == 1 && transcript[0]["role"] == "user" {
        transcript[0]["content"].as_str().unwrap_or("").to_string()
    } else {
        format!("请根据以下完整对话继续回答最后的用户请求。system 角色是对话行为约束，附件图片按编号对应各条用户消息。\n对话记录：{}", json!(transcript))
    };
    if text.trim().is_empty() {
        return Err(invalid("DeepSeek 消息内容不能为空"));
    }
    // HTTP 请求通过 base64 封装也要小于 ABI；不要把大文本误报为图片入站已解决。
    if text.len() > 4 * 1024 * 1024 {
        return Err(invalid("DeepSeek 网页文本请求过大，请缩短对话"));
    }
    // 首次插件账号读取、目录请求和文件上传前，核对所有已知内联内容与本次宿主资源。
    // 资源元数据/首片只来自当前调用，不展开大图；远程 URL 仍不预取或重试。
    image_input::validate_all(&images)?;
    Ok(Prompt { text, images })
}
