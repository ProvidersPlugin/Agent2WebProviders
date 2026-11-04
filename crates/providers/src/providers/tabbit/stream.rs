//! Tabbit v3 房间事件 → OpenAI chat.completion.chunk。
//!
//! ── 上游事件形状（实测）─────────────────────────────────────
//! `POST /api/v3/chat/rooms/{id}/join` 的事件流，每帧是
//! `event: <type>` + `data: {"event_type":…,"payload":{…},"created_at":…}`：
//!
//! ```text
//!   snapshot_begin / snapshot_end / active_runs_snapshot / message_history  订阅快照，忽略
//!   run_queued / run_started / user_message_created                          控制帧
//!   assistant_message_started                                                控制帧
//!   event_message_chunk  payload.chunk_type=thinking → chunk_payload.reasoning_content
//!   assistant_message_delta  payload.delta（或 chunk_payload.content）       正文增量
//!   assistant_message_finished / run_completed                               结束
//!   run_failed / error                                                       流内错误
//! ```
//!
//! ── 为什么先窥一帧再发 head ─────────────────────────────────
//! 上游常用 **HTTP 200 + run_failed/error 事件**表达权益不足（492）或排队
//! 拒绝。只看 HTTP 状态会把它记成成功，账号也就不会轮换。所以这里在发
//! head 之前先读到「第一条正文或错误」；读到错误就把整次请求作为控制面
//! 错误返回（网关据此换号），读到正文才发 200 并开始逐帧转发。
//!
//! ── 不做工具调用模拟 ────────────────────────────────────────
//! 网页 completion 没有原生 tool_calls，本适配器按 Web 包口径明确拒绝
//! tools（见 `prompt.rs`），因此这里没有工具标记扫描器。

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::ConversationSink;
use futures::{Stream, StreamExt};
use serde_json::{json, Value};

use super::sign;

/// 一条 SSE 事件。
struct Event {
    name: String,
    data: String,
}

/// 翻译结果。
enum Action {
    Ignore,
    Chunks(Vec<Value>),
    Error(String, u16),
    Done(Vec<Value>),
}

struct State {
    id: String,
    created: i64,
    model: String,
    started: bool,
    finished: bool,
    usage: Option<Value>,
}

impl State {
    fn new(model: &str) -> Self {
        Self {
            id: format!("chatcmpl-{}", sign::random_uuid().replace('-', "")),
            created: super::client::now() / 1000,
            model: model.to_string(),
            started: false,
            finished: false,
            usage: None,
        }
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        })
    }

    fn ensure_role(&mut self, out: &mut Vec<Value>) {
        if self.started {
            return;
        }
        self.started = true;
        out.push(self.chunk(json!({ "role": "assistant", "content": "" }), None));
    }

    fn content(&mut self, text: &str, out: &mut Vec<Value>) {
        if text.is_empty() {
            return;
        }
        self.ensure_role(out);
        out.push(self.chunk(json!({ "content": text }), None));
    }

    fn reasoning(&mut self, text: &str, out: &mut Vec<Value>) {
        if text.is_empty() {
            return;
        }
        self.ensure_role(out);
        out.push(self.chunk(json!({ "reasoning_content": text }), None));
    }

    fn finish(&mut self, reason: &str) -> Vec<Value> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut out = Vec::new();
        self.ensure_role(&mut out);
        let reason = if reason.is_empty() { "stop" } else { reason };
        out.push(self.chunk(json!({}), Some(reason)));
        if let Some(usage) = self.usage.clone() {
            out.push(json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": self.created,
                "model": self.model,
                "choices": [],
                "usage": usage,
            }));
        }
        out
    }
}

fn text_of(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|text| !text.is_empty())
        .unwrap_or("")
        .to_string()
}

fn usage_of(value: &Value) -> Option<Value> {
    let source = value
        .get("token_usage")
        .filter(|nested| nested.is_object())
        .unwrap_or(value);
    let prompt = ["input_tokens", "prompt_tokens"]
        .iter()
        .find_map(|key| super::client::as_i64(source.get(*key).unwrap_or(&Value::Null)));
    let completion = ["output_tokens", "completion_tokens"]
        .iter()
        .find_map(|key| super::client::as_i64(source.get(*key).unwrap_or(&Value::Null)));
    if prompt.is_none() && completion.is_none() {
        return None;
    }
    let prompt = prompt.unwrap_or(0);
    let completion = completion.unwrap_or(0);
    Some(json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": prompt + completion,
    }))
}

fn error_status(raw: &str, message: &str) -> u16 {
    let lower = raw.to_ascii_lowercase();
    let message_lower = message.to_ascii_lowercase();
    let all = format!("{lower}\n{message_lower}");
    if all.contains("insufficient")
        || all.contains("no_quota")
        || all.contains("quota")
        || all.contains("积分不足")
        || all.contains("余额不足")
        || all.contains("额度")
        || all.contains("premium")
        || all.contains("upgrade")
        || all.contains("492")
    {
        return 429;
    }
    if all.contains("rate limit") || all.contains("too many") || all.contains("429") {
        return 429;
    }
    if all.contains("unauthorized") || all.contains("401") || all.contains("token") {
        return 401;
    }
    if all.contains("session is processing")
        || all.contains("正在处理")
        || all.contains("处理中")
        || all.contains("409")
    {
        return 409;
    }
    if all.contains("serviceavailable") && all.contains("false") {
        return 503;
    }
    502
}

fn translate(event: &Event, run_id: &str, state: &mut State) -> Action {
    let raw = event.data.trim();
    if raw.is_empty() || raw == "[DONE]" {
        return Action::Ignore;
    }
    let payload: Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(_) => return Action::Ignore,
    };
    let mut name = event.name.trim().to_string();
    if name.is_empty() {
        name = text_of(&payload, &["type", "event", "event_type"]);
    }
    let inner = payload.get("payload").filter(|value| value.is_object());
    // 用量可能挂在顶层、payload 或 chunk_payload 任一层（v3 实测位置不稳定）。
    if let Some(usage) = usage_of(&payload) {
        state.usage = Some(usage);
    }
    if let Some(inner) = inner {
        if let Some(usage) = usage_of(inner) {
            state.usage = Some(usage);
        }
        if let Some(chunk) = inner.get("chunk_payload") {
            if let Some(usage) = usage_of(chunk) {
                state.usage = Some(usage);
            }
        }
        if let Some(run) = inner.get("run_id").and_then(Value::as_str) {
            if !run.is_empty() && !run_id.is_empty() && run != run_id {
                return Action::Ignore;
            }
        }
        match name.as_str() {
            "event_message_chunk" => {
                let kind = text_of(inner, &["chunk_type"]);
                let chunk = inner.get("chunk_payload").unwrap_or(&Value::Null);
                return match kind.as_str() {
                    "thinking" => {
                        let mut out = Vec::new();
                        state.reasoning(
                            &text_of(chunk, &["reasoning_content", "content"]),
                            &mut out,
                        );
                        Action::Chunks(out)
                    }
                    "thinking_finished" => Action::Ignore,
                    "error" => {
                        let message = text_of(chunk, &["message", "error_message", "detail"]);
                        Action::Error(message, error_status(raw, &text_of(inner, &["message"])))
                    }
                    _ => {
                        let mut out = Vec::new();
                        state.content(&text_of(chunk, &["content"]), &mut out);
                        Action::Chunks(out)
                    }
                };
            }
            "assistant_message_delta" => {
                let mut text = text_of(inner, &["delta"]);
                if text.is_empty() {
                    if let Some(chunk) = inner.get("chunk_payload") {
                        text = text_of(chunk, &["content"]);
                    }
                }
                let mut out = Vec::new();
                state.content(&text, &mut out);
                return Action::Chunks(out);
            }
            "assistant_message_finished" => return Action::Ignore,
            "run_completed" => {
                if let Some(usage) = usage_of(inner) {
                    state.usage = Some(usage);
                }
                let chunks = state.finish("stop");
                return Action::Done(chunks);
            }
            "run_failed" | "run_cancelled" => {
                let message = {
                    let text = text_of(inner, &["message", "error_message", "reason", "detail"]);
                    if text.is_empty() {
                        text_of(inner, &["status"])
                    } else {
                        text
                    }
                };
                let message = if message.is_empty() {
                    "Tabbit 上游拒绝了本次请求".to_string()
                } else {
                    message
                };
                return Action::Error(message.clone(), error_status(raw, &message));
            }
            "snapshot_begin" | "snapshot_end" | "message_history" | "active_runs_snapshot"
            | "run_queued" | "run_started" | "user_message_created"
            | "assistant_message_started" | "session_title_updated" | "room_reset_required" => {
                if name == "run_queued" {
                    let flat = raw
                        .to_ascii_lowercase()
                        .replace([' ', '\t', '\r', '\n'], "");
                    if flat.contains("\"serviceavailable\":false") {
                        return Action::Error(
                            "Tabbit 上游该模型此刻不可服务（排队拒绝），请稍后重试".to_string(),
                            503,
                        );
                    }
                }
                return Action::Ignore;
            }
            _ => {}
        }
    }
    // 非 v3 的扁平事件（兼容旧房间或心跳）。
    match name.as_str() {
        "thinking" => {
            let mut out = Vec::new();
            state.reasoning(&text_of(&payload, &["reasoning_content", "content", "text"]), &mut out);
            Action::Chunks(out)
        }
        "message_chunk" | "message" | "delta" => {
            let mut text = text_of(&payload, &["content", "text", "delta"]);
            if text.is_empty() {
                if let Some(delta) = payload.get("delta") {
                    text = text_of(delta, &["content", "text"]);
                }
            }
            let mut out = Vec::new();
            state.content(&text, &mut out);
            Action::Chunks(out)
        }
        "usage" => {
            if let Some(usage) = usage_of(&payload) {
                state.usage = Some(usage);
            }
            Action::Ignore
        }
        "error" => {
            let message = {
                let text = text_of(&payload, &["message", "msg", "error_message", "displayTips"]);
                if text.is_empty() {
                    text_of(payload.get("error").unwrap_or(&Value::Null), &["message"])
                } else {
                    text
                }
            };
            Action::Error(message, error_status(raw, ""))
        }
        "message_finish" | "finish" | "close" => {
            let reason = text_of(&payload, &["finish_reason", "stop_reason"]);
            let chunks = state.finish(&reason);
            Action::Done(chunks)
        }
        "cancel" => {
            let chunks = state.finish("stop");
            Action::Done(chunks)
        }
        "ready" | "ttft" | "title" | "switch_model" | "message_start" | "thinking_finished"
        | "risky" => Action::Ignore,
        _ => Action::Ignore,
    }
}

async fn next_event<S, T>(
    source: &mut S,
    buffer: &mut Vec<u8>,
    eof: &mut bool,
) -> Result<Option<Event>, String>
where
    S: Stream<Item = Result<T, reqwest::Error>> + Unpin,
    T: AsRef<[u8]>,
{
    loop {
        if let Some(frame) = take_frame(buffer) {
            return Ok(parse_frame(&frame));
        }
        if *eof {
            if buffer.is_empty() {
                return Ok(None);
            }
            let frame = std::mem::take(buffer);
            return Ok(parse_frame(&frame));
        }
        match source.next().await {
            Some(Ok(chunk)) => buffer.extend_from_slice(chunk.as_ref()),
            Some(Err(_)) => return Err("Tabbit 对话流传输中断".to_string()),
            None => *eof = true,
        }
    }
}

/// 取出一个完整帧（以空行结尾）；不足一帧返回 None。
fn take_frame(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let mut line_start = 0usize;
    let mut index = 0usize;
    while index < buffer.len() {
        if buffer[index] == b'\n' {
            let mut end = index;
            if end > line_start && buffer[end - 1] == b'\r' {
                end -= 1;
            }
            if end == line_start {
                let frame = buffer[..line_start].to_vec();
                buffer.drain(..=index);
                return Some(frame);
            }
            line_start = index + 1;
        }
        index += 1;
    }
    None
}

fn parse_frame(frame: &[u8]) -> Option<Event> {
    if frame.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(frame);
    let mut name = String::new();
    let mut data = Vec::new();
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if let Some(value) = line.strip_prefix("event:") {
            name = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value).to_string());
        }
    }
    if name.is_empty() && data.is_empty() {
        return None;
    }
    Some(Event { name, data: data.join("\n") })
}

async fn write_chunks(sink: &mut ConversationSink<'_>, chunks: &[Value]) -> Result<(), String> {
    for chunk in chunks {
        sink.send(format!("data: {chunk}\n\n").as_bytes()).await?;
    }
    Ok(())
}

async fn write_done(sink: &mut ConversationSink<'_>) -> Result<(), String> {
    sink.send(b"data: [DONE]\n\n").await
}

fn error_frame(message: &str) -> Value {
    json!({ "error": { "message": message, "type": "upstream_error" } })
}

/// 读取上游事件流并转发。head 只在这条函数内部、确认没有预置错误之后才发。
pub async fn forward(
    response: reqwest::Response,
    run_id: String,
    model: String,
    sink: &mut ConversationSink<'_>,
) -> Result<(), String> {
    let mut source = Box::pin(response.bytes_stream());
    let mut buffer: Vec<u8> = Vec::new();
    let mut eof = false;
    let mut state = State::new(&model);

    // ── 先窥到第一条正文 / 错误 / 结束（最多 64 帧控制事件）──
    let mut ready: Vec<Value> = Vec::new();
    let mut done = false;
    let mut seen = 0usize;
    loop {
        let event = match next_event(&mut source, &mut buffer, &mut eof).await? {
            Some(event) => event,
            None => break,
        };
        seen += 1;
        match translate(&event, &run_id, &mut state) {
            Action::Ignore => {
                if seen >= 64 {
                    break;
                }
            }
            Action::Chunks(chunks) => {
                ready.extend(chunks);
                break;
            }
            Action::Done(chunks) => {
                ready.extend(chunks);
                done = true;
                break;
            }
            Action::Error(message, status) => {
                let message = if message.trim().is_empty() {
                    "Tabbit 上游拒绝了本次请求".to_string()
                } else {
                    message
                };
                return Err(crate::errors::GatewayError::with_status(i32::from(status), message)
                    .into_control_message());
            }
        }
    }

    sink.head(proto::StreamHead {
        status: 200,
        headers: [("content-type".to_string(), "text/event-stream".to_string())]
            .into_iter()
            .collect(),
        upstream: proto::UpstreamKind::Chat,
    })
    .await?;
    write_chunks(sink, &ready).await?;
    if done {
        return write_done(sink).await;
    }

    // ── 继续读剩余事件 ─────────────────────────────────────
    loop {
        let event = match next_event(&mut source, &mut buffer, &mut eof).await {
            Ok(Some(event)) => event,
            Ok(None) => {
                let chunks = state.finish("stop");
                write_chunks(sink, &chunks).await?;
                return write_done(sink).await;
            }
            Err(_) => {
                let message = "Tabbit 对话流传输中断";
                write_chunks(sink, &[error_frame(message)]).await?;
                return write_done(sink).await;
            }
        };
        match translate(&event, &run_id, &mut state) {
            Action::Ignore => {}
            Action::Chunks(chunks) => write_chunks(sink, &chunks).await?,
            Action::Done(chunks) => {
                write_chunks(sink, &chunks).await?;
                return write_done(sink).await;
            }
            Action::Error(message, _) => {
                let message = if message.trim().is_empty() {
                    "Tabbit 上游拒绝了本次请求"
                } else {
                    message.as_str()
                };
                write_chunks(sink, &[error_frame(message)]).await?;
                return write_done(sink).await;
            }
        }
    }
}

