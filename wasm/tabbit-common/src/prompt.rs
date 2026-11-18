//! 保留完整角色与历史；超过已确认的上游字符预算时明确拒绝，不截断指令或结果。

use agent2api_wasm_sdk::{Error, Result, Value};

const PROMPT_BUDGET: usize = 16000;

fn bad(message: &str) -> Error {
    Error::http(400, message).with_code("tabbit_unsupported_input")
}

fn text(content: &Value) -> Result<String> {
    match content {
        Value::Null => Ok(String::new()),
        Value::String(text) => Ok(text.to_string()),
        Value::Array(parts) => {
            let mut result = Vec::new();
            for part in parts {
                match part["type"].as_str().unwrap_or("text") {
                    "text" | "input_text" | "output_text" => {
                        let text = part["text"]
                            .as_str()
                            .ok_or_else(|| bad("Tabbit 文本内容块缺少文字，未忽略该段历史"))?;
                        if !text.is_empty() {
                            result.push(text);
                        }
                    }
                    "image" | "image_url" | "input_image" => {
                        return Err(bad("Tabbit 网页只支持文本，图片输入尚未接入，请移除图片"))
                    }
                    _ => return Err(bad("Tabbit 网页仅接受文本内容块")),
                }
            }
            Ok(result.join("\n"))
        }
        _ => Err(bad("Tabbit 消息必须是文本或文本内容块数组")),
    }
}

fn within_budget(text: String) -> Result<String> {
    if text.chars().count() > PROMPT_BUDGET {
        Err(bad(
            "Tabbit 请求超过 16000 字符预算，请缩短历史；未截断系统指令、工具声明或结果",
        ))
    } else if text.trim().is_empty() {
        Err(bad("Tabbit messages 中没有可发送文本"))
    } else {
        Ok(text)
    }
}

pub fn build(body: &Value) -> Result<String> {
    let choice = body
        .get("tool_choice")
        .or_else(|| body.get("function_call"));
    let none = choice
        .and_then(Value::as_str)
        .is_some_and(|choice| choice.eq_ignore_ascii_case("none"));
    let tools = ["tools", "functions"]
        .iter()
        .any(|key| body[*key].as_array().is_some_and(|tools| !tools.is_empty()));
    if tools && !none
        || choice.is_some_and(|choice| {
            choice.is_object() || matches!(choice.as_str(), Some("required" | "any"))
        })
    {
        return Err(bad(
            "Tabbit 网页没有原生工具调用接口，请移除 tools / functions 或使用支持工具调用的提供商",
        ));
    }
    let messages = body["messages"]
        .as_array()
        .filter(|messages| !messages.is_empty())
        .ok_or_else(|| bad("Tabbit 请求缺少有效 messages"))?;
    let mut systems = Vec::new();
    let mut others = Vec::new();
    for message in messages {
        let role = message["role"]
            .as_str()
            .filter(|role| {
                matches!(
                    *role,
                    "system" | "developer" | "assistant" | "user" | "tool" | "function"
                )
            })
            .ok_or_else(|| bad("Tabbit 消息缺少有效角色，未将未知角色变成用户输入"))?;
        if matches!(role, "tool" | "function")
            || message["tool_calls"]
                .as_array()
                .is_some_and(|calls| !calls.is_empty())
            || message
                .get("function_call")
                .is_some_and(|call| !call.is_null())
        {
            return Err(bad("Tabbit 网页不接受工具调用历史或工具结果"));
        }
        let text = text(&message["content"])?;
        if text.trim().is_empty() {
            continue;
        }
        // 单条 user 保留原生直传行为，不加角色标记；工具历史仍先校验。
        if messages.len() == 1 && role == "user" {
            return within_budget(text);
        }
        match role {
            "system" | "developer" => systems.push(format!("[系统指令]\n{text}")),
            "assistant" => others.push(format!("[助手]\n{text}")),
            "user" => others.push(format!("[用户]\n{text}")),
            _ => return Err(bad("Tabbit 网页不接受工具调用历史或工具结果")),
        }
    }
    let mut result = systems;
    result.extend(others);
    within_budget(result.join("\n\n"))
}
