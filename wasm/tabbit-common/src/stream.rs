//! v3 房间事件转为标准 Chat SSE；只在收到本轮有效输出后发 200。
//! 原生协议中 assistant_message_finished 不是 run 结束，run_completed 才是。
//! 先核对错误再记录终态，成功尾帧延后到已接收缓存检查完成；不等房间订阅 EOF。

use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};

use crate::{
    credentials::Credentials,
    errors,
    network::Client,
    sign,
    sse::{Event, Reader},
};

const MAX_OUTPUT: usize = 8 * 1024 * 1024;
const MAX_TAIL_BYTES: usize = 64 * 1024;
const MAX_TAIL_EVENTS: usize = 64;
const MAX_TAIL_MS: i64 = 1000;
const MAX_TOKENS: u64 = (1_u64 << 53) - 1;

struct State {
    id: String,
    created: i64,
    model: String,
    role_sent: bool,
    head_sent: bool,
    room_protocol: bool,
    terminal: Option<String>,
    usage: Option<Value>,
    output_size: usize,
}

impl State {
    fn new(model: &str) -> Result<Self> {
        Ok(Self {
            id: format!("chatcmpl-{}", sign::random_uuid()?.replace('-', "")),
            created: stream_time()? / 1000,
            model: model.to_string(),
            role_sent: false,
            head_sent: false,
            room_protocol: false,
            terminal: None,
            usage: None,
            output_size: 0,
        })
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        json!({ "id": self.id, "object": "chat.completion.chunk", "created": self.created,
            "model": self.model, "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }] })
    }

    fn start(&mut self) -> Result<()> {
        if !self.head_sent {
            sdk::stream_head(200)?;
            self.head_sent = true;
        }
        if !self.role_sent {
            sdk::stream_event(&self.chunk(json!({ "role": "assistant", "content": "" }), None))?;
            self.role_sent = true;
        }
        Ok(())
    }

    fn text(&mut self, text: &str, reasoning: bool) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        if self.terminal.is_some() {
            return Err(Error::http(502, "Tabbit 完成事件后仍返回正文，结果未确认"));
        }
        self.output_size = self.output_size.saturating_add(text.len());
        if self.output_size > MAX_OUTPUT {
            return Err(Error::http(502, "Tabbit 回答超过大小上限"));
        }
        self.start()?;
        for text in text_chunks(text, 16 * 1024) {
            let delta = if reasoning {
                json!({ "reasoning_content": text })
            } else {
                json!({ "content": text })
            };
            sdk::stream_event(&self.chunk(delta, None))?;
        }
        Ok(())
    }

    fn mark_terminal(&mut self, reason: &str) -> Result<()> {
        // 原生 finish 会补空 assistant，但没有有效空回答的协议证据；保留 WASM 的拒绝。
        if self.output_size == 0 {
            return Err(Error::http(502, "Tabbit 返回了空回答，未作为成功结果转发"));
        }
        let reason = if reason.is_empty() { "stop" } else { reason };
        if self
            .terminal
            .as_deref()
            .is_some_and(|previous| previous != reason)
        {
            return Err(Error::http(502, "Tabbit 完成事件互相冲突，结果未确认"));
        }
        self.terminal = Some(reason.to_string());
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        let reason = self
            .terminal
            .clone()
            .ok_or_else(|| Error::http(502, "Tabbit 对话尚未确认完成"))?;
        self.start()?;
        sdk::stream_event(&self.chunk(json!({}), Some(&reason)))?;
        if let Some(usage) = &self.usage {
            sdk::stream_event(
                &json!({ "id": self.id, "object": "chat.completion.chunk", "created": self.created,
                "model": self.model, "choices": [], "usage": usage }),
            )?;
        }
        sdk::stream_bytes(b"data: [DONE]\n\n")
    }

    fn observe_usage(&mut self, value: &Value) {
        let source = match value.get("token_usage") {
            Some(source) if source.is_object() => {
                self.usage = None;
                source
            }
            Some(_) => {
                self.usage = None;
                return;
            }
            None => value,
        };
        if ![
            "input_tokens",
            "prompt_tokens",
            "output_tokens",
            "completion_tokens",
            "total_tokens",
        ]
        .iter()
        .any(|key| source.get(*key).is_some())
        {
            return;
        }
        // 不用额度查询的整数转换（会截小数）；不拼前后两帧，不用旧完整值掩盖新残缺值。
        self.usage = None;
        let Some(prompt) = token_alias(source, &["input_tokens", "prompt_tokens"]) else {
            return;
        };
        let Some(completion) = token_alias(source, &["output_tokens", "completion_tokens"]) else {
            return;
        };
        let Some(total) = prompt
            .checked_add(completion)
            .filter(|total| *total <= MAX_TOKENS)
        else {
            return;
        };
        if source
            .get("total_tokens")
            .is_some_and(|value| token_count(value) != Some(total))
        {
            return;
        }
        self.usage = Some(
            json!({ "prompt_tokens": prompt, "completion_tokens": completion,
            "total_tokens": total }),
        );
    }
}

fn token_count(value: &Value) -> Option<u64> {
    match value {
        // 只接受 JSON 整数，避免浮点舍入/下溢把未知小数变成真实的整数或零。
        Value::Number(number) => number.as_u64(),
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            text.parse().ok()
        }
        _ => None,
    }
    .filter(|value| *value <= MAX_TOKENS)
}

fn token_alias(source: &Value, keys: &[&str]) -> Option<u64> {
    let mut found = None;
    for key in keys {
        if let Some(value) = source.get(*key) {
            let value = token_count(value)?;
            if found.is_some_and(|previous| previous != value) {
                return None;
            }
            found = Some(value);
        }
    }
    found
}

// 单帧文本小于宿主的 credit 窗口，避免编码为 SSE 后挤满回压窗口。
fn text_chunks(mut text: &str, limit: usize) -> Vec<&str> {
    let mut chunks = Vec::new();
    while !text.is_empty() {
        let mut end = text.len().min(limit);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(&text[..end]);
        text = &text[end..];
    }
    chunks
}

// 文本增量不能 trim，空格/换行可能就是单独一帧；控制字段才 trim。
fn content(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|key| value[*key].as_str())
        .find(|text| !text.is_empty())
        .unwrap_or("")
        .to_string()
}
fn label(value: &Value, keys: &[&str]) -> String {
    crate::credentials::text(value, keys)
}

fn failure(status: u16, message: &str, creds: &Credentials) -> Error {
    errors::upstream(status, &json!({ "message": message }), creds)
}

fn failed_name(name: &str) -> bool {
    matches!(name, "error" | "run_failed" | "run_cancelled" | "cancel")
}

fn error_signal(value: &Value) -> bool {
    [
        "type",
        "event",
        "event_type",
        "chunk_type",
        "finish_reason",
        "stop_reason",
    ]
    .iter()
    .any(|key| {
        value[*key]
            .as_str()
            .is_some_and(|name| failed_name(name.trim()))
    }) || ["error", "error_message"].iter().any(|key| {
        value.get(*key).is_some_and(|error| {
            !error.is_null()
                && error != &Value::Bool(false)
                && error.as_str().is_none_or(|text| !text.trim().is_empty())
        })
    })
}

/// run 的两个原生位置先核对，再读错误/用量/终态，不能把其它 run 混入本轮。
fn belongs(payload: &Value, run_id: &str) -> Result<bool> {
    let mut found: Option<&str> = None;
    for source in [Some(payload), payload.get("payload")]
        .into_iter()
        .flatten()
    {
        for key in ["run_id", "runId"] {
            let Some(value) = source.get(key).filter(|value| !value.is_null()) else {
                continue;
            };
            let Some(run) = value.as_str() else {
                return Err(Error::http(502, "Tabbit 流式事件的 run 标识格式无效"));
            };
            if run.len() > 1024 || run.chars().any(char::is_control) {
                return Err(Error::http(502, "Tabbit 流式事件的 run 标识格式无效"));
            }
            let run = run.trim();
            if run.is_empty() {
                continue;
            }
            if found.is_some_and(|previous| previous != run) {
                return Err(Error::http(502, "Tabbit 流式事件的 run 标识互相冲突"));
            }
            found = Some(run);
        }
    }
    Ok(found.is_none_or(|run| run == run_id))
}

fn event_failure(payload: &Value, name: &str, creds: &Credentials) -> Error {
    let empty = Value::Null;
    let inner = payload.get("payload").unwrap_or(&empty);
    let chunk = inner.get("chunk_payload").unwrap_or(&empty);
    let message = [chunk, inner, payload]
        .iter()
        .find_map(|value| {
            let text = errors::text(value);
            (!text.is_empty()).then_some(text)
        })
        .or_else(|| {
            [chunk, inner, payload].iter().find_map(|value| {
                let text = label(value, &["reason", "status"]);
                (!text.is_empty()).then_some(text)
            })
        })
        .unwrap_or_else(|| {
            if matches!(name, "run_cancelled" | "cancel") {
                "Tabbit 上游取消了本次回答".to_string()
            } else {
                "Tabbit 上游返回了错误事件，结果未确认".to_string()
            }
        });
    // 元数据保留原有协议结构，不能先折成 message 再拿 HTTP 分类码冒充业务码。
    errors::stream_upstream(payload, &message, creds)
}

fn translate(event: Event, run_id: &str, state: &mut State, creds: &Credentials) -> Result<()> {
    let raw = event.data.trim();
    let explicit_name = event.name.trim();
    // 必须先于空 data、[DONE] 与 JSON 成功分支；错误事件没有可解析正文也仍然是失败。
    if failed_name(explicit_name) {
        let payload: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
        if payload.is_object() && !belongs(&payload, run_id)? {
            return Ok(());
        }
        return Err(event_failure(&payload, explicit_name, creds));
    }
    if raw.is_empty() {
        return if explicit_name.is_empty() {
            Ok(()) // SSE 注释/空事件，不是一个完成标记。
        } else {
            Err(Error::http(502, "Tabbit 命名事件缺少有效数据，结果未确认"))
        };
    }
    if raw == "[DONE]" {
        // 原生房间协议不靠 [DONE] 完成 run；只允许已收到真实终态后的无名尾标记。
        return if explicit_name.is_empty() && state.terminal.is_some() {
            Ok(())
        } else {
            Err(Error::http(502, "Tabbit 未确认本轮完成便返回结束标记"))
        };
    }
    let payload: Value = serde_json::from_str(raw)
        .map_err(|_| Error::http(502, "Tabbit 流式事件不是有效 JSON，结果未确认"))?;
    if !payload.is_object() {
        return Err(Error::http(502, "Tabbit 流式事件不是有效对象，结果未确认"));
    }
    if !belongs(&payload, run_id)? {
        return Ok(());
    }
    let name = if explicit_name.is_empty() {
        label(&payload, &["type", "event", "event_type"])
    } else {
        explicit_name.to_string()
    };
    let inner = payload.get("payload").filter(|value| value.is_object());
    if matches!(
        name.as_str(),
        "snapshot_begin" | "snapshot_end" | "message_history" | "active_runs_snapshot"
    ) {
        state.room_protocol = true;
        return Ok(()); // 快照里的历史错误/终态不能当本轮事实。
    }
    // 外层 event 是 finish 也不能覆盖同帧的 error、run_failed 或 chunk_type=error。
    if failed_name(&name)
        || error_signal(&payload)
        || inner.is_some_and(|value| error_signal(value) || error_signal(&value["chunk_payload"]))
    {
        return Err(event_failure(&payload, &name, creds));
    }
    state.observe_usage(&payload);
    if let Some(inner) = inner {
        state.observe_usage(inner);
        state.observe_usage(&inner["chunk_payload"]);
    }
    let v3 = matches!(
        name.as_str(),
        "event_message_chunk"
            | "assistant_message_delta"
            | "assistant_message_finished"
            | "run_completed"
            | "run_queued"
            | "run_started"
            | "user_message_created"
            | "assistant_message_started"
            | "session_title_updated"
            | "room_reset_required"
    );
    if v3 {
        state.room_protocol = true;
        let inner =
            inner.ok_or_else(|| Error::http(502, "Tabbit 房间事件缺少有效 payload，结果未确认"))?;
        match name.as_str() {
            "event_message_chunk" => {
                let chunk = inner
                    .get("chunk_payload")
                    .filter(|chunk| chunk.is_object())
                    .ok_or_else(|| Error::http(502, "Tabbit 消息事件缺少有效 chunk_payload"))?;
                match label(inner, &["chunk_type"]).as_str() {
                    "thinking" => {
                        state.text(&content(chunk, &["reasoning_content", "content"]), true)?
                    }
                    "thinking_finished" => {}
                    _ => state.text(&content(chunk, &["content"]), false)?,
                }
            }
            "assistant_message_delta" => {
                let delta = content(inner, &["delta"]);
                let delta = if delta.is_empty() {
                    content(&inner["chunk_payload"], &["content"])
                } else {
                    delta
                };
                state.text(&delta, false)?;
            }
            "run_completed" => state.mark_terminal("stop")?,
            "run_queued" => {
                if raw
                    .to_ascii_lowercase()
                    .replace([' ', '\t', '\r', '\n'], "")
                    .contains("\"serviceavailable\":false")
                {
                    return Err(failure(
                        503,
                        "Tabbit 上游该模型此刻不可服务，请稍后重试",
                        creds,
                    ));
                }
            }
            // assistant_message_finished 仍非本轮终态，必须继续等 run_completed。
            _ => {}
        }
        return Ok(());
    }
    // 原生已支持的旧房间扁平事件。已确认 v3 后，旧消息结束不能冒充 run 完成。
    match name.as_str() {
        "thinking" => state.text(
            &content(&payload, &["reasoning_content", "content", "text"]),
            true,
        )?,
        "message_chunk" | "message" | "delta" => {
            let delta = content(&payload, &["content", "text", "delta"]);
            let delta = if delta.is_empty() {
                content(&payload["delta"], &["content", "text"])
            } else {
                delta
            };
            state.text(&delta, false)?;
        }
        "message_finish" | "finish" | "close" => {
            if !state.room_protocol {
                state.mark_terminal(&label(&payload, &["finish_reason", "stop_reason"]))?;
            } else if name == "close" && state.terminal.is_none() {
                return Err(Error::http(502, "Tabbit 房间在本轮完成前关闭，结果未确认"));
            }
        }
        _ => {}
    }
    Ok(())
}

fn stream_time() -> Result<i64> {
    let now = sdk::now();
    if now > 0 {
        Ok(now)
    } else {
        Err(Error::http(502, "Tabbit 对话时间无法核对，结果未确认"))
    }
}

fn check_tail(
    reader: &mut Reader,
    run_id: &str,
    state: &mut State,
    creds: &Credentials,
) -> Result<()> {
    // join 是长期房间订阅：成功事件之后不调用 next/http.read，不等 EOF，不发额外请求。
    // 只检查当前已接收字节；预算耗尽或残帧均明确未确认，不提前写 stop/[DONE]。
    if reader.buffered_bytes() > MAX_TAIL_BYTES {
        return Err(Error::http(502, "Tabbit 完成事件后的缓存超过大小上限"));
    }
    let started = stream_time()?;
    let mut events = 0;
    loop {
        let elapsed = stream_time()?
            .checked_sub(started)
            .filter(|elapsed| (0..=MAX_TAIL_MS).contains(elapsed));
        if elapsed.is_none() {
            return Err(Error::http(502, "Tabbit 完成事件后的核对超时，结果未确认"));
        }
        let Some(event) = reader.buffered()? else {
            break;
        };
        events += 1;
        if events > MAX_TAIL_EVENTS {
            return Err(Error::http(502, "Tabbit 完成事件后的缓存超过事件数量上限"));
        }
        translate(event, run_id, state, creds)?;
    }
    if reader.incomplete() {
        return Err(Error::http(
            502,
            "Tabbit 完成事件后的已接收尾部不完整，结果未确认",
        ));
    }
    Ok(())
}

pub struct Failure {
    pub error: Error,
    pub started: bool,
}

pub fn forward(
    response: http::Response,
    run_id: &str,
    model: &str,
    client: &Client,
) -> std::result::Result<(), Failure> {
    let creds = &client.credentials;
    let mut reader = Reader::new(response);
    let mut state = State::new(model).map_err(|error| Failure {
        error,
        started: false,
    })?;
    let result: Result<()> = (|| loop {
        let event = reader
            .next()?
            .ok_or_else(|| Error::http(502, "Tabbit 对话流未收到完成事件便结束"))?;
        client.current()?;
        translate(event, run_id, &mut state, creds)?;
        if state.terminal.is_some() {
            check_tail(&mut reader, run_id, &mut state, creds)?;
            client.current()?;
            return state.finish();
        }
    })();
    match result {
        Ok(()) => Ok(()),
        Err(mut error) => {
            error.message = creds.redact(&error.message);
            if state.head_sent {
                // 只发错误，不补成功终态；started 保持真实 head 状态，供宿主禁止重放。
                let _ = sdk::stream_event(&errors::event(&error));
            }
            Err(Failure {
                error,
                started: state.head_sent,
            })
        }
    }
    // Reader 持有的 Response 在所有返回/错误路径自动 http.close；不停止或删除远端 run。
}
