use serde_json::{json, Value};

#[derive(Default)]
pub struct Decoder {
    buffer: String,
    pub content: String,
    pub reasoning: String,
    pub total_tokens: u64,
    pub finished: bool,
    current_reasoning: bool,
}

impl Decoder {
    pub fn feed(&mut self, text: &str) -> Result<(), String> {
        self.buffer.push_str(&text.replace('\r', ""));
        if self.buffer.len() > 1024 * 1024 { return Err("DeepSeek SSE 帧超过大小上限".into()); }
        while let Some(index) = self.buffer.find("\n\n") {
            let frame = self.buffer[..index].to_string();
            self.buffer.drain(..index + 2);
            self.frame(&frame)?;
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<(), String> {
        let frame = std::mem::take(&mut self.buffer);
        if !frame.trim().is_empty() { self.frame(&frame)?; }
        if !self.finished { return Err("DeepSeek 网页响应未正常结束，未执行可能截断的工具调用".into()); }
        Ok(())
    }

    fn frame(&mut self, frame: &str) -> Result<(), String> {
        let event = frame.lines().find_map(|line| line.strip_prefix("event:")).map(str::trim).unwrap_or("");
        let data = frame.lines().filter_map(|line| line.strip_prefix("data:")).map(str::trim).collect::<Vec<_>>().join("\n");
        if data == "[DONE]" || event == "close" { self.finished = true; return Ok(()); }
        if data.is_empty() { return Ok(()); }
        let value: Value = serde_json::from_str(&data).map_err(|_| "DeepSeek SSE 数据不是有效 JSON")?;
        if event == "error" || value.get("error").is_some_and(|error| !error.is_null()) {
            return Err(value.get("message").or_else(|| value.pointer("/error/message")).and_then(Value::as_str).unwrap_or("DeepSeek 网页返回错误").into());
        }
        if event == "ready" { self.current_reasoning = true; return Ok(()); }
        if let Some(response) = value.pointer("/v/response") {
            if let Some(fragments) = response["fragments"].as_array() { for fragment in fragments { self.fragment(fragment); } }
            self.total_tokens = response["accumulated_token_usage"].as_u64().unwrap_or(self.total_tokens);
            return Ok(());
        }
        if value["p"] == "response/fragments" && value["o"] == "APPEND" {
            if let Some(fragments) = value["v"].as_array() { for fragment in fragments { self.fragment(fragment); } }
        } else if (value["p"] == "response/fragments/-1/content" && value["o"] == "APPEND")
            || (value.get("p").is_none() && value["o"] == "APPEND")
            || (value.get("p").is_none() && value.get("o").is_none() && value["v"].is_string()) {
            if let Some(text) = value["v"].as_str() { self.push(text); }
        } else if value["p"] == "response" && value["o"] == "BATCH" {
            if let Some(patches) = value["v"].as_array() {
                for patch in patches { if patch["p"] == "accumulated_token_usage" { self.total_tokens = patch["v"].as_u64().unwrap_or(self.total_tokens); } }
            }
        }
        if matches!(value["v"].as_str(), Some("FINISHED")) && matches!(value["p"].as_str(), Some("response/status" | "status")) { self.finished = true; }
        Ok(())
    }

    fn fragment(&mut self, fragment: &Value) {
        match fragment["type"].as_str() {
            Some("THINK") => self.current_reasoning = true,
            Some("RESPONSE") => self.current_reasoning = false,
            _ => return,
        }
        if let Some(text) = fragment["content"].as_str() { self.push(text); }
    }

    fn push(&mut self, text: &str) {
        if self.current_reasoning { self.reasoning.push_str(text); } else { self.content.push_str(text); }
    }
}

pub fn completion(id: &str, model: &str, decoder: &Decoder, calls: Option<Vec<Value>>) -> Value {
    let mut message = json!({ "role": "assistant", "content": decoder.content });
    if !decoder.reasoning.is_empty() { message["reasoning_content"] = json!(decoder.reasoning); }
    let reason = if let Some(calls) = calls { message["content"] = Value::Null; message["tool_calls"] = json!(calls); "tool_calls" } else { "stop" };
    let mut body = json!({ "id": id, "object": "chat.completion", "created": crate::http::now_ms() / 1000, "model": model,
        "choices": [{ "index": 0, "message": message, "finish_reason": reason }] });
    if decoder.total_tokens > 0 { body["usage"] = json!({ "total_tokens": decoder.total_tokens }); }
    body
}
