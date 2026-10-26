//! 智谱清言（`chatglm.cn`）插件实现：**无状态转发 + 双凭证形态**。
//!
//! ── 上游长什么样（依据：客户端 2.0.7 的 `app.asar` + `agentmore` 线上包）──
//! 智谱清言桌面端是标准 Electron 套壳（网页是 `agentmore.chatglm.cn` /
//! `chatglm.cn`），**没有任何可外部调用的本地 HTTP API**；全部能力都在
//! `chatglm.cn` 的一组 REST 接口上。三件事决定了本目录的实现形态：
//!
//! ```text
//!   1. 每个请求都要**签名三件套 + 设备号**（见 sign.rs）。
//!      签名密钥是客户端内置的（随安装包分发，不是用户凭证）；
//!      时间戳是智谱自创的校验位算法（不是普通时间戳）。
//!      实测边界：错密钥 40014、过期时间戳 40013、缺设备号 40004、缺令牌 40003。
//!
//!   2. 对话响应的 SSE 帧**混着两种语义**：`part.status == "init"` 的帧是增量，
//!      带 finish 的帧给整段全文快照。因此网关按 `UpstreamKind::ChatglmSnapshot`
//!      选翻译层做差分 —— **插件只回答「这次响应说的是哪套协议」**，差分留在网关。
//!
//!   3. **上游只认最后一条 user 消息**：messages 数组里的历史与 system 全被丢弃
//!      （实测：原生数组问「我刚才说的数字」答「不知道」；拍平成单条后答对）。
//!      因此请求侧必须把多轮拍平（plan.rs）。
//! ```
//!
//! ── 两条凭证形态（本家最特别的地方）───────────────────────────
//! ```text
//!   访客（guest）  POST /chatglm/user-api/guest/access（空 body）恒可换发，
//!                  不需要登录 —— 本家「零配置就能用」的入口。
//!                  但访客**没有积分体系**：余额 / 签到 / 领取类接口一律
//!                  401（`user needs login`），因此那些能力对它如实跳过。
//!   登录账号       手机验证码 / 授权码 / 粘贴 access_token 得到，
//!                  有积分 → 余额查询、每日登录积分、活动领取全部可用。
//! ```
//!
//! ── 设备号是身份的一部分（不要当成可丢弃的装饰）──────────────────
//! 服务端把 `X-Device-Id` 写进签发的 JWT（`device_id` 声明），而访客身份
//! **按设备号分配**（实测两个 UUID 各自换到不同 `user_id`）。因此设备号落进
//! 账号记录持久化：每次请求现随机 = 每次换身份；换设备号 = 旧身份与已累积的
//! 免费额度作废。
//!
//! ── 从网关搬来时改了什么（逐条对照，便于 parity 核对）────────────
//! ```text
//!   不变：签名三件套与全部功能性头、多轮拍平、错误分类的判定顺序、
//!         访客换发 / 续期的恢复路径、积分读数的归一形状、
//!         每日登录积分与活动领取、任务中心的新手/每日任务切分
//!   改：  ① 凭证从 `store.chatglm_account_record()` 改成 `store::record()`
//!         ② 目录缓存不落盘（网关自己缓存插件返回的清单）
//!         ③ 新手任务的结算记忆改存 ledger（形状与语义不变）
//!         ④ 添加账号的「换证 / 校验」搬进 `normalize_account`
//! ```
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 本 crate 编译期禁止 unwrap/expect/panic（见 `main.rs` 的 deny）。

mod account;

pub mod balance;
pub mod checkin;
pub mod credentials;
pub mod endpoints;
pub mod http;
pub mod models;
pub mod plan;
pub mod sign;
pub mod tasks;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ChatPlan, PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::Value;

use crate::errors::GatewayError;
use crate::logging;
use self::account::{account_view, identity_of, normalize, save_tokens};

/// provider id（落盘契约，不能改）。
pub const ID: &str = "chatglm";

/// 智谱清言适配器（无状态）。
pub struct ChatglmProvider;

impl Provider for ChatglmProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(ID, "智谱清言")
            .with_capabilities(proto::Capabilities {
                chat: true,
                stateful: false,
                // POST /chatglm/user-api/user/refresh
                refresh: true,
                // GET /chatglm/user-api/operation/detail?tag=available_models
                model_refresh: true,
                usage: true,
                checkin: true,
                claims: true,
                // 手机验证码 / 授权码 / 粘贴令牌 / 访客，都走「填表」而不是网页登录
                web_login: false,
                // 本家的 SSE 帧语义特殊，由网关的翻译层处理，不需要改写 model
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                // 访客模式让本家「零配置可用」：一个账号都没有时也能换发访客凭证
                anonymous_default_session: true,
                system_proxy_when_unset: false,
                refresh_uses_account: true,
                // 本家是否认「默认模型」概念（协议新增位，见 `Capabilities`）
                supports_default_model: false,
                // 本家有没有环境变量 / 默认登录态旁路（协议新增位）
                env_credentials: false,
            })
            .with_login(vec![
                proto::LoginMethod::Token {
                    label: "访问令牌".into(),
                    placeholder: "粘贴 chatglm.cn 的 chatglm_token（或整段 Cookie）".into(),
                    extra_fields: vec![proto::LoginField {
                        key: "deviceId".into(),
                        label: "设备号".into(),
                        placeholder: "留空则自动生成（换设备号会换一个访客身份）".into(),
                        optional: true,
                        multiline: false,
                    }],
                },
                proto::LoginMethod::Guest,
                proto::LoginMethod::Desktop {
                    button: "导入本机登录态".into(),
                    hint: Some(
                        "读取智谱清言客户端 Cookies（仅 Windows 可解，需先打开客户端登录）"
                            .into(),
                    ),
                },
            ])
            .with_models(models::fallback_models())
            // 账号行：`usage` —— 余额是**会员积分**（`member-api` 的活动状态
            // `current_score`，外加 `payment-api` 的会员档位）。⚠️ **访客账号没有
            // 积分体系**：那些接口对访客一律 401，后端回 400
            // `usage_not_configured`（文案说明「访客本来就没有积分」），界面按
            // 「未配置查询」如实显示而不是报错。
            //   `credit_detail` —— 逐份读数（会员积分 + 会员档位两行），主读数
            //     带活动进度。
            //   `edition: false` —— 单一站点（国际站是另一个产品 Z.AI/ZCode，
            //     已按独立 provider 建模）。
            //   `identifier: uid` —— JWT 的 `uid` 声明；访客也有。
            //   `expiry` 空 + `long_lived: true` —— 登录账号的 refresh token 由
            //     插件自动续期（`user/refresh` 实测可用），访客则由插件用**同一个
            //     设备号**重新 `guest/access` 换发，所以「什么时候到期」对用户
            //     没有行动价值。⚠️ 访客令牌本身有 7 天量级的寿命，换发后身份是否
            //     延续由上游决定（设备号不变是插件能做的全部）；这里说的是
            //     「不需要用户操心有效期」，不是「令牌永不过期」。
            .with_account_features(proto::AccountFeatures {
                usage: true,
                credit_detail: true,
                identifier: vec!["uid".to_string()],
                long_lived: true,
                ..Default::default()
            })
        .with_checkin(proto::CheckinInfo {
            description: Some(
                "每日登录积分 · 顺带领取会员活动 · 访客账号无积分体系（跳过）".to_string(),
            ),
            modes: vec![proto::CheckinMode::Checkin],
            // 访客没有积分体系（相关接口一律 401）：列进签到范围只会永远显示
            // 「待签」并把进度分母撑大，所以按账号形态排除。
            exclude_guest: true,
            onboarding: true,
            ..Default::default()
        })
    }

    fn provider_id(&self) -> String {
        ID.to_string()
    }

    // ── 数据面 ────────────────────────────────────────────────

    /// 构造 `POST /chatglm/mainchat-api/engine/stream`。
    ///
    /// 请求侧只多一件事：**把多轮 messages 拍平为单条文本**（`plan.rs` 有实测
    /// 论证：上游只认最后一条 user 消息）。
    fn build_chat<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ChatRequest,
    ) -> SendFuture<'a, ChatPlan> {
        Box::pin(async move {
            let record = crate::store::record(ctx, ID, &req.account).await?;
            let credentials = credentials::from_record(Some(&record))?;
            let token = credentials.bearer();
            if token.trim().is_empty() {
                return Err(GatewayError::with_status(
                    401,
                    "智谱清言账号缺少令牌，无法转发（请重新添加该账号）",
                )
                .into_message());
            }
            let model = req
                .body
                .get("model")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .unwrap_or_else(models::default_model);
            let built = plan::build_chat_plan(&req.body, &model);
            let headers = http::request_headers(
                &token,
                &credentials.device_id,
                &[("Accept", "text/event-stream")],
            );
            Ok(ChatPlan {
                url: built.url,
                headers,
                body: built.body,
                upstream: proto::UpstreamKind::ChatglmSnapshot,
            })
        })
    }

    /// 响应头是否表明上游错误。
    ///
    /// ── 本家为什么要覆写 ────────────────────────────────────────
    /// 正常对话返回 `text/event-stream`；但**额度不足 / 未登录等业务失败返回
    /// `HTTP 200 + application/json`**（实测 `-1001 剩余积分不足`、
    /// `40103 user needs login`）。不覆写的话这些响应会被当成成功流交给客户端，
    /// 结果是一条「200 但内容全无」的空回答 —— 既看不到真实原因，也不会触发
    /// 账号轮换。
    ///
    /// 判定口径与 zcode 逐字相同（MIME 参数剥离 + 大小写不敏感），两处刻意
    /// 保持一致：同一类判据在两家各写一套，迟早分叉。
    fn is_error_response(
        &self,
        status: u16,
        headers: &std::collections::BTreeMap<String, String>,
    ) -> bool {
        if !(200..300).contains(&status) {
            return true;
        }
        crate::headers::Headers::from_map(headers)
            .get("content-type")
            .and_then(|value| value.split(';').next())
            .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
    }

    /// 上游错误分类。
    ///
    /// 取值链：先取上游文案（`message` / `msg` / `error.message`），再按状态码
    /// 或**业务码**归四档：
    ///   - `401` / `403`、业务码 `40003`（缺 Authorization）/ `40103`
    ///     （`user needs login`）→ `TokenExpired`；
    ///   - `429`、业务码 `-1001`（**剩余积分不足**）→ `QuotaLimited`；
    ///   - 业务码 `40014`/`40013`（签名/时间戳异常，通常是本机时钟偏了）→ `Fatal`；
    ///   - `400` + 审核文案 → 共用内容拦截判定（不罚账号）；
    ///   - 其余 → `Fatal` 原样透传。
    fn classify(&self, status: u16, error_body: &Value) -> proto::ErrorClass {
        let raw = error_body
            .get("message")
            .or_else(|| error_body.get("msg"))
            .or_else(|| error_body.get("error"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误");
        // 业务码可能是顶层 `status`（本家的信封字段）或 `code`
        let code = error_body
            .get("status")
            .and_then(Value::as_i64)
            .or_else(|| error_body.get("code").and_then(Value::as_i64))
            .or_else(|| error_body.get("error_code").and_then(Value::as_i64));
        let message = format!("上游返回 {status}: {raw}");

        match code {
            Some(40003) | Some(40103) => return proto::ErrorClass::TokenExpired { message },
            Some(-1001) => {
                return proto::ErrorClass::QuotaLimited {
                    reset_at: None,
                    message: format!("{message}（该账号积分不足，请换账号或稍后再试）"),
                    upstream_code: Some(-1001),
                    status: 429,
                }
            }
            Some(40013) | Some(40014) | Some(40001) | Some(40004) => {
                return proto::ErrorClass::Fatal {
                    status: 400,
                    message: format!(
                        "{message}（请求签名/设备头未通过上游校验；\
                         最常见原因是本机系统时间不准，校准后重试）"
                    ),
                    upstream_code: code,
                }
            }
            _ => {}
        }

        match status {
            401 | 403 => proto::ErrorClass::TokenExpired { message },
            429 => proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: code,
                status: 429,
            },
            400 | 422 => crate::content_block::classify_or_fatal(status, error_body, message, code),
            _ => proto::ErrorClass::Fatal {
                status: if status >= 500 { 502 } else { status },
                message,
                upstream_code: code,
            },
        }
    }

    // ── 凭证 ──────────────────────────────────────────────────

    /// 取可用令牌（**只读记录，不发网络请求**）。
    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let record = crate::store::record(ctx, ID, account).await?;
            let credentials = credentials::from_record(Some(&record))?;
            Ok(credentials.bearer())
        })
    }

    /// 401 后的**强制**刷新。
    ///
    /// ── 访客与登录账号走不同的恢复路径（本家特有）──────────────
    ///   - 登录账号：`user/refresh`（refresh_token 换新令牌，上游会轮换它，
    ///     因此必须回写账号）；
    ///   - 访客：同一个设备号重新 `guest/access` 换发一对新令牌 ——
    ///     访客令牌有 7 天有效期，过期后 `user/refresh` 未必救得回来，
    ///     而重新换发**恒可用**（换个新访客身份，免费额度照旧）。
    fn refresh_token<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, String> {
        Box::pin(async move {
            let record = crate::store::record(ctx, ID, account).await?;
            let credentials = credentials::from_record(Some(&record))?;
            if !credentials.is_guest && credentials.refresh_token.trim().is_empty() {
                return Err(GatewayError::with_status(401, "智谱清言登录账号缺少刷新令牌，请重新登录")
                    .into_control_message());
            }
            if credentials.is_guest {
                let (access, refresh, uid) = http::guest_access(&credentials.device_id, None)
                    .await
                    .map_err(|error| {
                        GatewayError::with_status(
                            error.status_code,
                            format!("智谱清言访客凭证重新换发失败：{}", error.message),
                        )
                    })?;
                // 写回账号（设备号保持不变 —— 它是身份的一部分）
                return save_tokens(ctx, account, &record, &access, &refresh, None, Some(&uid)).await
                    .map_err(GatewayError::into_control_message);
            }
            // 登录账号：refresh_token 续期（上游可能轮换 refresh_token，必须回写）
            let (access, rotated, uid) =
                http::refresh_token(&credentials.refresh_token, &credentials.device_id, None)
                    .await
                    .map_err(|error| {
                        GatewayError::with_status(
                            error.status_code,
                            format!(
                                "智谱清言凭证续期失败：{}（请在账号页重新添加该账号）",
                                error.message
                            ),
                        )
                    })?;
            save_tokens(ctx, account, &record, &access, &rotated, None, Some(&uid)).await
                .map_err(GatewayError::into_control_message)
        })
    }

    /// 凭证是否临期：令牌有效期写在 JWT 的 `exp` 声明里 —— 直接读它，不靠猜。
    /// 判不出来（记录缺失 / 令牌形态异常）一律 false：宁可漏刷也不要凭猜测打上游。
    fn token_expiring<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, bool> {
        Box::pin(async move {
            if account.is_empty() {
                return Ok(false);
            }
            let Ok(record) = crate::store::record(ctx, ID, account).await else {
                return Ok(false);
            };
            let Ok(credentials) = credentials::from_record(Some(&record)) else {
                return Ok(false);
            };
            Ok((credentials.is_guest || !credentials.refresh_token.trim().is_empty())
                && credentials::token_expiring_soon(&credentials.bearer()))
        })
    }

    // ── 目录 / 余额 / 签到 / 领取 ──────────────────────────────

    fn models<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async { Ok(models::list_report()) })
    }

    /// 刷模型目录：用指定账号（空串 = 库里第一个可用账号）的令牌。
    ///
    /// 「一个账号都没有」不是失败（那是「用户还没添加账号」的正常状态），
    /// 返回 `unchanged()` —— 与 lobsterai / atomcode 同一处置。
    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            let record = match crate::store::record(ctx, ID, account).await {
                Ok(record) => record,
                Err(_) => {
                    if account.is_empty() {
                        logging::verbose("[Models]", "智谱清言模型目录刷新跳过：尚未添加账号");
                        return Ok(proto::RefreshOutcome::unchanged());
                    }
                    return Ok(proto::RefreshOutcome::failed(
                        "指定的账号不存在或不可用，请重新选择",
                    ));
                }
            };
            let credentials = match credentials::from_record(Some(&record)) {
                Ok(credentials) => credentials,
                Err(error) => return Ok(proto::RefreshOutcome::failed(error.message)),
            };
            Ok(models::refresh(&credentials.bearer(), &credentials.device_id, force).await)
        })
    }

    /// 查询积分余额（`balance.rs`）。
    ///
    /// 访客账号这些接口一律 401 —— 如实报「访客账号没有积分体系」，
    /// 用中性标记（`usage_not_configured`）而不是红色失败。
    ///
    /// ── 为什么这里必须用 `into_control_message`（不能是 `into_message`）──
    /// 错误码要**过线**才能到前端：`usage_not_configured` 是「中性提示」与
    /// 「红色失败」之间唯一的判据，而 `into_message` 只留文案，码在这条边界上
    /// 就被丢了 —— 于是前端把访客的「本来就没有积分」渲染成一格红色
    /// 「查询失败」。网关侧 `plugin_host::errors::control_error` 负责把它还原。
    fn usage<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, Value> {
        Box::pin(async move {
            let mut report = balance::query(ctx, account)
                .await
                .map_err(GatewayError::into_control_message)?;
            let sources = crate::quota::sources(ID, &report);
            if let Some(object) = report.as_object_mut() {
                object.insert("quotaSources".to_string(), Value::Array(sources));
            }
            Ok(report)
        })
    }

    /// 每日登录积分（本家的「签到」）。
    ///
    /// 只对登录账号有效（访客实测 401）—— `checkin.rs` 判形态后给中性结果。
    ///
    /// ── 为什么签到里还顺带领两笔（与网关内置版同一口径）──────────
    /// 网关的 `billing::checkin` 在 chatglm 分支里依次跑了三件事：
    /// 每日登录积分（签到本身）、活动领取（`claim_bonus`）、每日任务
    /// （`claim_daily_tasks`），然后把后两者的结果塞进同一个结果行的
    /// `bonus` / `dailyTasks` 格。插件契约里的 `CheckinReport` 只有
    /// `outcome / credit / streak / message` 四格，装不下那两个子对象 ——
    /// 但**副作用必须保留**（那两笔是真会到账的积分），所以它们照样执行，
    /// 结论折进 `credit` 与 `message`。
    ///
    /// 顺序与内置版一致：登录积分 → 活动 → 每日任务。后两步都是 best-effort：
    /// 失败只记进文案，不改签到本身的成败（活动非活动期本来就会给中性拒绝）。
    fn checkin<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(async move {
            if self.token_expiring(ctx, account).await? {
                self.refresh_token(ctx, account).await?;
            } else {
                self.ensure_token(ctx, account).await?;
            }
            let claim = match checkin::claim_daily_login_score(ctx, account).await {
                Err(error) if error.status_code == 401 => {
                    self.refresh_token(ctx, account).await?;
                    checkin::claim_daily_login_score(ctx, account).await.map_err(|error| {
                        error.with_code("checkin_auth_retry_exhausted").into_control_message()
                    })?
                }
                result => result.map_err(GatewayError::into_control_message)?,
            };
            let mut report = checkin_report_of(&claim);
            // 两笔顺带领取（访客会在实现里中性跳过）
            let bonus = checkin::claim_bonus(ctx, account).await;
            let daily = tasks::claim_daily_tasks(ctx, account).await;
            let mut notes: Vec<String> = Vec::new();
            let mut extra_credit = 0_i64;
            match &bonus {
                Ok(value) => {
                    if let Some(text) = value.get("msg").and_then(Value::as_str) {
                        if !text.trim().is_empty() {
                            notes.push(format!("活动领取：{text}"));
                        }
                    }
                }
                Err(error) => notes.push(format!("活动领取失败：{}", error.message)),
            }
            match &daily {
                Ok(value) => {
                    extra_credit = value
                        .get("gained")
                        .and_then(Value::as_i64)
                        .filter(|points| *points > 0)
                        .unwrap_or(0);
                    if let Some(text) = value.get("msg").and_then(Value::as_str) {
                        if !text.trim().is_empty() {
                            notes.push(format!("每日任务：{text}"));
                        }
                    }
                }
                Err(error) => notes.push(format!("每日任务失败：{}", error.message)),
            }
            if extra_credit > 0 {
                report.credit = Some(match report.credit.take() {
                    Some(existing) => format!("{existing}（每日任务另 +{extra_credit} 积分）"),
                    None => format!("每日任务 +{extra_credit} 积分"),
                });
            }
            if !notes.is_empty() {
                let joined = notes.join("；");
                report.message = Some(match report.message.take() {
                    Some(existing) if !existing.trim().is_empty() => {
                        format!("{existing}（{joined}）")
                    }
                    _ => joined,
                });
            }
            Ok(report)
        })
    }

    /// 新手任务状态（任务中心的 `newbie` 分类）。
    fn claim_status<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        refresh: bool,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            tasks::get_tasks(ctx, account, refresh)
                .await
                .map_err(GatewayError::into_message)
        })
    }

    /// 新手任务一键领取（逐条 click + complete）。
    fn claim<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        _claim: &'a str,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            tasks::claim_all(ctx, account)
                .await
                .map_err(GatewayError::into_message)
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    /// 添加账号：访客换发 / 粘贴令牌 / 导入本机登录态。
    ///
    /// ── 三条路径都要**出网校验** ────────────────────────────────
    /// 本家的令牌是不透明的（身份、访客标记都在 JWT 声明里，但不验签就不知道
    /// 上游认不认）。所以三条路都拿令牌问一次 `user/info`：它顺带读回 uid /
    /// 昵称 / 访客标记 —— 这三样决定账号 id、展示名与「有没有积分体系」。
    ///
    /// 校验失败一律 400（不落空记录）：添加时验证，要的是「现在真的能用」。
    fn normalize_account<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move { normalize(input).await.map_err(GatewayError::into_message) })
    }

    fn account_view(&self, secret: &Value) -> Result<Value, String> {
        Ok(account_view(secret))
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(identity_of(secret))
    }
}

/// 签到结果 → 协议化的 `CheckinReport`。
///
/// `checkin.rs` 给的是内置版的 `{success, msg}` 形态：
///   - `success: true` → `Claimed`；
///   - `alreadyCompleted: true`（业务码或活动开启时的零积分）→ `Done`；
///   - 访客 / 活动未开 → `Inactive`（中性，不是失败）；
///   - 其余 → `Failed`。
fn checkin_report_of(claim: &Value) -> proto::CheckinReport {
    let success = claim
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let message = claim
        .get("msg")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let credit = claim
        .get("score")
        .and_then(Value::as_f64)
        .filter(|value| *value > 0.0)
        .map(|value| format!("{value} 积分"));
    let text = message.clone().unwrap_or_default();
    let outcome = if success {
        proto::CheckinOutcome::Claimed
    } else if claim.get("alreadyCompleted").and_then(Value::as_bool) == Some(true) {
        proto::CheckinOutcome::Done
    } else if claim.get("outcome").and_then(Value::as_str) == Some("inactive")
        || text.contains("访客") || text.contains("没有积分") || text.contains("未开启") {
        proto::CheckinOutcome::Inactive
    } else {
        proto::CheckinOutcome::Failed
    };
    proto::CheckinReport {
        outcome,
        credit,
        streak: None,
        message,
    }
}
