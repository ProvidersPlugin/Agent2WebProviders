//! 从主程序 core/protocol/chatglm_stream.rs 移植必要纯逻辑，不复制协议树。
//! init 是增量，finish 是当前内容段全文快照；每个logic_id维护独立基准，混合loading不遮住真实正文。

use crate::{account::Session, errors, sign};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub struct Stream {
    id: String,
    model: String,
    created: i64,
    segments: std::collections::BTreeMap<String, (String, String)>,
    usage: Option<Value>,
    delivered: usize,
    pub started: bool,
    pub finished: bool,
}

impl Stream {
    pub fn new(model: &str) -> Result<Self> {
        Ok(Self {
            id: format!("chatcmpl-{}", sign::uuid()?.replace('-', "")),
            model: model.into(),
            created: sign::now()? / 1000,
            segments: Default::default(),
            usage: None,
            delivered: 0,
            started: false,
            finished: false,
        })
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        json!({ "id": self.id, "object": "chat.completion.chunk", "created": self.created, "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }] })
    }

    fn start(&mut self) -> Result<()> {
        if !self.started {
            sdk::stream_head(200)?;
            self.started = true;
            sdk::stream_event(&self.chunk(json!({ "role": "assistant", "content": "" }), None))?;
        }
        Ok(())
    }

    fn output(&mut self, mut text: &str, reasoning: bool) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        self.delivered = self.delivered.saturating_add(text.len());
        if self.delivered > 8 * 1024 * 1024 {
            return Err(Error::http(502, "智谱回答超过大小限制"));
        }
        self.start()?;
        while !text.is_empty() {
            let mut end = text.len().min(16 * 1024);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let part = &text[..end];
            let delta = if reasoning {
                json!({ "reasoning_content": part })
            } else {
                json!({ "content": part })
            };
            sdk::stream_event(&self.chunk(delta, None))?;
            text = &text[end..];
        }
        Ok(())
    }

    pub fn consume(&mut self, frame: &crate::sse::Event, session: &Session) -> Result<()> {
        let data = frame.data.as_str();
        if frame.name.trim().eq_ignore_ascii_case("error") {
            let event = serde_json::from_str(data)
                .unwrap_or_else(|_| json!({ "message": "智谱上游返回了错误事件，内容无法解析" }));
            return Err(errors::upstream(
                200,
                &event,
                &[
                    &session.token,
                    &crate::credentials::refresh(&session.secret),
                ],
            ));
        }
        if data.trim().is_empty() || data.trim().eq_ignore_ascii_case("HEARTBEAT") {
            return Ok(());
        }
        if data.trim() == "[DONE]" {
            return self.finish();
        }
        let event: Value = serde_json::from_str(data)
            .map_err(|_| Error::http(502, "智谱流式事件内容无法解析，结果未确认"))?;
        if !event.is_object() {
            return Err(Error::http(502, "智谱流式事件格式无效，结果未确认"));
        }
        if errors::failed(&event) {
            return Err(errors::upstream(
                200,
                &event,
                &[
                    &session.token,
                    &crate::credentials::refresh(&session.secret),
                ],
            ));
        }
        if self.finished {
            return Ok(());
        }
        if let Some(usage) = event.get("usage").filter(|usage| usage.is_object()) {
            self.usage = normalized_usage(usage).or_else(|| self.usage.clone());
        }
        for part in event["parts"].as_array().into_iter().flatten() {
            if errors::failed(part) {
                return Err(errors::upstream(
                    200,
                    part,
                    &[
                        &session.token,
                        &crate::credentials::refresh(&session.secret),
                    ],
                ));
            }
            if part
                .pointer("/meta_data/engine/action")
                .and_then(Value::as_str)
                == Some("loading")
            {
                continue;
            }
            let mut key = errors::text(part, &["logic_id", "id"]);
            if key.is_empty() {
                key = "default".into();
            }
            if !self.segments.contains_key(&key) && self.segments.len() >= 1024 {
                return Err(Error::http(502, "智谱回答内容段超过限制"));
            }
            let snapshot = finish_status(&event["status"]) || finish_status(&part["status"]);
            let mut full_text = String::new();
            let mut full_thinking = String::new();
            for content in part["content"].as_array().into_iter().flatten() {
                match content["type"]
                    .as_str()
                    .unwrap_or("")
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "text" => full_text.push_str(content["text"].as_str().unwrap_or("")),
                    "think" => full_thinking.push_str(content["think"].as_str().unwrap_or("")),
                    _ => {}
                }
            }
            let previous = self.segments.entry(key).or_default();
            let text = advance(&mut previous.0, &full_text, snapshot)?;
            let thinking = advance(&mut previous.1, &full_thinking, snapshot)?;
            self.output(&thinking, true)?;
            self.output(&text, false)?;
        }
        if finish_status(&event["status"]) {
            self.finish()?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        if !self.started {
            return Err(Error::http(502, "智谱上游返回空响应，没有任何正文或思考"));
        }
        self.finished = true;
        Ok(())
    }

    pub fn complete(&self) -> Result<()> {
        if !self.finished || !self.started {
            return Err(Error::http(502, "智谱上游流尚未确认完成"));
        }
        sdk::stream_event(&self.chunk(json!({}), Some("stop")))?;
        if let Some(usage) = &self.usage {
            sdk::stream_event(
                &json!({ "id": self.id, "object": "chat.completion.chunk", "created": self.created,
                "model": self.model, "choices": [], "usage": usage }),
            )?;
        }
        sdk::stream_bytes(b"data: [DONE]\n\n")
    }
}

fn finish_status(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|value| matches!(value.to_lowercase().as_str(), "finish" | "finished"))
}

fn normalized_usage(value: &Value) -> Option<Value> {
    let read = |keys: &[&str]| keys.iter().find_map(|key| value[*key].as_u64());
    let prompt = read(&["prompt_tokens", "input_tokens"]);
    let completion = read(&["completion_tokens", "output_tokens"]);
    if prompt.is_none() && completion.is_none() {
        return None;
    }
    let prompt = prompt.unwrap_or(0);
    let completion = completion.unwrap_or(0);
    let total = read(&["total_tokens"]).or_else(|| prompt.checked_add(completion))?;
    Some(json!({ "prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": total }))
}

fn advance(previous: &mut String, frame: &str, snapshot: bool) -> Result<String> {
    if frame.is_empty() {
        return Ok(String::new());
    }
    let delta = if snapshot {
        frame
            .strip_prefix(previous.as_str())
            .ok_or_else(|| Error::http(502, "智谱上游改写了已输出的内容快照，响应未确认完成"))?
    } else {
        frame
    };
    if previous.len().saturating_add(delta.len()) > 8 * 1024 * 1024 {
        return Err(Error::http(502, "智谱回答内容段超过大小限制"));
    }
    let delta = delta.to_string();
    previous.push_str(&delta);
    Ok(delta)
}
