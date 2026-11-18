//! 网页私有补丁转为标准 Chat SSE；无路径 token 沿用当前片段，不输出 status 等控制字段。
//! 保留原生 THINK/RESPONSE、APPEND、初始化快照及用量；SET 快照只能追加差分，不能重发全文。
//! quasi_status=FINISHED 不等于真实完成；晚到的错误或 CONTENT_FILTER 不伪装 stop。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

use crate::{account::Session, errors, sse::Event, util};

const MAX_TEXT: usize = 8 * 1024 * 1024;
const MAX_FRAGMENTS: usize = 4096;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Think,
    Response,
    Other,
}

fn kind(value: &Value) -> Kind {
    match value["type"].as_str() {
        Some("THINK" | "REASONING" | "THINKING") => Kind::Think,
        Some("RESPONSE" | "ANSWER") => Kind::Response,
        _ => Kind::Other,
    }
}

struct Fragment {
    kind: Kind,
    text: String,
}

#[derive(Default)]
struct Decoder {
    content: String,
    reasoning: String,
    total_tokens: Option<u64>,
    fragments: Vec<Fragment>,
    implicit_reasoning: bool,
    direct_content: String,
    direct_reasoning: String,
    finished: bool,
    closed: bool,
}

fn malformed() -> Error {
    Error::http(502, "DeepSeek 网页流格式已变化，请更新插件")
}

impl Decoder {
    fn emit(&mut self, kind: Kind, text: &str) -> Result<()> {
        if self
            .content
            .len()
            .saturating_add(self.reasoning.len())
            .saturating_add(text.len())
            > MAX_TEXT
        {
            return Err(Error::http(502, "DeepSeek 响应超过大小上限"));
        }
        match kind {
            Kind::Think => self.reasoning.push_str(text),
            Kind::Response => self.content.push_str(text),
            Kind::Other if !text.is_empty() => {
                return Err(Error::http(502, "DeepSeek 返回了尚未适配的网页片段类型"))
            }
            _ => {}
        }
        Ok(())
    }

    fn reconcile_fragments(&mut self, kind: Kind) -> Result<()> {
        if kind == Kind::Other {
            return Ok(());
        }
        let size = self
            .fragments
            .iter()
            .filter(|fragment| fragment.kind == kind)
            .map(|fragment| fragment.text.len())
            .sum::<usize>();
        if size > MAX_TEXT {
            return Err(malformed());
        }
        let mut candidate = String::with_capacity(size);
        for fragment in self
            .fragments
            .iter()
            .filter(|fragment| fragment.kind == kind)
        {
            candidate.push_str(&fragment.text);
        }
        let emitted = if kind == Kind::Think {
            &self.reasoning
        } else {
            &self.content
        };
        let delta = if let Some(delta) = candidate.strip_prefix(emitted) {
            delta.to_string()
        } else if emitted.starts_with(&candidate) {
            String::new()
        } else {
            return Err(malformed());
        };
        self.emit(kind, &delta)
    }

    fn fragment(&mut self, value: &Value, at: usize, append: bool) -> Result<()> {
        let kind = kind(value);
        // TIP、搜索提示等未知片段不当作正文或工具，也不保存其整段内容。
        let text = if kind == Kind::Other {
            ""
        } else {
            value["content"].as_str().unwrap_or("")
        };
        if at == self.fragments.len() {
            if at >= MAX_FRAGMENTS {
                return Err(Error::http(502, "DeepSeek 网页片段数量超过限制"));
            }
            if text.len() > MAX_TEXT {
                return Err(malformed());
            }
            self.fragments.push(Fragment {
                kind,
                text: text.into(),
            });
            self.reconcile_fragments(kind)?;
        } else {
            let old = self.fragments.get(at).ok_or_else(malformed)?;
            if append || old.kind != kind {
                return Err(malformed());
            }
            let delta = if let Some(delta) = text.strip_prefix(&old.text) {
                delta
            } else if old.text.starts_with(text) {
                // 晚到的短快照不能回退已发内容，保留原片段等待真正增量。
                return Ok(());
            } else {
                return Err(Error::http(
                    502,
                    "DeepSeek 已发送的网页片段被改写，不能重复发送全文",
                ));
            };
            if !delta.is_empty() && at + 1 != self.fragments.len() {
                return Err(malformed());
            }
            self.fragments[at].text = text.into();
            self.reconcile_fragments(kind)?;
        }
        // response片段是正文已开始的明确证据，后续裸续段应跟随它。
        if !self.fragments.is_empty() && kind != Kind::Other {
            self.implicit_reasoning = kind == Kind::Think;
        }
        Ok(())
    }

    fn fragments(&mut self, value: &Value, append: bool) -> Result<()> {
        if append && value.is_object() {
            return self.fragment(value, self.fragments.len(), true);
        }
        let items = value.as_array().ok_or_else(malformed)?;
        if !append && items.len() < self.fragments.len() {
            // 更早的初始化快照不能清掉后来已追加的片段。
            return Ok(());
        }
        for (index, item) in items.iter().enumerate() {
            self.fragment(
                item,
                if append { self.fragments.len() } else { index },
                append,
            )?;
        }
        Ok(())
    }

    fn direct(&mut self, kind: Kind, value: &Value, operation: &str) -> Result<()> {
        let text = value
            .as_str()
            .or_else(|| value["text"].as_str())
            .or_else(|| value["content"].as_str())
            .ok_or_else(malformed)?;
        let stored = match kind {
            Kind::Think => &self.direct_reasoning,
            _ => &self.direct_content,
        };
        let emitted = if kind == Kind::Think {
            &self.reasoning
        } else {
            &self.content
        };
        // 从片段格式切到直连增量时，第一段增量接在已发送内容后；SET仍只做快照对账。
        let stored = if stored.is_empty() && matches!(operation, "" | "APPEND") {
            emitted
        } else {
            stored
        };
        let candidate = match operation {
            "" | "APPEND" => format!("{stored}{text}"),
            "SET" => {
                if stored.starts_with(text) {
                    return Ok(());
                }
                if !text.starts_with(stored) {
                    return Err(malformed());
                }
                text.to_string()
            }
            _ => return Err(malformed()),
        };
        self.implicit_reasoning = kind == Kind::Think;
        if candidate.len() > MAX_TEXT {
            return Err(malformed());
        }
        let emitted = if kind == Kind::Think {
            &self.reasoning
        } else {
            &self.content
        };
        let delta = if let Some(delta) = candidate.strip_prefix(emitted) {
            delta.to_string()
        } else if emitted.starts_with(&candidate) {
            String::new()
        } else {
            return Err(malformed());
        };
        self.emit(kind, &delta)?;
        if kind == Kind::Think {
            self.direct_reasoning = candidate;
        } else {
            self.direct_content = candidate;
        }
        Ok(())
    }

    fn text(&mut self, at: Option<usize>, value: &Value, operation: &str) -> Result<()> {
        let text = value
            .as_str()
            .or_else(|| value["text"].as_str())
            .or_else(|| value["content"].as_str())
            .ok_or_else(malformed)?;
        let Some(at) = at.or_else(|| self.fragments.len().checked_sub(1)) else {
            if !matches!(operation, "" | "APPEND") {
                return Err(malformed());
            }
            return self.direct(
                if self.implicit_reasoning {
                    Kind::Think
                } else {
                    Kind::Response
                },
                value,
                operation,
            );
        };
        let fragment = self.fragments.get(at).ok_or_else(malformed)?;
        if at + 1 != self.fragments.len() {
            return Err(malformed());
        }
        let kind = fragment.kind;
        if kind == Kind::Other {
            return Ok(());
        }
        let delta = match operation {
            "" | "APPEND" => text,
            "SET" => {
                if fragment.text.starts_with(text) {
                    return Ok(());
                }
                text.strip_prefix(&fragment.text).ok_or_else(|| {
                    Error::http(502, "DeepSeek 已发送的网页片段被改写，不能重复发送全文")
                })?
            }
            _ => return Err(malformed()),
        };
        if operation == "SET" {
            self.fragments[at].text = text.into();
        } else {
            self.fragments[at].text.push_str(delta);
        }
        self.reconcile_fragments(kind)?;
        Ok(())
    }

    fn status(&mut self, value: &Value, quasi: bool) -> Result<()> {
        match value.as_str().unwrap_or("") {
            "FINISHED" if !quasi => self.finished = true,
            "CONTENT_FILTER" => {
                return Err(Error::http(400, "DeepSeek 网页因内容策略结束了本次响应")
                    .with_code("deepseek_content_blocked"))
            }
            "INCOMPLETE" | "AUTO_CONTINUE" => {
                return Err(Error::http(
                    502,
                    "DeepSeek 网页响应尚未完成，需要上游续写；当前插件未自动重复提交",
                )
                .with_code("deepseek_incomplete"))
            }
            "FAILED" | "ERROR" | "CANCELED" | "CANCELLED" => {
                return Err(Error::http(502, "DeepSeek 网页提前终止了本次响应")
                    .with_code("deepseek_incomplete"))
            }
            _ => {}
        }
        Ok(())
    }

    fn usage(&mut self, value: &Value) -> Result<()> {
        if value.is_null() {
            return Ok(());
        }
        let total = value
            .as_u64()
            .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
            .ok_or_else(malformed)?;
        self.total_tokens = Some(total);
        Ok(())
    }

    fn snapshot(&mut self, response: &Value) -> Result<()> {
        self.status(&response["status"], false)?;
        self.status(&response["quasi_status"], true)?;
        self.usage(&response["accumulated_token_usage"])?;
        if let Some(fragments) = response.get("fragments") {
            self.fragments(fragments, false)?;
        }
        if let Some(thinking) = response.get("thinking_enabled").and_then(Value::as_bool) {
            if self.fragments.is_empty() {
                self.implicit_reasoning = thinking;
            }
        }
        if let Some(value) = response
            .get("thinking_content")
            .filter(|value| !value.is_null())
        {
            self.direct(Kind::Think, value, "SET")?;
        }
        if let Some(value) = response.get("content").filter(|value| !value.is_null()) {
            self.direct(Kind::Response, value, "SET")?;
        }
        Ok(())
    }

    fn patch(&mut self, value: &Value, base: &str, depth: usize) -> Result<()> {
        if depth > 8 {
            return Err(malformed());
        }
        let own = value["p"].as_str().unwrap_or("");
        let path = if own.is_empty() {
            base.to_string()
        } else if base.is_empty() || own.starts_with("response/") || own == "response" {
            own.to_string()
        } else {
            format!("{base}/{own}")
        };
        let operation = value["o"].as_str().unwrap_or("");
        let data = &value["v"];
        if operation == "BATCH"
            || own.is_empty()
                && data
                    .as_array()
                    .is_some_and(|items| items.iter().all(|item| item.get("p").is_some()))
        {
            let patches = data
                .as_array()
                .filter(|patches| patches.len() <= MAX_FRAGMENTS)
                .ok_or_else(malformed)?;
            for patch in patches {
                self.patch(patch, &path, depth + 1)?;
            }
            return Ok(());
        }
        match path.as_str() {
            "response/status" | "status" => self.status(data, false)?,
            "response/quasi_status" | "quasi_status" => self.status(data, true)?,
            "response/accumulated_token_usage" | "accumulated_token_usage" => self.usage(data)?,
            "response/thinking_content" | "thinking_content" => {
                self.direct(Kind::Think, data, operation)?
            }
            "response/content" | "content" => self.direct(Kind::Response, data, operation)?,
            "response/fragments" | "fragments" => match operation {
                "APPEND" => self.fragments(data, true)?,
                "SET" | "" => self.fragments(data, false)?,
                _ => return Err(malformed()),
            },
            "response" if data.is_object() => self.snapshot(data)?,
            "" if data.is_string()
                || data.get("text").is_some()
                || data.get("content").is_some() =>
            {
                self.text(None, data, operation)?
            }
            "response/auto_continue" | "auto_continue" if data.as_bool() == Some(true) => {
                return Err(
                    Error::http(502, "DeepSeek 网页要求继续生成，当前插件未自动重复提交")
                        .with_code("deepseek_incomplete"),
                );
            }
            _ => {
                if let Some(index) = path
                    .strip_prefix("response/fragments/")
                    .or_else(|| path.strip_prefix("fragments/"))
                    .and_then(|path| path.strip_suffix("/content"))
                {
                    let at = if index == "-1" {
                        self.fragments.len().checked_sub(1)
                    } else {
                        Some(index.parse::<usize>().map_err(|_| malformed())?)
                    };
                    self.text(at, data, operation)?;
                }
                // title、会话 id、速度等未知元数据不作为可见正文。
            }
        }
        Ok(())
    }

    fn consume(&mut self, event: &Event, session: &Session) -> Result<()> {
        // 错误事件优先于结束标记，不能因为载荷碰巧写着 [DONE] 就变成成功。
        if event.name == "error" {
            let value = serde_json::from_str::<Value>(&event.data).unwrap_or(Value::Null);
            return Err(errors::upstream(200, &value, session));
        }
        if event.data.trim() == "[DONE]" {
            self.finished = true;
            self.closed = true;
            return Ok(());
        }
        if !event.data.trim().is_empty() {
            let value: Value = serde_json::from_str(&event.data).map_err(|_| malformed())?;
            if errors::has_error(&value) {
                return Err(errors::upstream(200, &value, session));
            }
            if event.name == "ready" {
                // ready 只给消息ID，不代表启用了思考；使用请求的真实开关。
                if let Some(response) = value
                    .pointer("/v/response")
                    .or_else(|| value.get("response"))
                {
                    self.snapshot(response)?;
                }
            } else if matches!(event.name.as_str(), "" | "message" | "finish" | "close") {
                self.status(&value["status"], false)?;
                self.status(&value["quasi_status"], true)?;
                if let Some(response) = value
                    .pointer("/v/response")
                    .or_else(|| value.get("response"))
                {
                    self.snapshot(response)?;
                } else if value.get("v").is_none()
                    && (value.get("content").is_some() || value.get("thinking_content").is_some())
                {
                    self.snapshot(&value)?;
                } else if matches!(event.name.as_str(), "" | "message")
                    || value.get("p").is_some()
                    || value["v"].is_array()
                {
                    self.patch(&value, "", 0)?;
                }
            }
        }
        if event.name == "close" {
            // 关闭事件也可出现在中断流，只有FINISHED/finish/[DONE]才确认成功。
            self.closed = true;
        }
        if event.name == "finish" {
            self.finished = true;
        }
        Ok(())
    }
}

pub struct Stream {
    decoder: Decoder,
    id: String,
    model: String,
    created: i64,
    pub started: bool,
}

impl Stream {
    pub fn new(model: &str, thinking: bool) -> Result<Self> {
        Ok(Self {
            decoder: Decoder {
                implicit_reasoning: thinking,
                ..Decoder::default()
            },
            id: util::id("chatcmpl")?,
            model: model.into(),
            created: util::now()? / 1000,
            started: false,
        })
    }

    fn delta(&self, value: Value, finish: Option<&str>) -> Result<()> {
        sdk::stream_event(
            &json!({ "id": self.id, "object": "chat.completion.chunk", "created": self.created, "model": self.model,
            "choices": [{ "index": 0, "delta": value, "finish_reason": finish }] }),
        )
    }

    fn start(&mut self) -> Result<()> {
        if self.started {
            return Ok(());
        }
        sdk::stream_head(200)?;
        self.started = true;
        self.delta(json!({ "role": "assistant" }), None)
    }

    pub fn consume(&mut self, event: &Event, session: &Session) -> Result<()> {
        let before_content = self.decoder.content.len();
        let before_reasoning = self.decoder.reasoning.len();
        self.decoder.consume(event, session)?;
        if self.decoder.content.len() != before_content
            || self.decoder.reasoning.len() != before_reasoning
        {
            self.start()?;
            let mut delta = json!({});
            if self.decoder.content.len() > before_content {
                delta["content"] = json!(&self.decoder.content[before_content..]);
            }
            if self.decoder.reasoning.len() > before_reasoning {
                delta["reasoning_content"] = json!(&self.decoder.reasoning[before_reasoning..]);
            }
            self.delta(delta, None)?;
        }
        Ok(())
    }

    pub fn closed(&self) -> bool {
        self.decoder.closed
    }

    pub fn finish(&mut self) -> Result<()> {
        if !self.decoder.finished {
            return Err(Error::http(502, "DeepSeek 网页流在完成前结束，响应被截断"));
        }
        if self.decoder.content.trim().is_empty() {
            return Err(
                Error::http(502, "DeepSeek 网页只返回思考或空响应，没有最终回答")
                    .with_code("deepseek_incomplete"),
            );
        }
        self.start()?;
        self.delta(json!({}), Some("stop"))?;
        if let Some(total) = self.decoder.total_tokens {
            // 上游只提供总数时不臆造输入/输出拆分；显式 0 也保留。
            sdk::stream_event(
                &json!({ "id": self.id, "object": "chat.completion.chunk", "model": self.model,
                "choices": [], "usage": { "total_tokens": total } }),
            )?;
        }
        sdk::stream_bytes(b"data: [DONE]\n\n")
    }
}
