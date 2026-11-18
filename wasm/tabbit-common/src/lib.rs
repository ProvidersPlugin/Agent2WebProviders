//! 沿用原生 Tabbit 已有协议；此库不注册提供商，也不申请系统或任意网络权限。
//! 网络、账号、时钟、随机数、锁及跨调用状态全部经过 WASM SDK。
//! 原实现未接入图片和原生工具；现有 JWT 导入没有续期，不能凭迁移新增能力承诺。

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod account;
mod checkin;
mod checkin_state;
mod credentials;
mod errors;
mod login;
mod models;
mod network;
mod prompt;
mod rooms;
mod sign;
mod sse;
mod state;
mod stream;
mod usage;

use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

#[derive(Clone, Copy)]
pub struct Site {
    pub id: &'static str,
    pub label: &'static str,
    pub origin: &'static str,
    pub timezone: &'static str,
    pub fallback_sign_key: &'static str,
    pub order: u32,
}

pub const CLIENT_VERSION: &str = "1.17.16(10117016)";
pub(crate) const ROOM_TTL_MS: i64 = 30 * 60 * 1000;
pub(crate) const CATALOG_TTL_MS: i64 = 10 * 60 * 1000;

pub(crate) fn now() -> Result<i64> {
    let time = sdk::now();
    if time > 0 {
        Ok(time)
    } else {
        Err(Error::new("主程序未提供有效时间，未发送 Tabbit 请求"))
    }
}

fn metadata(site: Site) -> Value {
    json!({
        "id": site.id, "label": site.label, "order": site.order,
        "capabilities": {
            "chat": true, "stateful": true, "refresh": false, "model_refresh": true,
            "usage": true, "checkin": true, "claims": false, "web_login": false,
            "refresh_uses_account": true, "supports_default_model": false,
            "anonymous_default_session": false, "env_credentials": false,
            "system_proxy_when_unset": false, "sse_model_rewrite": false,
            "sse_strip_newline_chunks": false
        },
        "login_methods": [{ "kind": "token", "label": "网页 Cookie / JWT",
            "placeholder": format!("粘贴 {} 的 Cookie、Copy as cURL 或 JWT", site.origin),
            "extra_fields": [
                { "key": "cookie", "label": "网页 Cookie", "optional": true, "multiline": true },
                { "key": "unlockPro", "label": "解锁 Premium", "placeholder": "默认开启；填 0 关闭", "optional": true },
                { "key": "clientVersion", "label": "客户端版本", "placeholder": CLIENT_VERSION, "optional": true }
            ]
        }],
        "account_features": { "usage": true, "credit_detail": true, "credit_expiry": false,
            "usage_detail": false, "edition": false, "identifier": ["uid"],
            "expiry": ["expiresAt"], "long_lived": false },
        "checkin": { "description": "Tabbit 每日签到（以当前开放活动为准）；额度按百分比计，奖励以实际返回为准",
            "daily": true, "modes": ["checkin"] },
        "default_models": models::fallback(site)["models"]
    })
}

/// 国内短信在尚未有账号时复用同一指纹算法，不用伪造凭证读取账号。
pub fn prelogin_headers(site: Site) -> Result<Value> {
    network::prelogin_headers(site)
}

/// 只供受控登录流程把已取得的原生字段转换为相同身份/公开形态，不负责落库。
pub fn normalized_login(secret: Value) -> Result<Value> {
    account::normalized_login(secret)
}

pub fn handle(site: Site, method: &str, params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata(site)])),
        "ping" => Ok(json!(true)),
        "provider.info" => Ok(metadata(site)),
        "admin.action" if params["action"] == "auth.methods" => Ok(login::methods(
            site,
            params["payload"]["locale"].as_str().unwrap_or("zh-CN"),
        )),
        "admin.action" if params["action"] == "network.user-agent" => {
            Ok(json!({ "accountWide": true }))
        }
        "admin.action" if params["action"] == "quota.sources" => Ok(json!([])),
        "account.normalize" => account::normalize(site, &params),
        "account.view" => Ok(account::view(&params["secret"])),
        "account.identity" => Ok(json!(account::identity(&params["secret"]))),
        "token.ensure" => {
            let credentials = credentials::Credentials::from_secret(&sdk::account()?["secret"])?;
            credentials.ensure_valid()?;
            Ok(json!(credentials.token))
        }
        "token.expiring" => Ok(json!(false)),
        "token.refresh" => Err(Error::http(
            400,
            "Tabbit 会话不能自动续期，请重新登录对应地区并导入 Cookie",
        )
        .with_code("refresh_not_supported")),
        "models.list" => models::list(site, &params),
        "models.refresh" => models::refresh(site, &params),
        "usage.query" => {
            let _lock = AccountLock::acquire()?;
            usage::query(&mut network::Client::for_account(site)?)
        }
        "checkin.claim" => {
            let _lock = AccountLock::acquire()?;
            checkin::claim(&mut network::Client::for_account(site)?)
        }
        "conversation" => {
            rooms::forward(site, &params)?;
            Ok(Value::Null)
        }
        "classify" => Ok(errors::classify(
            params["status"].as_u64().unwrap_or(502) as u16,
            &params["body"],
        )),
        "is_error_response" => Ok(json!(
            !matches!(params["status"].as_u64(), Some(200..=299))
                || errors::business_failure(&params["body"])
        )),
        "error.policy" => Ok(json!({ "stop": errors::stopped(&params["body"]) })),
        "retry.decision" => Ok(if errors::stopped(&params["body"]) {
            json!({ "action": "stop", "reason": "Tabbit 本次任务或原账号状态未确认，不自动重发" })
        } else {
            json!({ "action": "default" })
        }),
        "retry_advice" => Ok(Value::Null),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "reasoning_patch" => {
            Ok(json!({ "action": "skip", "reason": "Tabbit 网页沿用所选模型的思考行为" }))
        }
        "outbound_reasoning" => Ok(Value::Null),
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false })),
        _ => Err(Error::unsupported()),
    }
}
