//! Tabbit 浏览器（`web.tabbit.ai` / `web.tabbit.com`）网页提供商。
//!
//! ── 为什么归 Web 包 ─────────────────────────────────────────
//! Tabbit 的「客户端」是一个 Chromium 浏览器，登录态是网页 Cookie 里的 JWT；
//! 上游是网页对话产品，只收一段纯文本、没有原生工具调用。这与 Web 包里
//! 智谱清言 / DeepSeek 网页同一形态（智谱清言也有桌面客户端，仍归 Web）。
//!
//! ── 两个地区是两家 provider ─────────────────────────────────
//! 两站签名 key 不同、账号不通用、登录方式也不同（国内站有手机号登录，
//! 国际站只能导入已有 Cookie）。按仓库的「两套账号就拆家」规则：
//! `tabbit`（国内）/ `tabbit-intl`（国际）。
//!
//! ── Pro 标记 ────────────────────────────────────────────────
//! 上游的 `unique-uuid` 第 5 位是「是否默认浏览器」标记位，命中后解锁
//! Premium 模型（Claude / GPT / Gemini）。本适配把账号上的 `unlockPro`
//! （添加账号时默认开启）映射到这一位；关闭时如实填「非默认浏览器」。
//!
//! ── 没有续期接口 ────────────────────────────────────────────
//! JWT 7 天有效，上游没有 refresh 端点，`refresh` 能力位为 false。过期只能
//! 重新导出 Cookie；账号行会显示有效期，不假装能自动续期。
//!
//! ── 工具调用 ────────────────────────────────────────────────
//! 上游没有原生 `tool_calls`，本适配明确拒绝 `tools`（不写提示词模拟），
//! 与 Web 包的 DeepSeek 网页同一口径。模型目录的 `supportsToolCall` 恒 false。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 本 crate 编译期禁止 unwrap/expect/panic（见 `main.rs` 的 deny）。

pub mod account;
pub mod checkin;
pub mod client;
pub mod config;
pub mod login;
pub mod models;
pub mod prompt;
pub mod sign;
pub mod stream;
pub mod usage;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ConversationSink, PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::Value;

use crate::errors::GatewayError;

use self::config::Realm;

/// 一家 Tabbit 地区实例。
pub struct TabbitProvider {
    realm: Realm,
}

impl TabbitProvider {
    pub const fn new(realm: Realm) -> Self {
        Self { realm }
    }
}

fn control(error: GatewayError) -> String {
    error.into_control_message()
}

fn control_message(status: i32, message: impl Into<String>) -> String {
    GatewayError::with_status(status, message).into_control_message()
}

async fn prepare(
    ctx: &PluginCtx,
    req: proto::ConversationRequest,
    realm: Realm,
    sink: &mut ConversationSink<'_>,
) -> Result<(), String> {
    let record = ctx
        .store_get(realm.id(), &req.account)
        .await
        .map_err(|error| control_message(401, format!("读取 Tabbit 账号失败：{error}")))?;
    let creds = client::Credentials::from_secret(&record.secret).map_err(control)?;
    let prompt = prompt::build(&req.body).map_err(control)?;
    let http = client::client_for(req.proxy.as_ref(), &ctx.env.proxy)
        .map_err(|error| control_message(502, error))?;
    let requested = if req.wire_model.is_empty() {
        req.body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    } else {
        req.wire_model.clone()
    };
    let catalog = models::load(&record.id, &creds, realm, &http, false)
        .await
        .map_err(control)?;
    let upstream_model = models::select(&catalog, &requested).map_err(control)?;
    let display_model = if requested.trim().is_empty() {
        upstream_model.clone()
    } else {
        requested
    };

    // 房间被占用（409）时换一个新房间重试一次；别的错误直接上抛。
    for attempt in 0..2 {
        let session = client::session_id(&creds, realm, &http)
            .await
            .map_err(control)?;
        let join_body = client::build_join_payload(&format!("{}|{}", creds.uid, session));
        let response = match client::open_join(&creds, realm, &session, &join_body, &http).await {
            Ok(response) => response,
            Err(error) if error.status_code == 409 && attempt == 0 => {
                client::drop_session(&creds, realm);
                continue;
            }
            Err(error) => return Err(control(error)),
        };
        let run_body = client::build_run_payload(&prompt, &upstream_model);
        match client::post_run(&creds, realm, &session, &run_body, &http).await {
            Ok(run_id) => {
                return stream::forward(response, run_id, display_model, sink).await;
            }
            Err(error) if error.status_code == 409 && attempt == 0 => {
                drop(response);
                client::drop_session(&creds, realm);
                continue;
            }
            Err(error) => {
                drop(response);
                return Err(control(error));
            }
        }
    }
    Err(control_message(
        502,
        "Tabbit 会话反复冲突，请稍后重试",
    ))
}

impl Provider for TabbitProvider {
    fn meta(&self) -> ProviderMeta {
        let cookie_placeholder = match self.realm {
            Realm::Cn => {
                "粘贴 web.tabbit.com 的 Cookie（token=…; user_id=…），也可直接粘 DevTools 的 Copy as cURL"
            }
            Realm::Intl => {
                "粘贴 web.tabbit.ai 的 Cookie（token=…; user_id=…），也可直接粘 DevTools 的 Copy as cURL"
            }
        };
        let mut login_methods = vec![proto::LoginMethod::Token {
                label: "网页 Cookie / JWT".into(),
                placeholder: cookie_placeholder.into(),
                extra_fields: vec![
                    proto::LoginField {
                        key: "cookie".into(),
                        label: "网页 Cookie".into(),
                        placeholder: "token=…; user_id=…（或 Copy as cURL 整段）".into(),
                        optional: false,
                        multiline: true,
                    },
                    proto::LoginField {
                        key: "token".into(),
                        label: "登录 JWT".into(),
                        placeholder: "只拿到 JWT 时填这里；Cookie 里有 token 可留空".into(),
                        optional: true,
                        multiline: false,
                    },
                    proto::LoginField {
                        key: "unlockPro".into(),
                        label: "解锁 Premium".into(),
                        placeholder: "默认开启；填 0 关闭（发送「已设为默认浏览器」标记）".into(),
                        optional: true,
                        multiline: false,
                    },
                    proto::LoginField {
                        key: "clientVersion".into(),
                        label: "客户端版本".into(),
                        placeholder: "留空用 1.17.16(10117016)".into(),
                        optional: true,
                        multiline: false,
                    },
                ],
        }];
        if self.realm == Realm::Cn {
            login_methods.push(proto::LoginMethod::Sms {
                hint: Some(
                    "用绑定的手机号登录：点「获取验证码」后填入即可。这是国内站官方唯一的登录方式。"
                        .into(),
                ),
            });
        }
        ProviderMeta::new(self.realm.id(), self.realm.label())
            .with_capabilities(proto::Capabilities {
                chat: true,
                // 建会话 → join 订阅 → runs 投递 → 翻译事件，属于多步会话协议。
                stateful: true,
                // 上游没有 refresh 端点：JWT 7 天，过期只能重新导出 Cookie。
                refresh: false,
                model_refresh: true,
                usage: true,
                checkin: true,
                claims: false,
                // 没有可复刻的网页授权回调；添加账号走粘贴 Cookie / cURL。
                web_login: false,
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                refresh_uses_account: true,
                supports_default_model: false,
                env_credentials: false,
            })
            .with_login(login_methods)
            .with_models(models::fallback_for(self.realm))
            .with_account_features(proto::AccountFeatures {
                usage: true,
                credit_detail: true,
                // 额度是百分比制：只有「剩 X%」与重置时刻，没有积分包。
                credit_expiry: false,
                usage_detail: false,
                edition: false,
                identifier: vec!["uid".to_string()],
                expiry: vec!["expiresAt".to_string()],
                // JWT 固定 7 天且不能续期，不能报「长期有效」。
                long_lived: false,
                ..Default::default()
            })
            .with_checkin(proto::CheckinInfo {
                description: Some(
                    "Tabbit 每日签到（daily_sign_in）；额度按百分比计，签到可能附带额度重置券"
                        .to_string(),
                ),
                modes: vec![proto::CheckinMode::Checkin],
                ..Default::default()
            })
    }

    fn provider_id(&self) -> String {
        self.realm.id().to_string()
    }

    fn conversation<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ConversationRequest,
        sink: ConversationSink<'a>,
    ) -> SendFuture<'a, ()> {
        Box::pin(async move {
            let mut sink = sink;
            let account = req.account.clone();
            crate::user_agent::scope(ctx, self.realm.id(), &account,
                prepare(ctx, req, self.realm, &mut sink)).await
        })
    }

    fn models<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, proto::ModelList> {
        Box::pin(crate::user_agent::scope(ctx, self.realm.id(), account, async move {
            let record = ctx
                .store_get(self.realm.id(), account)
                .await
                .map_err(|error| format!("读取 Tabbit 账号失败：{error}"))?;
            let creds = client::Credentials::from_secret(&record.secret)
                .map_err(GatewayError::into_message)?;
            let http = client::record_client_for(ctx, &record)
                .map_err(GatewayError::into_message)?;
            match models::load(&record.id, &creds, self.realm, &http, false).await {
                Ok(catalog) => Ok(catalog.list),
                Err(_) => Ok(models::fallback_catalog(self.realm).list),
            }
        }))
    }

    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(crate::user_agent::scope(ctx, self.realm.id(), account, async move {
            let record = ctx
                .store_get(self.realm.id(), account)
                .await
                .map_err(|error| format!("读取 Tabbit 账号失败：{error}"))?;
            let creds = client::Credentials::from_secret(&record.secret)
                .map_err(GatewayError::into_message)?;
            let http = client::record_client_for(ctx, &record)
                .map_err(GatewayError::into_message)?;
            match models::load(&record.id, &creds, self.realm, &http, force).await {
                Ok(catalog) => Ok(proto::RefreshOutcome::refreshed(catalog.list.models.len())),
                Err(error) => Ok(proto::RefreshOutcome::failed(error.message)),
            }
        }))
    }

    fn usage<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, proto::UsageReport> {
        Box::pin(crate::user_agent::scope(ctx, self.realm.id(), account, async move {
            let record = ctx
                .store_get(self.realm.id(), account)
                .await
                .map_err(|error| format!("读取 Tabbit 账号失败：{error}"))?;
            let creds = client::Credentials::from_secret(&record.secret)
                .map_err(GatewayError::into_message)?;
            let http = client::record_client_for(ctx, &record)
                .map_err(GatewayError::into_message)?;
            let mut report = usage::query(&creds, self.realm, &http)
                .await
                .map_err(GatewayError::into_control_message)?;
            let sources = crate::quota::sources(self.realm.id(), &report);
            if let Some(object) = report.as_object_mut() {
                object.insert("quotaSources".to_string(), Value::Array(sources));
            }
            Ok(report)
        }))
    }

    fn checkin<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(crate::user_agent::scope(ctx, self.realm.id(), account, async move {
            let record = ctx
                .store_get(self.realm.id(), account)
                .await
                .map_err(|error| format!("读取 Tabbit 账号失败：{error}"))?;
            let creds = client::Credentials::from_secret(&record.secret)
                .map_err(GatewayError::into_message)?;
            let http = client::record_client_for(ctx, &record)
                .map_err(GatewayError::into_message)?;
            checkin::claim(&creds, self.realm, &http)
                .await
                .map_err(GatewayError::into_control_message)
        }))
    }

    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let record = ctx
                .store_get(self.realm.id(), account)
                .await
                .map_err(|error| format!("读取 Tabbit 账号失败：{error}"))?;
            let creds = client::Credentials::from_secret(&record.secret)
                .map_err(GatewayError::into_message)?;
            Ok(creds.token)
        })
    }

    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        let message = {
            let text = client::error_text(body);
            if text.is_empty() {
                match status {
                    401 | 403 => "Tabbit 登录态已失效，请重新导出 Cookie".to_string(),
                    409 => "Tabbit 会话正在处理中，请稍后重试".to_string(),
                    429 => "Tabbit 请求过于频繁，请稍后重试".to_string(),
                    492 => {
                        "Tabbit 当前账号没有该模型的权限（需要 Pro 或今日额度已用完）".to_string()
                    }
                    _ => format!("Tabbit 上游返回 HTTP {status}"),
                }
            } else {
                text
            }
        };
        match status {
            401 | 403 => proto::ErrorClass::TokenExpired { message },
            429 | 492 => proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: Some(i64::from(status)),
                status,
            },
            // 房间占用：短冷却并换账号（不是永久失败，也不罚账号额度）。
            409 => proto::ErrorClass::QuotaLimited {
                reset_at: Some(crate::http::now_ms() + 30_000),
                message,
                upstream_code: Some(409),
                status,
            },
            _ => proto::ErrorClass::Fatal {
                status,
                message,
                upstream_code: None,
            },
        }
    }

    fn normalize_account<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move {
            account::normalize(ctx, input, self.realm)
                .await
                .map_err(GatewayError::into_control_message)
        })
    }

    fn account_view(&self, secret: &Value) -> Result<proto::AccountView, String> {
        Ok(account::account_view(secret))
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(account::identity_of(secret))
    }

    /// 动态登录表单（手机端 / 新添加账号界面走这一条）。
    ///
    /// 只申报一个「粘贴网页凭证」的方法：Tabbit 没有可复刻的授权回调，
    /// 也不做短信登录，所以没有 browser / sms 方法。
    fn admin_action<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        _account: &'a str,
        action: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            match action {
                "network.user-agent" => Ok(serde_json::json!({ "accountWide": true })),
                "login.sms-send" => {
                    if self.realm != Realm::Cn {
                        return Err(control_message(
                            400,
                            "Tabbit 国际站暂不支持短信登录，请粘贴网页 Cookie",
                        ));
                    }
                    login::sms_send(ctx, payload)
                        .await
                        .map_err(GatewayError::into_control_message)
                }
                "login.sms-verify" => login::sms_verify(ctx, payload, self.realm)
                    .await
                    .map_err(GatewayError::into_control_message),
                "auth.methods" => {
                    let site = match self.realm {
                        Realm::Cn => "web.tabbit.com",
                        Realm::Intl => "web.tabbit.ai",
                    };
                    Ok(serde_json::json!({
                        "methods": [{
                            "methodIndex": 0,
                            "type": "api",
                            "label": "粘贴网页 Cookie / JWT",
                            "instructions": format!(
                                "在 {site} 登录本人账号后，从开发者工具 Application → Cookies 复制 token 与 user_id，或直接粘贴该站点请求的 Copy as cURL 整段。登录态约 7 天有效，上游没有续期接口；过期后重新导入。Cookie 与 JWT 至少填一项。"
                            ),
                            "prompts": [
                                { "key": "cookie", "type": "text", "label": "网页 Cookie", "placeholder": "token=…; user_id=…（或 Copy as cURL 整段）", "optional": true, "secret": true },
                                { "key": "token", "type": "text", "label": "登录 JWT", "placeholder": "只拿到 JWT 时填这里；Cookie 里有 token 可留空", "optional": true, "secret": true },
                                { "key": "unlockPro", "type": "text", "label": "解锁 Premium", "placeholder": "默认开启；填 0 关闭（发送「已设为默认浏览器」标记）", "optional": true },
                                { "key": "clientVersion", "type": "text", "label": "客户端版本", "placeholder": "留空使用 1.17.16(10117016)", "optional": true },
                                { "key": "name", "type": "text", "label": "备注名", "placeholder": "可选，留空自动生成", "optional": true }
                            ]
                        }]
                    }))
                }
                _ => Err(control_message(
                    501,
                    format!("Tabbit 没有实现管理动作「{action}」"),
                )),
            }
        })
    }
}
