use md5::{Digest, Md5};
use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

// 只在手工导入或旧记录未携带浏览器标识时兜底；网页登录优先保留窗口的实际值。
const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";

pub(super) fn user_agent(payload: &Value) -> Result<&str, String> {
    match payload.get("userAgent") {
        None | Some(Value::Null) => Ok(DEFAULT_USER_AGENT),
        Some(Value::String(value)) if value.is_empty() => Ok(DEFAULT_USER_AGENT),
        Some(Value::String(value)) if value.len() <= 2048 && value.bytes().all(|byte| (32..=126).contains(&byte)) => Ok(value),
        _ => Err(super::bad_input("DeepSeek 网页浏览器标识无效，请重新登录")),
    }
}

fn has_user(response: &Value) -> bool {
    if response["code"].as_i64() != Some(0)
        || response.pointer("/data/biz_code").is_some_and(|code| code.as_i64() != Some(0)) {
        return false;
    }
    let Some(data) = response.get("data").filter(|data| data.is_object()) else { return false };
    let payload = data.get("biz_data").unwrap_or(data);
    let user = payload.get("user").unwrap_or(payload);
    if !user.is_object() { return false; }
    let id = user.get("id").is_some_and(|id| {
        id.as_str().is_some_and(|id| !id.trim().is_empty()) || id.is_i64() || id.is_u64()
    });
    id || ["email", "mobile_number", "mobile", "phone", "username", "nickname", "name"].into_iter()
        .any(|key| user[key].as_str().is_some_and(|value| !value.trim().is_empty()))
}

pub(super) async fn normalize(ctx: &PluginCtx, input: proto::NormalizeInput) -> Result<proto::NormalizedAccount, String> {
    let payload = match input.source {
        proto::NormalizeSource::Manual { payload } if payload.get("methodIndex").is_some() => {
            if payload["methodIndex"].as_u64() != Some(0) { return Err("DeepSeek 网页登录方式索引无效".into()); }
            let fields = payload["inputs"].as_object().ok_or("DeepSeek 网页登录 inputs 必须是字符串字段对象")?;
            if fields.iter().any(|(key, value)| !matches!(key.as_str(), "accessToken" | "cookie") || !value.is_string()) {
                return Err("DeepSeek 网页登录 inputs 包含未申报的字段或非字符串值".into());
            }
            Value::Object(fields.clone())
        },
        proto::NormalizeSource::Manual { payload } => payload,
        proto::NormalizeSource::FileText { content, .. } => serde_json::from_str(&content).unwrap_or_else(|_| json!({ "token": content })),
        _ => return Err("DeepSeek 网页请粘贴本人登录令牌与 Cookie".into()),
    };
    let token = ["accessToken", "token", "apiKey"].into_iter().find_map(|key| payload[key].as_str())
        .unwrap_or("").trim().trim_start_matches("Bearer ").trim();
    let cookie = payload["cookie"].as_str().unwrap_or("").trim();
    if token.is_empty() || token.len() > 16384 || token.starts_with("sk-") || matches!(token, "null" | "undefined")
        || cookie.len() > 32768 || token.contains(['\r', '\n']) || cookie.contains(['\r', '\n']) {
        return Err("请填写有效的 DeepSeek 网页访问令牌，不是收费 API Key".into());
    }
    let user_agent = user_agent(&payload)?;
    let client = super::client(input.proxy.as_ref(), &ctx.env.proxy)?;
    let headers = super::headers(token, cookie, user_agent);
    // 与官方网页、dsh-deepseek-web-login 的 validateAuth 一样，仅查询当前用户。
    // 对话挑战可能另受人机验证限制，不能用它作为保存已登录账号的前提。
    let response = super::get(&client, "/api/v0/users/current", &headers, &[]).await?;
    if !has_user(&response) { return Err("DeepSeek 登录态校验失败：未返回有效用户信息".into()); }
    let tail: String = token.chars().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
    // 保留旧账号的凭证身份规则，本次登录修复不迁移账号主键。
    let uid = format!("{:x}", Md5::digest(token.as_bytes()));
    Ok(proto::NormalizedAccount {
        identity: format!("{}-{uid}", super::ID),
        secret: json!({ "accessToken": token, "cookie": cookie, "userAgent": user_agent }),
        public: json!({ "name": input.name.unwrap_or_else(|| format!("DeepSeek {tail}")), "tokenTail": tail, "uid": uid, "credentialIdentity": true }),
        desktop: false,
    })
}
