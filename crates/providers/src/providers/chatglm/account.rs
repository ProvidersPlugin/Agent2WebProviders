use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde_json::Value;

use crate::errors::GatewayError;
use super::{credentials, http};

// ─── 凭证写回 ──────────────────────────────────────────────────

/// 把续期 / 换发拿到的新令牌写回账号记录（**比较再写**）。
///
/// ── 为什么只改那几个字段 ────────────────────────────────────
/// 设备号 `deviceId` **绝不能动**：它是身份的一部分（见模块头），换掉等于换一个
/// 访客身份、已累积的免费额度作废。所以走 `store::with_fields` 而不是整份替换。
///
/// 轮换后的令牌必须可靠落盘；并发写入冲突时回读当前令牌，不返回未保存的一份。
pub(super) async fn save_tokens(
    ctx: &PluginCtx,
    account: &str,
    record: &Value,
    access: &str,
    refresh: &str,
    is_guest: Option<bool>,
    uid: Option<&str>,
) -> Result<String, GatewayError> {
    let mut next = crate::store::with_fields(record, &[("accessToken", Value::String(access.to_string()))]);
    if !refresh.trim().is_empty() {
        next = crate::store::with_fields(&next, &[("refreshToken", Value::String(refresh.to_string()))]);
    }
    if let Some(is_guest) = is_guest {
        next = crate::store::with_fields(&next, &[("isGuest", Value::Bool(is_guest))]);
    }
    if let Some(uid) = uid.filter(|value| !value.trim().is_empty()) {
        next = crate::store::with_fields(&next, &[("uid", Value::String(uid.to_string()))]);
    }
    // 记录形态 → secret 形态（去掉 `store::record` 补上的键，否则比较永远不等）
    let expect = secret_of(record);
    if crate::store::save(ctx, account, &expect, &secret_of(&next)).await? {
        return Ok(access.to_string());
    }
    let latest = crate::store::record(ctx, super::ID, account).await?;
    let current = credentials::from_record(Some(&latest))?;
    if current.bearer() == credentials::from_record(Some(record))?.bearer() {
        return Err(GatewayError::with_status(409, "智谱清言续期结果未能落盘，请重新登录该账号"));
    }
    Ok(current.bearer())
}

/// 记录形态 → secret 形态（去掉 `store::record` 补上的三个键）。
fn secret_of(record: &Value) -> Value {
    // 去掉 `store::record_of` 补上的三个键（`id` / `provider` / `name`）——
    // 少去掉一个，"比较再写"就会永远不等（网关存的 secret 里没有它们）。
    crate::store::secret_view(record)
}

// ─── 账号视图 / 身份 ───────────────────────────────────────────

/// 账号的稳定身份：`uid`（JWT 的 `uid` 声明，或校验时读回的 `_id`）。
///
/// 访客没有稳定 uid（每次换发都是新身份），此时给空串让网关按凭证去重 ——
/// 那正是访客账号的语义（它们本来就不该被合并）。
pub(super) fn identity_of(secret: &Value) -> String {
    if secret
        .get("isGuest")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        // 访客：uid 会随换发变，不能拿它当去重键
        return String::new();
    }
    let uid = crate::store::pick(secret, &["uid", "userId", "user_id"]);
    if uid.is_empty() {
        return String::new();
    }
    format!("uid-{uid}")
}

/// 账号公开视图（界面账号页读的那几个键）。
pub(super) fn account_view(secret: &Value) -> Value {
    let access_token = crate::store::pick(secret, &["accessToken", "access_token", "token"]);
    let refresh_token = crate::store::pick(secret, &["refreshToken", "refresh_token"]);
    let uid = crate::store::pick(secret, &["uid", "userId", "user_id"]);
    let is_guest = secret
        .get("isGuest")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let nickname = crate::store::pick(secret, &["nickname"]);
    let name = {
        let explicit = crate::store::pick(secret, &["name"]);
        if !explicit.is_empty() {
            explicit
        } else if !nickname.is_empty() {
            nickname
        } else if is_guest {
            "智谱清言访客".to_string()
        } else if !uid.is_empty() {
            format!("智谱清言账号 {uid}")
        } else {
            "智谱清言账号".to_string()
        }
    };
    // 访客没有 refreshToken 也照样能「续期」（换发），所以判据是「访客或有令牌」。
    // 这一行必须在 `refresh_token` 被移进 `token` 之前算出来。
    let can_refresh = is_guest || !refresh_token.is_empty();
    let token = if !access_token.is_empty() {
        access_token
    } else {
        refresh_token
    };
    let tail = if token.len() <= 8 {
        String::new()
    } else {
        token.chars().skip(token.len() - 8).collect()
    };
    let mut view = serde_json::json!({
        "name": name,
        "uid": uid,
        "tokenTail": tail,
        "hasSession": true,
        "isGuest": is_guest,
        "canRefresh": can_refresh,
    });
    if let Some(expires_at) = credentials::token_expiry_ms(&token) {
        if let Some(object) = view.as_object_mut() {
            object.insert("expiresAt".to_string(), Value::from(expires_at));
        }
    }
    view
}

// ─── 账号规范化 ────────────────────────────────────────────────

/// 三条添加路径（与网关内置版 `api::accounts` 的 chatglm 分支逐条对齐）。
pub(super) async fn normalize(input: proto::NormalizeInput) -> Result<proto::NormalizedAccount, GatewayError> {
    // 设备号：用户给了就用（换设备号 = 换访客身份，是用户的选择），
    // 没给就现生成一个并随账号落盘
    let explicit_device = match &input.source {
        proto::NormalizeSource::Manual { payload } => {
            crate::store::pick(payload, &["deviceId", "device_id"])
        }
        _ => String::new(),
    };
    let device_id = credentials::resolve_device_id(&explicit_device);

    let (access, refresh, source_label) = match &input.source {
        proto::NormalizeSource::Desktop => match credentials::read_desktop_credentials() {
            Ok(Some((access, refresh))) => (
                access.trim().to_string(),
                refresh.trim().to_string(),
                "desktop",
            ),
            Ok(None) => {
                return Err(GatewayError::with_status(
                    400,
                    "没有在本机找到智谱清言客户端的登录态（请先打开客户端登录，\
                     或改用「访客模式」/「粘贴令牌」）",
                ))
            }
            Err(reason) => return Err(GatewayError::with_status(400, reason)),
        },
        proto::NormalizeSource::FileText { content, .. } => {
            // 文件导入：内容就是用户粘的那段（可能是一整份 JSON，也可能是一串令牌）。
            // 整段 Cookie 里的 `chatglm_refresh_token` 一并解析（缺它，access 过期
            // 后 `user/refresh` 无从续期，账号只能等死 —— 见 tokens_from_text）。
            let (token, refresh) = tokens_from_text(content)?;
            (token, refresh, "manual")
        }
        proto::NormalizeSource::FileBase64 { .. } => {
            return Err(GatewayError::with_status(
                400,
                "智谱清言不支持二进制凭证文件，请改用「导入本机登录态」或粘贴令牌",
            ))
        }
        proto::NormalizeSource::Manual { payload } => {
            let plain_token = crate::store::pick(
                payload,
                &["accessToken", "access_token", "token", "refreshToken"],
            );
            let want_guest = payload
                .get("guest")
                .and_then(Value::as_bool)
                .unwrap_or(plain_token.is_empty());
            if want_guest && plain_token.is_empty() {
                // 访客换证：不需要任何前置登录，只依赖设备号
                match http::guest_access(&device_id, None).await {
                    Ok((access, refresh, _uid)) => (access, refresh, "guest"),
                    Err(error) => {
                        return Err(GatewayError::with_status(error.status_code, error.message))
                    }
                }
            } else {
                if plain_token.is_empty() {
                    return Err(GatewayError::with_status(
                        400,
                        "请提供智谱清言的访问令牌（accessToken）",
                    ));
                }
                // 用户可能整段粘了 Cookie（`chatglm_token=…; chatglm_refresh_token=…;`），
                // 或 JSON / 裸令牌 —— 统一交给 tokens_from_text 解析。早先的实现
                // 只认 `chatglm_token=`、把 refresh_token 静默丢弃：账号能建、能转
                // 发，但 access 过期后无法续期（401「缺少刷新令牌」），只能重新添加。
                let (token, refresh) = tokens_from_text(&plain_token)?;
                (token, refresh, "manual")
            }
        }
    };

    if access.is_empty() {
        return Err(GatewayError::with_status(400, "没有取到智谱清言的访问令牌"));
    }

    // 真实校验：拿令牌问一次 user/info。顺带读回 uid / 昵称 / 访客标记
    let verified = http::user_info(&access, &device_id, None)
        .await
        .map_err(|error| {
            GatewayError::with_status(400, format!("智谱清言校验失败：{}", error.message))
        })?;
    let want_guest = source_label == "guest";
    let is_guest = verified
        .get("is_guest")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| credentials::token_is_guest(&access, want_guest));
    let uid = verified
        .get("_id")
        .or_else(|| verified.get("user_id"))
        .or_else(|| verified.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let nickname = verified
        .get("nickname")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    let mut secret = serde_json::json!({
        "accessToken": access,
        "deviceId": device_id,
        "isGuest": is_guest,
    });
    if !refresh.trim().is_empty() {
        secret["refreshToken"] = Value::String(refresh);
    }
    if !uid.is_empty() {
        secret["uid"] = Value::String(uid);
    }
    if !nickname.is_empty() {
        secret["nickname"] = Value::String(nickname);
    }
    secret["source"] = Value::String(source_label.to_string());
    if let Some(name) = input
        .name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        secret["name"] = Value::String(name.to_string());
    }
    Ok(proto::NormalizedAccount {
        public: account_view(&secret),
        identity: identity_of(&secret),
        secret,
        desktop: source_label == "desktop",
    })
}

/// 从一段文本里取出令牌（文件导入用）：先试 JSON，再当裸令牌。
/// 从用户粘贴的文本解析 `(access_token, refresh_token)`。
///
/// 接受三种形态（与桌面导入 `read_desktop_credentials` 的取值一致）：
///   1. JSON：顶层 `accessToken` / `access_token` / `token`，可选
///      `refreshToken` / `refresh_token`；
///   2. Cookie 串：`chatglm_token=…; chatglm_refresh_token=…;`（顺序无关，
///      其余 cookie 忽略；也接受换行分隔）；
///   3. 裸令牌：单个值按 access_token 处理（向后兼容）。
///
/// refresh 为空不报错：裸令牌本就没有它；但 access 取不到仍按原样报 400 ——
/// 仅 refresh 无法完成添加校验（`user/info` 要 access）。
fn tokens_from_text(content: &str) -> Result<(String, String), GatewayError> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err(GatewayError::with_status(400, "凭证文件是空的"));
    }
    if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
        if parsed.is_object() {
            let access = crate::store::pick(&parsed, &["accessToken", "access_token", "token"]);
            let refresh = crate::store::pick(&parsed, &["refreshToken", "refresh_token"]);
            if access.is_empty() {
                return Err(GatewayError::with_status(
                    400,
                    "凭证文件里没有 accessToken / token 字段",
                ));
            }
            return Ok((access, refresh));
        }
    }
    // Cookie 串：逐个 `k=v` 片段认两条已知 cookie 名
    let mut access = String::new();
    let mut refresh = String::new();
    for part in trimmed.split([';', '\n']) {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(credentials::COOKIE_ACCESS_TOKEN).map(str::trim) {
            if let Some(value) = value.strip_prefix('=') {
                access = value.trim().to_string();
            }
        } else if let Some(value) = part
            .strip_prefix(credentials::COOKIE_REFRESH_TOKEN)
            .map(str::trim)
        {
            if let Some(value) = value.strip_prefix('=') {
                refresh = value.trim().to_string();
            }
        }
    }
    if access.is_empty() && !refresh.is_empty() {
        // 只有 refresh 也能续期换出 access，但添加校验需要 access —— 明说，
        // 别让用户对着「校验失败」猜
        return Err(GatewayError::with_status(
            400,
            "粘贴内容里只有 chatglm_refresh_token、没有 chatglm_token：请一并粘贴两条 cookie",
        ));
    }
    if access.is_empty() {
        // 裸令牌（原行为）：整段按 access_token 处理
        access = trimmed
            .trim_start_matches("chatglm_token=")
            .trim_end_matches(';')
            .trim()
            .to_string();
    }
    Ok((access, refresh))
}

#[cfg(test)]
// 测试断言路径允许 unwrap：主程序 `#![deny(clippy::unwrap_used)]` 只为生产代码
// panic 安全，测试里 unwrap 失败本身就是测试失败，语义等价 expect。
#[allow(clippy::unwrap_used)]
mod tests {
    use super::tokens_from_text;

    #[test]
    fn whole_cookie_yields_both_tokens() {
        let text = "chatglm_token=abc123; chatglm_refresh_token=ref456; chatglm_site=xyz";
        let (access, refresh) = tokens_from_text(text).unwrap();
        assert_eq!(access, "abc123");
        assert_eq!(refresh, "ref456");
    }

    #[test]
    fn cookie_refresh_only_is_rejected_with_clear_message() {
        let text = "chatglm_refresh_token=ref456;";
        let error = tokens_from_text(text).unwrap_err();
        assert!(error.message.contains("chatglm_refresh_token"));
        assert!(error.message.contains("chatglm_token"));
    }

    #[test]
    fn cookie_order_is_irrelevant() {
        let text = "chatglm_refresh_token=ref456;\nchatglm_token=abc123";
        let (access, refresh) = tokens_from_text(text).unwrap();
        assert_eq!(access, "abc123");
        assert_eq!(refresh, "ref456");
    }

    #[test]
    fn json_payload_keeps_refresh_token() {
        let text = r#"{"accessToken":"abc123","refreshToken":"ref456"}"#;
        let (access, refresh) = tokens_from_text(text).unwrap();
        assert_eq!(access, "abc123");
        assert_eq!(refresh, "ref456");
    }

    #[test]
    fn bare_token_still_works() {
        let (access, refresh) = tokens_from_text("just-a-bare-token").unwrap();
        assert_eq!(access, "just-a-bare-token");
        assert_eq!(refresh, "");
    }

    #[test]
    fn legacy_token_prefix_without_refresh() {
        let (access, refresh) = tokens_from_text("chatglm_token=abc123;").unwrap();
        assert_eq!(access, "abc123");
        assert_eq!(refresh, "");
    }

    #[test]
    fn empty_input_is_rejected() {
        assert!(tokens_from_text("   ").is_err());
    }
}

