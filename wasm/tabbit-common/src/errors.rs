use crate::credentials::Credentials;
use agent2api_wasm_sdk::{json, Error, Value};

pub fn text(body: &Value) -> String {
    for key in [
        "message",
        "msg",
        "detail",
        "error_message",
        "displayTips",
        "error",
    ] {
        if let Some(text) = body[key].as_str().filter(|text| !text.trim().is_empty()) {
            return text.trim().to_string();
        }
        if let Some(text) = body[key]["details"]["message"]
            .as_str()
            .filter(|text| !text.trim().is_empty())
        {
            return text.trim().to_string();
        }
        if let Some(text) = body[key]["message"]
            .as_str()
            .filter(|text| !text.trim().is_empty())
        {
            return text.trim().to_string();
        }
    }
    String::new()
}

fn fallback(status: u16) -> &'static str {
    match status {
        401 | 403 => "Tabbit 登录态已失效，请重新导入对应站点的 Cookie",
        409 => "Tabbit 会话正在处理中，请稍后重试",
        429 => "Tabbit 请求过于频繁或当前额度不足，请稍后重试",
        492 => "Tabbit 当前账号没有该模型权限（需要 Premium 或今日额度已用完）",
        499 => "Tabbit 请求签名校验失败，请稍后重试",
        _ => "Tabbit 上游请求未成功",
    }
}

fn http_status(status: u16) -> u16 {
    match status {
        401 | 403 => 401,
        492 => 429,
        500..=599 => 502,
        400..=499 => status,
        _ => 502,
    }
}

pub fn business_failure(value: &Value) -> bool {
    value
        .get("success")
        .is_some_and(|value| value.as_bool() != Some(true))
        || ["error", "error_message"].iter().any(|key| {
            value.get(*key).is_some_and(|value| {
                !value.is_null()
                    && value != &Value::Bool(false)
                    && value.as_str().is_none_or(|text| !text.is_empty())
            })
        })
        || match response_code(value) {
            Ok(Some(code)) => code != 0,
            Ok(None) => false,
            Err(()) => true, // 异常或冲突业务码不能作为可继续写操作的成功查询。
        }
}

pub fn upstream(status: u16, body: &Value, credentials: &Credentials) -> Error {
    let code = response_code(body).ok().flatten();
    // 当前上游用 HTTP 403 + 明确业务码 492 表示付费模型，不是登录失效。
    let classified = if matches!(code, Some(401 | 403 | 409 | 429 | 492 | 499 | 503))
        && (status < 400 || status == 403 && code == Some(492))
    {
        u16::try_from(code.unwrap_or(i64::from(status))).unwrap_or(status)
    } else {
        status
    };
    let raw = text(body);
    let message: String = if classified == 401 {
        let detail: String = credentials.redact(&raw).chars().take(300).collect();
        format!("Tabbit 当前站点的登录令牌未通过校验，请重新登录当前地区并更新 Cookie；不会改用另一地区账号或无鉴权查询{}",
            if detail.is_empty() { String::new() } else { format!("（上游：{detail}）") })
    } else if raw.is_empty() {
        fallback(classified).into()
    } else {
        credentials.redact(&raw).chars().take(500).collect()
    };
    let mut error = Error::http(http_status(classified), message).with_code("tabbit_upstream");
    error.upstream_code = code.or(Some(i64::from(status)));
    if status == 409 {
        error.reset_at = conflict_reset();
    }
    error
}

fn conflict_reset() -> Option<i64> {
    let now = agent2api_wasm_sdk::now();
    (now > 0).then(|| now.checked_add(30_000)).flatten()
}

/// 仅沿用原生已有状态与文案判据；未知数字不猜成业务码，未知 429 不附加日重置时刻。
/// raw 只提供结构化错误字段，不能在 run_id、trace 或 token_usage 的序列化文本里找数字。
pub fn event_status(raw: &str, message: &str) -> u16 {
    if let Ok(value) = serde_json::from_str::<Value>(raw) {
        if let Some(status) = known_event_status(&value, 0) {
            return status;
        }
    }
    let message = message.to_lowercase();
    let words: Vec<&str> = message
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .collect();
    if words.contains(&"492") {
        return 492;
    }
    if [
        "insufficient",
        "no_quota",
        "quota",
        "积分不足",
        "余额不足",
        "额度",
        "premium",
        "upgrade",
        "rate limit",
        "too many",
    ]
    .iter()
    .any(|marker| message.contains(marker))
        || words.contains(&"429")
    {
        return 429;
    }
    if ["unauthorized", "token"]
        .iter()
        .any(|marker| message.contains(marker))
        || words.contains(&"401")
    {
        return 401;
    }
    if ["session is processing", "正在处理", "处理中"]
        .iter()
        .any(|marker| message.contains(marker))
        || words.contains(&"409")
    {
        return 409;
    }
    502
}

fn known_event_status(value: &Value, depth: usize) -> Option<u16> {
    if depth > 4 {
        return None;
    }
    if let Some(values) = value.as_array() {
        return values
            .iter()
            .find_map(|value| known_event_status(value, depth + 1));
    }
    for key in ["status", "status_code", "code", "error_code"] {
        let status = value[key]
            .as_u64()
            .and_then(|status| u16::try_from(status).ok())
            .or_else(|| {
                value[key]
                    .as_str()
                    .and_then(|status| status.parse::<u16>().ok())
            });
        if status.is_some_and(|status| matches!(status, 401 | 403 | 409 | 429 | 492 | 499 | 503)) {
            return status;
        }
    }
    if value["serviceAvailable"] == false || value["serviceavailable"] == false {
        return Some(503);
    }
    ["error", "payload", "chunk_payload"]
        .iter()
        .filter_map(|key| value.get(*key))
        .find_map(|value| known_event_status(value, depth + 1))
}

fn response_code(value: &Value) -> std::result::Result<Option<i64>, ()> {
    let mut found = None;
    // 已确认的拒绝信封：detail/details，再下一层details；不扫描正文或任意嵌套。
    for source in [
        Some(value),
        value.get("error"),
        value.get("detail"),
        value.get("details"),
        value.pointer("/detail/details"),
        value.pointer("/details/details"),
    ]
    .into_iter()
    .flatten()
    .filter(|value| value.is_object())
    {
        for key in ["code", "error_code", "upstreamCode"] {
            if let Some(value) = source.get(key).filter(|value| !value.is_null()) {
                let value = metadata_integer(value).ok_or(())?;
                merge_metadata(&mut found, value)?;
            }
        }
    }
    Ok(found)
}

#[derive(Default)]
struct EventMetadata {
    code: Option<i64>,
    status: Option<i64>,
}

fn metadata_integer(value: &Value) -> Option<i64> {
    match value {
        Value::Number(value) => value.as_i64(),
        Value::String(value) if value.len() <= 20 => {
            let digits = value.strip_prefix('-').unwrap_or(value);
            if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            value.parse().ok()
        }
        _ => None,
    }
}

fn merge_metadata(slot: &mut Option<i64>, value: i64) -> std::result::Result<(), ()> {
    if slot.is_some_and(|previous| previous != value) {
        return Err(());
    }
    *slot = Some(value);
    Ok(())
}

fn event_metadata(payload: &Value) -> std::result::Result<EventMetadata, ()> {
    let mut metadata = EventMetadata::default();
    let inner = payload.get("payload").filter(|value| value.is_object());
    let chunk = inner
        .and_then(|value| value.get("chunk_payload"))
        .filter(|value| value.is_object());
    // 固定三个协议位置及各自一层 error 对象，至多六个节点；不递归扫描正文/ID/用量。
    for source in [Some(payload), inner, chunk].into_iter().flatten() {
        for node in [
            Some(source),
            source.get("error").filter(|value| value.is_object()),
            source.get("detail").filter(|value| value.is_object()),
            source.get("details").filter(|value| value.is_object()),
            source
                .pointer("/detail/details")
                .filter(|value| value.is_object()),
        ]
        .into_iter()
        .flatten()
        {
            for key in ["code", "error_code"] {
                if let Some(value) = node.get(key).filter(|value| !value.is_null()) {
                    merge_metadata(&mut metadata.code, metadata_integer(value).ok_or(())?)?;
                }
            }
            for key in ["status", "status_code"] {
                let Some(value) = node.get(key).filter(|value| !value.is_null()) else {
                    continue;
                };
                // 原生允许 status 是状态文案；它不是数字码，也不能从中截取数字。
                if key == "status" && value.is_string() && metadata_integer(value).is_none() {
                    continue;
                }
                merge_metadata(&mut metadata.status, metadata_integer(value).ok_or(())?)?;
            }
        }
    }
    Ok(metadata)
}

/// 只在 stream 已通过本轮 run 归属核对后调用；业务码与 HTTP 分类独立保存。
/// 原生 Tabbit 流没有已证实的 resetAt 字段，不能把未知字段解释成冷却窗口。
pub fn stream_upstream(payload: &Value, message: &str, credentials: &Credentials) -> Error {
    let invalid = || {
        Error::http(502, "Tabbit 流式错误元数据格式异常或互相冲突，结果未确认")
            .with_code("tabbit_upstream")
    };
    let metadata = match event_metadata(payload) {
        Ok(metadata) => metadata,
        Err(()) => return invalid(),
    };
    let status = metadata
        .status
        .and_then(|status| u16::try_from(status).ok())
        .filter(|status| (400..600).contains(status));
    // 只沿用已证实的业务码映射；未知数字码可原样传递，但不据其猜 HTTP 状态。
    let code_status = metadata
        .code
        .and_then(|code| u16::try_from(code).ok())
        .filter(|code| matches!(code, 401 | 403 | 409 | 429 | 492 | 499 | 503));
    if let (Some(status), Some(code)) = (status, code_status) {
        // HTTP 429 + 业务码 492 是原生已有映射；401 与额度等互斥结论不能任选。
        if http_status(status) != http_status(code) && !(status == 403 && code == 492) {
            return invalid();
        }
    }
    let selected = code_status
        .or(status)
        .unwrap_or_else(|| event_status("", message));
    let mut error = upstream(selected, &json!({ "message": message }), credentials);
    // 文案分类只决定 HTTP 语义，不把推测出来的 429/401/502 写成真实业务码。
    error.upstream_code = metadata.code.or_else(|| status.map(i64::from));
    error
}

pub fn classify(status: u16, body: &Value) -> Value {
    let raw = text(body);
    let message = if raw.is_empty() {
        fallback(status).to_string()
    } else {
        raw
    };
    let code = response_code(body).ok().flatten();
    let (kind, output) = match status {
        403 if code == Some(492) => ("quota_limited", 429),
        401 | 403 => ("token_expired", 401),
        429 | 492 => ("quota_limited", 429),
        409 => ("quota_limited", 409),
        _ => (
            "fatal",
            if status >= 500 || status < 400 {
                502
            } else {
                status
            },
        ),
    };
    let upstream_code =
        code.or_else(|| matches!(status, 409 | 429 | 492).then_some(i64::from(status)));
    let reset_at = body["resetAt"].as_i64().or_else(|| {
        if status == 409 {
            conflict_reset()
        } else {
            None
        }
    });
    json!({ "kind": kind, "status": output, "message": message, "reset_at": reset_at, "upstream_code": upstream_code })
}

/// runs 已可能被接受时的失联/坏回执不是未投递证明；保留原状态码但禁止宿主换号重放。
pub fn unconfirmed(mut error: Error) -> Error {
    if matches!(
        error.code.as_deref(),
        Some("aborted" | "plugin_unavailable" | "forbidden")
    ) {
        return error;
    }
    error.code = Some("tabbit_submission_unconfirmed".into());
    error.message = format!("Tabbit 本次任务结果未确认，不自动重发：{}", error.message);
    error
}

pub fn stopped(value: &Value) -> bool {
    let decoded = value
        .as_str()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
    let value = decoded.as_ref().unwrap_or(value);
    [value, &value["error"]].iter().any(|value| {
        ["code", "providerCode"].iter().any(|key| {
            matches!(
                value[*key].as_str(),
                Some(
                    "tabbit_submission_unconfirmed"
                        | "tabbit_account_changed"
                        | "tabbit_state_conflict"
                )
            )
        })
    })
}

pub fn event(error: &Error) -> Value {
    let mut value = json!({ "error": { "message": error.message, "status": error.status.unwrap_or(502),
        "code": error.code, "type": "upstream_error" } });
    // SDK 的两个独立字段直接过线；不与 plugin_response_started 等机器标记共用 code。
    if let Some(code) = error.upstream_code {
        value["error"]["upstreamCode"] = json!(code);
    }
    if let Some(reset_at) = error.reset_at {
        value["error"]["resetAt"] = json!(reset_at);
    }
    value
}

pub fn sanitize(value: &Value, credentials: &Credentials) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| {
                    let key = key.to_lowercase().replace(['_', '-'], "");
                    ![
                        "token",
                        "cookie",
                        "password",
                        "authorization",
                        "secret",
                        "apikey",
                        "signkey",
                    ]
                    .iter()
                    .any(|part| key.contains(part))
                })
                .map(|(key, value)| (key.clone(), sanitize(value, credentials)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| sanitize(value, credentials))
                .collect(),
        ),
        Value::String(value) => Value::String(credentials.redact(value)),
        _ => value.clone(),
    }
}
