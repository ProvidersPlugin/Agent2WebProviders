//! 新请求使用独立上游会话并重放完整历史；不跨账号共享会话或凭证。
//! 网页协议归插件，逐模型声明供主程序模拟工具；不接收费 API 或自行执行工具。
//! 未确认独立换证、余额、签到与领取接口，不把未接入宣传为上游不存在。
//! 大内联图片由显式 resources.inbound-images.v1 入场后按本次资源消费，非图片 JSON 不放宽 ABI。

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod account;
mod chat;
mod errors;
mod image_gif;
mod image_input;
mod images;
mod login;
mod models;
mod network;
mod pow;
mod prompt;
mod sse;
mod stream;
mod util;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

const ID: &str = "deepseek-web";
const BASE: &str = "https://chat.deepseek.com";

sdk::export_provider!(handle);

fn metadata() -> Value {
    json!({ "id": ID, "label": "DeepSeek 网页", "order": 1,
        "capabilities": { "chat": true, "stateful": true, "web_login": true, "model_refresh": true,
            "refresh": false, "usage": false, "checkin": false, "claims": false,
            "anonymous_default_session": false, "env_credentials": false,
            "supports_default_model": false, "system_proxy_when_unset": false, "sse_model_rewrite": false,
            "refresh_uses_account": true },
        "login_methods": [
            { "kind": "browser", "button": "网页登录",
                "hint": "在桌面独立窗口中登录 DeepSeek 官网；浏览器面板请粘贴令牌。" },
            { "kind": "token", "label": "网页访问令牌",
                "placeholder": "chat.deepseek.com 的 userToken.value，不是收费 API Key", "extra_fields": [
                    { "key": "cookie", "label": "网页 Cookie", "optional": true, "multiline": true },
                    { "key": "userAgent", "label": "浏览器标识", "optional": true }
                ] }
        ],
        "account_features": { "identifier": ["uid"], "concurrency": true, "concurrency_default": 1 },
        "default_models": models::fallback()["models"] })
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
        "account.view" => account::view(&params["secret"]),
        "account.identity" => Ok(json!(account::identity(&params["secret"])?)),
        "token.ensure" => {
            let session = account::Session::load()?;
            session.check()?;
            Ok(json!(session.token))
        }
        "token.expiring" => Ok(json!(false)),
        "models.list" => models::list(&params),
        "models.refresh" => models::refresh(&params),
        "login.start" => login::start(&params),
        "login.poll" => login::poll(&params),
        "login.callback" => login::callback(&params),
        "login.cancel" => login::cancel(&params),
        "conversation" => {
            chat::forward(&params)?;
            Ok(Value::Null)
        }
        "classify" => Ok(errors::classify(
            params["status"].as_u64().unwrap_or(502) as u16,
            &params["body"],
        )),
        "is_error_response" => {
            let status = params["status"].as_u64().unwrap_or(502);
            let mime = params["headers"]
                .as_object()
                .into_iter()
                .flatten()
                .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                .and_then(|(_, value)| value.as_str())
                .unwrap_or("");
            Ok(json!(
                !(200..300).contains(&status)
                    || mime.contains("application/json")
                    || mime.contains("text/html")
            ))
        }
        "reasoning_patch" => Ok(
            json!({ "action": "skip", "reason": "DeepSeek 网页保留请求中的思考开关，不注入未声明档位" }),
        ),
        "error.policy" => Ok(json!({ "stop": false })),
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" => Ok(Value::Null),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false })),
        _ => Err(Error::unsupported()),
    }
}
