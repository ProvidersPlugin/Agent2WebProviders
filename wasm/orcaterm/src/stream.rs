//! assistant SSE → 标准 OpenAI Chat SSE，保留思考与 ACP 动作信封解包。
//! 真正的 [ERROR]: [LLM_ERROR] 不改成空成功；断流或空正文也不伪造正常 stop。
//! 原生没有估算 usage：这里只转交上游实际存在的 usage 对象，不造零值或猜 token 数。

use agent2api_wasm_sdk::{self as sdk, http, json, sse::SseDecoder, Error, Result, Value};

use super::{account, credentials::Credentials, errors};

const ENVELOPE_LIMIT: usize = 4 * 1024 * 1024;

pub struct Output {
    id: String,
    created: i64,
    model: String,
    decoder: SseDecoder,
    started: bool,
    finished: bool,
    buffered: Option<bool>,
    content: String,
    current_id: String,
    emitted_plain: bool,
    has_content: bool,
    reasoning_seen: bool,
    usage: Option<Value>,
}

enum EnvelopeOutcome {
    Text(String),
    Empty,
    NotEnvelope,
}

struct ParsedEnvelope {
    thinking: Option<String>,
    outcome: EnvelopeOutcome,
}

impl Output {
    pub fn new(model: &str) -> Self {
        let now = sdk::now();
        Self {
            id: format!(
                "chatcmpl-{}",
                now.saturating_mul(1000) % 1_000_000_000_000_000
            ),
            created: now / 1000,
            model: model.to_string(),
            decoder: SseDecoder::default(),
            started: false,
            finished: false,
            buffered: None,
            content: String::new(),
            current_id: String::new(),
            emitted_plain: false,
            has_content: false,
            reasoning_seen: false,
            usage: None,
        }
    }

    fn head(&mut self) -> Result<()> {
        if !self.started {
            sdk::stream_head(200)?;
            self.started = true;
        }
        Ok(())
    }

    fn chunk(&mut self, text: &str, reasoning: bool, finish: Option<&str>) -> Result<()> {
        self.head()?;
        let mut delta = json!({});
        if !text.is_empty() {
            delta[if reasoning {
                "reasoning_content"
            } else {
                "content"
            }] = json!(text);
        }
        sdk::stream_event(&json!({
            "id": self.id, "object": "chat.completion.chunk", "created": self.created, "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }]
        }))
    }

    fn emit_plain(&mut self, text: &str) -> Result<()> {
        self.emitted_plain |= !text.is_empty();
        self.has_content |= !text.trim().is_empty();
        self.chunk(text, false, None)
    }

    fn text(&mut self, delta: &str) -> Result<()> {
        if self.buffered == Some(false) {
            return self.emit_plain(delta);
        }
        if self.content.len().saturating_add(delta.len()) > ENVELOPE_LIMIT {
            return Err(Error::http(
                502,
                "OrcaTerm 动作信封超过 4 MiB，未静默截断内容",
            ));
        }
        self.content.push_str(delta);
        if self.buffered.is_none() {
            let head = self.content.trim_start();
            if head.is_empty() || "```".starts_with(head) && head.len() < 3 {
                return Ok(());
            }
            let envelope = head.starts_with('{') || head.starts_with("```");
            self.buffered = Some(envelope);
            if !envelope {
                let text = std::mem::take(&mut self.content);
                return self.emit_plain(&text);
            }
        }
        Ok(())
    }

    fn flush_content(&mut self, allow_incomplete_raw: bool) -> Result<()> {
        if self.buffered == Some(false) {
            return Ok(());
        }
        let raw = std::mem::take(&mut self.content);
        if raw.is_empty() {
            return Ok(());
        }
        if raw.trim_start().starts_with("{ERROR}:") {
            return Err(Error::http(
                502,
                "OrcaTerm 上游返回动作错误，未伪装成正常正文",
            ));
        }
        let parsed = parse_envelope(&raw);
        let text = match parsed.outcome {
            EnvelopeOutcome::Text(text) => text,
            EnvelopeOutcome::Empty => {
                if allow_incomplete_raw {
                    return Ok(());
                }
                return Err(Error::http(
                    502,
                    "OrcaTerm 上游动作信封未返回有效正文，未当作正常回答",
                )
                .with_code("orcaterm_incomplete"));
            }
            EnvelopeOutcome::NotEnvelope => {
                if allow_incomplete_raw {
                    return Ok(());
                }
                raw
            }
        };
        if !self.reasoning_seen {
            if let Some(reasoning) = parsed.thinking {
                self.reasoning_seen = true;
                self.chunk(&reasoning, true, None)?;
            }
        }
        self.emit_plain(&text)
    }

    fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.flush_content(false)?;
        if !self.has_content {
            return Err(Error::http(
                502,
                "OrcaTerm 上游只返回思考或空响应，没有最终正文",
            )
            .with_code("orcaterm_incomplete"));
        }
        self.chunk("", false, Some("stop"))?;
        if let Some(usage) = self.usage.take() {
            self.head()?;
            sdk::stream_event(&json!({ "id": self.id, "object": "chat.completion.chunk",
                "created": self.created, "model": self.model, "choices": [], "usage": usage }))?;
        }
        sdk::stream_bytes(b"data: [DONE]\n\n")?;
        self.finished = true;
        Ok(())
    }

    fn event(&mut self, payload: &str, token: &str) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        if payload.trim() == "[DONE]" {
            return self.finish();
        }
        if let Some(message) = payload.trim_start().strip_prefix("[ERROR]:") {
            return Err(errors::upstream(
                200,
                &json!({ "message": message.trim() }),
                &[token],
            ));
        }
        if payload.trim_start().starts_with("[ABORTED]") {
            return Err(Error::http(502, "OrcaTerm 上游已中止生成，未当作完成"));
        }
        let value = serde_json::from_str::<Value>(payload)
            .map_err(|_| Error::http(502, "OrcaTerm 流式事件不是有效 JSON"))?;
        if errors::business_failed(&value) {
            return Err(errors::upstream(200, &value, &[token]));
        }
        if let Some(usage) = value.get("usage").filter(|usage| usage.is_object()) {
            self.usage = Some(usage.clone());
        }
        let id = value["id"].as_str().unwrap_or("");
        if id.starts_with("heartbeat-") {
            return Ok(());
        }
        if let Some(removed) = id.strip_prefix("remove-") {
            if removed == self.current_id {
                if self.emitted_plain {
                    return Err(Error::http(
                        502,
                        "OrcaTerm 上游撤回了已发送正文，未拼接改写回答",
                    ));
                }
                self.content.clear();
                self.buffered = None;
            }
            return Ok(());
        }
        if value["type"] != "ai" {
            return Ok(());
        }
        if !id.is_empty() && id != self.current_id {
            if !self.current_id.is_empty() {
                self.flush_content(false)?;
                self.buffered = None;
            }
            self.current_id = id.to_string();
            self.emitted_plain = false;
            self.reasoning_seen = false;
        }
        if value["reasoning"] != true {
            if let Some(text) = value["reasoningContent"]
                .as_str()
                .filter(|text| !text.is_empty())
            {
                self.reasoning_seen = true;
                self.chunk(text, true, None)?;
            }
        }
        let reasoning = value["reasoning"].as_bool().unwrap_or(false);
        let key = if reasoning {
            "reasoningContent"
        } else {
            "content"
        };
        if let Some(text) = value[key].as_str().filter(|text| !text.is_empty()) {
            if reasoning {
                self.reasoning_seen = true;
                self.chunk(text, true, None)?;
            } else {
                self.text(text)?;
            }
        }
        Ok(())
    }

    fn consume(&mut self, mut response: http::Response, credentials: &Credentials) -> Result<()> {
        while let Some(bytes) = response.read()? {
            account::validate_current(credentials)?;
            for payload in self.decoder.push(&bytes)? {
                self.event(&payload, &credentials.access_token)?;
            }
            if self.finished {
                return Ok(());
            }
        }
        account::validate_current(credentials)?;
        for payload in self.decoder.push(b"\n\n")? {
            self.event(&payload, &credentials.access_token)?;
        }
        if self.finished {
            Ok(())
        } else {
            Err(Error::http(
                502,
                "OrcaTerm 上游流式传输提前结束，未收到完整结束标记",
            ))
        }
    }

    pub fn forward(mut self, response: http::Response, credentials: &Credentials) -> Result<()> {
        let result = self.consume(response, credentials);
        if let Err(error) = result {
            // 故障时仍可交付已完整解包的缓冲正文，但绝不发 stop/[DONE] 把错误算成成功，也不漏出半截内部信封。
            if self.started
                && !self.content.is_empty()
                && error.code.as_deref() != Some("account_identity_changed")
                && error.code.as_deref() != Some("account_credentials_changed")
            {
                let _ = self.flush_content(true);
            }
            if self.started {
                sdk::stream_event(&errors::event(&error))?;
            }
            return Err(error);
        }
        Ok(())
    }
}

fn strip_code_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let Some(body) = rest.trim_end().strip_suffix("```") else {
        return trimmed;
    };
    if let Some((tag, content)) = body.split_once('\n') {
        let tag = tag.trim();
        if tag.is_empty()
            || tag
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '+'))
        {
            return content.trim();
        }
    } else if body
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("json"))
    {
        return body[4..].trim();
    } else if body.trim_start().starts_with('{') {
        return body.trim();
    }
    trimmed
}

fn parse_envelope(raw: &str) -> ParsedEnvelope {
    let inner = strip_code_fence(raw);
    let Ok(value) = serde_json::from_str::<Value>(inner) else {
        return ParsedEnvelope {
            thinking: None,
            outcome: EnvelopeOutcome::NotEnvelope,
        };
    };
    let Some(object) = value.as_object() else {
        return ParsedEnvelope {
            thinking: None,
            outcome: EnvelopeOutcome::NotEnvelope,
        };
    };
    let Some(action) = object.get("action").and_then(Value::as_str) else {
        return ParsedEnvelope {
            thinking: None,
            outcome: EnvelopeOutcome::NotEnvelope,
        };
    };
    let thinking = object
        .get("thinking")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let candidate = match action {
        "completion" => object.get("taskCompletion").and_then(Value::as_str),
        "default" => object.get("content").and_then(Value::as_str),
        _ => ["taskCompletion", "content", "question"]
            .iter()
            .find_map(|key| object.get(*key).and_then(Value::as_str)),
    };
    if let Some(text) = candidate.map(str::trim).filter(|text| !text.is_empty()) {
        return ParsedEnvelope {
            thinking,
            outcome: EnvelopeOutcome::Text(text.to_string()),
        };
    }
    let is_acp_envelope = matches!(action, "completion" | "default" | "question")
        || object.contains_key("thinking")
        || ["taskCompletion", "content", "question"]
            .iter()
            .any(|key| object.contains_key(*key));
    ParsedEnvelope {
        thinking,
        outcome: if is_acp_envelope {
            EnvelopeOutcome::Empty
        } else {
            EnvelopeOutcome::NotEnvelope
        },
    }
}
