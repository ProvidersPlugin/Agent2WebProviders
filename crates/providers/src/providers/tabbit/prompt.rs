//! 把 OpenAI 请求摊成 Tabbit 能吃的**一段纯文本**。
//!
//! 上游 completion 只收一个 `content` 字符串：没有 messages 数组、没有 role、
//! 没有原生工具调用。所以这里：
//! 1. 按角色加标记（`[系统指令]` / `[助手]`）让模型看得出对话结构；
//! 2. 按字符预算裁剪（实测 20000 字符成功、25000 必失败，预算取 16000）：
//!    系统指令优先，其余**从最近往回填**；
//! 3. 明确拒绝工具声明与工具历史 —— 本适配器不做提示词模拟工具调用
//!    （Web 包的口径，与 DeepSeek 网页一致）。
//!
//! 图片同样拒绝：网页 v3 runs 的图片要走上传 + references，当前适配未接，
//! 不能假装看得见。

use serde_json::Value;

use crate::errors::GatewayError;

use super::config::{
    HISTORY_NOTE, PROMPT_BUDGET, PROMPT_HEAD, PROMPT_SINGLE, PROMPT_TAIL, SYSTEM_BUDGET,
    TRUNC_NOTE,
};

fn bad(message: impl Into<String>) -> GatewayError {
    GatewayError::with_status(400, message)
}

fn message_text(content: Option<&Value>) -> Result<String, GatewayError> {
    let Some(content) = content else {
        return Ok(String::new());
    };
    match content {
        Value::Null => Ok(String::new()),
        Value::String(text) => Ok(text.trim().to_string()),
        Value::Array(parts) => {
            let mut chunks = Vec::new();
            for part in parts {
                let kind = part.get("type").and_then(Value::as_str).unwrap_or("text");
                match kind {
                    "text" | "input_text" | "output_text" => {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            if !text.trim().is_empty() {
                                chunks.push(text.trim().to_string());
                            }
                        }
                    }
                    "image" | "image_url" | "input_image" => {
                        return Err(bad(
                            "Tabbit 网页当前适配只支持文本，图片输入尚未接入（请移除图片或改用支持图片的提供商）",
                        ))
                    }
                    _ => {
                        return Err(bad(format!(
                            "Tabbit 网页不支持内容块类型「{kind}」，只接受文本"
                        )))
                    }
                }
            }
            Ok(chunks.join("\n"))
        }
        _ => Err(bad("Tabbit 消息内容必须是文本或文本内容块数组")),
    }
}

fn render_message(message: &Value) -> Result<Option<String>, GatewayError> {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user");
    if role == "tool" || role == "function" {
        return Err(bad(
            "Tabbit 网页没有原生工具调用，不接受工具结果历史（请移除 tools / tool 消息）",
        ));
    }
    if message
        .get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|calls| !calls.is_empty())
    {
        return Err(bad(
            "Tabbit 网页没有原生工具调用，不接受工具调用历史（请移除 tools / tool_calls）",
        ));
    }
    let text = message_text(message.get("content"))?;
    if text.is_empty() {
        return Ok(None);
    }
    let rendered = match role {
        "system" | "developer" => format!("[系统指令]\n{text}"),
        "assistant" => format!("[助手]\n{text}"),
        _ => text,
    };
    Ok(Some(rendered))
}

fn clip_text(text: &str, limit: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= limit {
        return text.to_string();
    }
    let mut head = PROMPT_HEAD.min(limit.saturating_sub(200).max(1));
    if limit < 200 {
        head = (limit / 2).max(1);
    }
    let tail = PROMPT_TAIL.min(limit.saturating_sub(head + 60));
    let mut out = String::new();
    out.extend(chars.iter().take(head));
    out.push_str(TRUNC_NOTE);
    if tail > 0 {
        out.extend(chars.iter().skip(chars.len() - tail));
    }
    out
}

fn join_within_budget(systems: Vec<String>, others: Vec<String>) -> String {
    let system_text = clip_text(&systems.join("\n\n"), SYSTEM_BUDGET);
    let mut used = system_text.chars().count();
    let mut kept_reversed: Vec<String> = Vec::new();
    let mut dropped = false;
    for block in others.iter().rev() {
        let block = clip_text(block, PROMPT_SINGLE);
        let length = block.chars().count();
        if used + length + 2 > PROMPT_BUDGET {
            dropped = true;
            continue;
        }
        kept_reversed.push(block);
        used += length + 2;
    }
    let mut blocks = Vec::new();
    if !system_text.trim().is_empty() {
        blocks.push(system_text);
    }
    if dropped {
        blocks.push(HISTORY_NOTE.to_string());
    }
    blocks.extend(kept_reversed.into_iter().rev());
    blocks.join("\n\n")
}

/// 返回要送上游的 content。
pub fn build(body: &Value) -> Result<String, GatewayError> {
    // 工具声明：`tool_choice: "none"` 时客户端明确表示这次不用工具，可以继续；
    // 其余情况一律拒绝，避免把一次「本应调工具」的请求静默降级成纯聊天。
    let has_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    let has_functions = body
        .get("functions")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    let choice = body.get("tool_choice").or_else(|| body.get("function_call"));
    let choice_none = choice
        .and_then(Value::as_str)
        .is_some_and(|value| value.eq_ignore_ascii_case("none"));
    if (has_tools || has_functions) && !choice_none {
        return Err(bad(
            "Tabbit 网页没有原生工具调用接口，请移除 tools / functions，或改用支持工具调用的提供商",
        ));
    }
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("Tabbit 请求缺少 messages"))?;
    if messages.is_empty() {
        return Err(bad("Tabbit messages 不能为空"));
    }
    // 单条 user 消息直接原文发出：最省 token，也最接近正常网页对话。
    if messages.len() == 1
        && messages[0].get("role").and_then(Value::as_str) == Some("user")
    {
        let text = message_text(messages[0].get("content"))?;
        if !text.trim().is_empty() {
            return Ok(text);
        }
    }
    let mut systems = Vec::new();
    let mut others = Vec::new();
    for message in messages {
        let Some(rendered) = render_message(message)? else {
            continue;
        };
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        if matches!(role, "system" | "developer") {
            systems.push(rendered);
        } else {
            others.push(rendered);
        }
    }
    let text = join_within_budget(systems, others);
    if text.trim().is_empty() {
        return Err(bad("Tabbit messages 里没有可发送的文本内容"));
    }
    Ok(text)
}
