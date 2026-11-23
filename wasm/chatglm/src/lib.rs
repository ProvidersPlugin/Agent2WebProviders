//! 智谱清言独立 WASM：业务协议归插件，网络/账号/状态调用受宿主授权。
//! 不启用隐式零账号访客；本机登录态只消费 Windows 桌面本次确认材料，不开放通用辅助调用。

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod account;
mod chat;
mod checkin;
mod claim_result;
mod credentials;
mod endpoints;
mod errors;
mod login;
mod memory;
mod models;
mod network;
mod plan;
mod sign;
mod sse;
mod stream;
mod tasks;
mod usage;

use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

sdk::export_provider!(handle);

fn metadata() -> Value {
    use sdk::error_rules::{attach, class};
    let meta = json!({ "id": "chatglm", "label": "智谱清言", "order": 0,
        "capabilities": { "chat": true, "stateful": false, "refresh": true, "model_refresh": true,
            "usage": true, "checkin": true, "claims": true, "web_login": false,
            "anonymous_default_session": false, "refresh_uses_account": true, "env_credentials": false,
            "supports_default_model": false, "system_proxy_when_unset": false, "sse_model_rewrite": false },
        "login_methods": [
            { "kind": "token", "label": "访问令牌", "placeholder": "chatglm_token、Cookie 或凭证 JSON", "extra_fields": [
                { "key": "deviceId", "label": "设备号", "optional": true },
                { "key": "refreshToken", "label": "刷新令牌", "optional": true }
            ] },
            { "kind": "guest" },
            { "kind": "desktop", "button": "导入本机登录态", "hint": "仅 Windows 本机桌面专用确认入口；每次确认只读取当前用户智谱客户端的两项登录令牌，再由上游校验。不会通过手动表单或通用辅助读取。" }
        ],
        "account_features": { "usage": true, "credit_detail": true, "identifier": ["uid"], "long_lived": true },
        "checkin": { "description": "每日登录积分、会员活动与每日任务；访客账号跳过", "daily": true, "modes": ["checkin"],
            "exclude_guest": true, "onboarding": true },
        "default_models": models::fallback()["models"] });
    attach(
        meta,
        vec![
            class("quota", &[429], &["-1001"]),
            class("auth", &[401, 403], &["40003", "40103"]),
            class("bad_request", &[400], &["40013", "40014", "40001", "40004"]),
        ],
        vec![],
    )
}

fn handle(method: &str, params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata()])),
        "provider.info" => Ok(metadata()),
        "ping" => Ok(json!(true)),
        "admin.action" if params["action"] == "auth.methods" => Ok(login::methods(
            params["payload"]["locale"].as_str().unwrap_or("zh-CN"),
        )),
        "admin.action" if params["action"] == "network.user-agent" => {
            Ok(json!({ "accountWide": true }))
        }
        "admin.action" if params["action"] == "quota.sources" => Ok(json!([])),
        "account.normalize" => account::normalize(&params),
        "account.view" => Ok(account::view(&params["secret"])),
        "account.identity" => Ok(json!(account::identity(&params["secret"]))),
        "token.ensure" => Ok(json!(account::ensure()?)),
        "token.refresh" => Ok(json!(account::refresh()?)),
        "token.expiring" => Ok(json!(credentials::expiring_within(
            &sdk::account()?["secret"],
            sdk::expiring_lead(&params, 0),
        ))),
        "models.list" if params["account"].as_str().unwrap_or("").is_empty() => {
            Ok(models::fallback())
        }
        "models.list" => models::cached(),
        "models.refresh" => models::refresh(&params),
        "conversation" => {
            chat::forward(&params)?;
            Ok(Value::Null)
        }
        "usage.query" => {
            let _lock = AccountLock::acquire()?;
            usage::query(&account::ready_locked()?)
        }
        "checkin.claim" => {
            let _lock = AccountLock::acquire()?;
            checkin::claim()
        }
        "claim.status" => {
            let _lock = AccountLock::acquire()?;
            // 已结清的新手任务不为了读本地台账额外续期/出网。
            tasks::status(
                &account::load_locked()?,
                params["refresh"].as_bool().unwrap_or(false),
            )
        }
        "claim" => {
            let _lock = AccountLock::acquire()?;
            tasks::claim(&account::load_locked()?)
        }
        "classify" => Ok(errors::classify(
            params["status"].as_u64().unwrap_or(502) as u16,
            &params["body"],
        )),
        "is_error_response" => {
            let status = params["status"].as_u64().unwrap_or(502);
            let mime = params["headers"]
                .as_object()
                .and_then(|headers| {
                    headers
                        .iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case("content-type"))
                })
                .and_then(|(_, value)| value.as_str())
                .unwrap_or("")
                .split(';')
                .next()
                .unwrap_or("")
                .trim();
            Ok(json!(
                !(200..300).contains(&status) || mime.eq_ignore_ascii_case("application/json")
            ))
        }
        "reasoning_patch" => Ok(plan::reasoning_patch(
            params["level"].as_str().unwrap_or(""),
            &params["body"],
        )),
        "error.policy" => Ok(json!({ "stop": false })),
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" => Ok(Value::Null),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false })),
        _ => Err(Error::unsupported()),
    }
}
