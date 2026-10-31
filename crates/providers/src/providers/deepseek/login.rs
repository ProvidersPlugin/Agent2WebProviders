use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{control_error, PluginCtx, Provider};
use serde_json::{json, Value};

struct Pending {
    expires: i64,
    processing: bool,
    name: Option<String>,
    proxy: Option<proto::WireProxy>,
}

fn pending() -> &'static Mutex<BTreeMap<String, Pending>> {
    static PENDING: OnceLock<Mutex<BTreeMap<String, Pending>>> = OnceLock::new();
    PENDING.get_or_init(Mutex::default)
}

fn failure(status: i32, message: &str) -> String {
    control_error(status, message, None, None)
}

pub fn start(payload: &Value) -> Result<proto::LoginStart, String> {
    if payload.get("methodIndex").is_some_and(|index| index.as_u64() != Some(1)) {
        return Err(failure(400, "DeepSeek 网页登录方式索引无效"));
    }
    let inputs = payload.get("inputs").cloned().unwrap_or_else(|| json!({}));
    let fields = inputs.as_object().ok_or_else(|| failure(400, "DeepSeek 登录 inputs 必须是字符串字段对象"))?;
    if fields.iter().any(|(key, value)| key != "name" || !value.is_string()) {
        return Err(failure(400, "DeepSeek 网页登录包含未申报的输入字段"));
    }
    let name = fields.get("name").and_then(Value::as_str).map(str::trim)
        .filter(|name| !name.is_empty()).map(|name| name.chars().take(100).collect());
    let proxy = match payload.get("proxy") {
        None | Some(Value::Null) => None,
        Some(value) => Some(serde_json::from_value(value.clone()).map_err(|_| failure(400, "DeepSeek 登录代理格式无效"))?),
    };
    let flow = super::id("deepseek-login");
    let now = crate::http::now_ms();
    let mut state = pending().lock().map_err(|_| failure(500, "DeepSeek 登录状态不可用"))?;
    state.retain(|_, pending| pending.expires > now);
    if state.len() >= 16 {
        return Err(failure(429, "DeepSeek 登录流程过多，请稍后重试"));
    }
    state.insert(flow.clone(), Pending { expires: now + 5 * 60 * 1000, processing: false, name, proxy });
    Ok(proto::LoginStart {
        flow,
        url: format!("{}/", super::BASE),
        instructions: "在 DeepSeek 官方页面完成登录，桌面窗口会自动读取令牌与 Cookie；验证码请在官方页面手动完成。".into(),
        poll_interval_ms: 0,
        external_browser: false,
        allowed_hosts: vec!["chat.deepseek.com".into(), "accounts.google.com".into(), "appleid.apple.com".into()],
    })
}

pub fn poll(flow: &str) -> Result<proto::LoginPoll, String> {
    let state = pending().lock().map_err(|_| failure(500, "DeepSeek 登录状态不可用"))?;
    if !state.get(flow).is_some_and(|pending| pending.expires > crate::http::now_ms()) {
        return Err(failure(400, "DeepSeek 登录已过期或取消，请重新发起"));
    }
    Ok(proto::LoginPoll::Pending { hint: Some("等待你在 DeepSeek 官方页面完成登录。".into()) })
}

pub fn cancel(flow: &str) {
    if let Ok(mut state) = pending().lock() {
        state.remove(flow);
    }
}

pub async fn callback(ctx: &PluginCtx, callback: proto::LoginCallback) -> Result<proto::LoginPoll, String> {
    if callback.code.len() > 64 * 1024 {
        return Err(failure(400, "DeepSeek 登录回调过大"));
    }
    let payload: Value = serde_json::from_str(&callback.code)
        .map_err(|_| failure(400, "DeepSeek 登录回调不是有效的凭证对象"))?;
    let fields = payload.as_object().ok_or_else(|| failure(400, "DeepSeek 登录回调必须是凭证对象"))?;
    if fields.iter().any(|(key, value)| !matches!(key.as_str(), "accessToken" | "cookie") || !value.is_string()) {
        return Err(failure(400, "DeepSeek 登录回调包含无效字段"));
    }
    let (name, proxy) = {
        let mut state = pending().lock().map_err(|_| failure(500, "DeepSeek 登录状态不可用"))?;
        let current = state.get_mut(&callback.flow)
            .filter(|pending| pending.expires > crate::http::now_ms() && !pending.processing)
            .ok_or_else(|| failure(400, "DeepSeek 登录已过期或取消，请重新发起"))?;
        current.processing = true;
        (current.name.clone(), current.proxy.clone())
    };
    let account = super::DeepseekWebProvider.normalize_account(ctx, proto::NormalizeInput {
        provider: super::ID.into(),
        source: proto::NormalizeSource::Manual { payload },
        name,
        proxy,
    }).await;
    let current = pending().lock().map_err(|_| failure(500, "DeepSeek 登录状态不可用"))?
        .remove(&callback.flow).is_some_and(|pending| pending.expires > crate::http::now_ms());
    if !current {
        return Err(failure(400, "DeepSeek 登录已取消，不会保存账号"));
    }
    Ok(proto::LoginPoll::Done { account: account? })
}
