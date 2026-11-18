use crate::{credentials as cred, endpoints, errors, network};
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

pub fn identity(secret: &Value) -> String {
    // 原生访客身份明确为空：不能把轮换的 uid 或 token 变成新的稳定主键。
    if secret["isGuest"].as_bool().unwrap_or(false) {
        return String::new();
    }
    let uid = cred::field(secret, &["uid", "userId", "user_id"]);
    if uid.is_empty() {
        String::new()
    } else {
        format!("uid-{uid}")
    }
}

pub fn view(secret: &Value) -> Value {
    let token = cred::access(secret);
    let guest = cred::is_guest(secret);
    let uid = cred::uid(secret);
    let explicit = errors::text(secret, &["name", "nickname"]);
    let name = if !explicit.is_empty() {
        explicit
    } else if guest {
        "智谱清言访客".into()
    } else if !uid.is_empty() {
        format!("智谱清言账号 {uid}")
    } else {
        "智谱清言账号".into()
    };
    let tail: String = if token.chars().count() <= 8 {
        String::new()
    } else {
        token
            .chars()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    };
    let mut value = json!({ "name": name, "uid": uid, "tokenTail": tail, "hasSession": !token.is_empty(),
        "isGuest": guest, "canRefresh": guest || !cred::refresh(secret).is_empty() });
    if let Some(expiry) = cred::expires(&token) {
        value["expiresAt"] = json!(expiry);
    }
    value
}

fn fields(params: &Value) -> Result<(Value, &'static str)> {
    let source = &params["source"];
    match source["kind"].as_str() {
        Some("desktop") => {
            // 桌面材料只来自原宿主私有 Context；请求不能自行补凭据、路径或访客选项。
            if params.as_object().is_none_or(|fields| fields.len() != 3)
                || params["provider"].as_str() != Some("chatglm")
                || params["account"].as_str() != Some("")
                || source.as_object().is_none_or(|fields| fields.len() != 1)
            {
                return Err(Error::http(400, "智谱本机导入只接受本次专用确认入口，不接受自由输入材料")
                    .with_code("chatglm_desktop_input_invalid"));
            }
            // 只消费一次，失败不重试、不调用 native.call、不转成手动导入或访客。
            let value = sdk::host("login.desktop.credentials.take", json!({}))?;
            let fields = value.as_object().filter(|fields| !fields.is_empty() && fields.len() <= 2)
                .ok_or_else(|| Error::http(502, "主程序未返回有效的智谱本次桌面凭据材料")
                    .with_code("chatglm_desktop_material_invalid"))?;
            for (key, value) in fields {
                let token = value.as_str().ok_or_else(|| Error::http(502, "智谱桌面凭据材料字段格式无效")
                    .with_code("chatglm_desktop_material_invalid"))?;
                if !matches!(key.as_str(), "accessToken" | "refreshToken")
                    || token.is_empty() || token.trim().is_empty()
                    || token.len() > 32 * 1024 || token.chars().any(char::is_control)
                {
                    return Err(Error::http(502, "智谱桌面凭据材料含未约定字段、空值或超限内容")
                        .with_code("chatglm_desktop_material_invalid"));
                }
            }
            if value.to_string().len() > 128 * 1024 {
                return Err(Error::http(502, "智谱桌面凭据材料超过大小限制")
                    .with_code("chatglm_desktop_material_invalid"));
            }
            Ok((value, "desktop"))
        }
        Some("file_text") => Ok((
            cred::read_text(source["content"].as_str().unwrap_or(""))?,
            "manual",
        )),
        Some("manual") => {
            let payload = &source["payload"];
            if let Some(index) = payload.get("methodIndex") {
                let index = index
                    .as_u64()
                    .filter(|index| *index <= 1)
                    .ok_or_else(|| Error::http(400, "智谱登录方式无效"))?;
                let input = payload["inputs"]
                    .as_object()
                    .ok_or_else(|| Error::http(400, "智谱登录字段必须为对象"))?;
                let allowed: &[&str] = if index == 1 {
                    &["deviceId", "name"]
                } else {
                    &[
                        "credential",
                        "accessToken",
                        "refreshToken",
                        "deviceId",
                        "name",
                    ]
                };
                if input
                    .iter()
                    .any(|(key, value)| !allowed.contains(&key.as_str()) || !value.is_string())
                {
                    return Err(Error::http(400, "智谱登录包含未申报字段"));
                }
                let mut value = Value::Object(input.clone());
                if index == 1 {
                    value["guest"] = json!(true);
                }
                Ok((value, if index == 1 { "guest" } else { "manual" }))
            } else if payload.is_object() {
                Ok((payload.clone(), "manual"))
            } else if let Some(text) = payload.as_str() {
                Ok((cred::read_text(text)?, "manual"))
            } else {
                Err(Error::http(400, "智谱凭证输入无效"))
            }
        }
        _ => Err(Error::http(
            400,
            "智谱请手动导入令牌、Cookie 或凭证 JSON，或明确选择添加访客；本机登录态须使用 Windows 桌面专用确认入口",
        )),
    }
}

pub fn normalize(params: &Value) -> Result<Value> {
    let (mut input, mut source) = fields(params)?;
    let explicit_guest = source == "guest" || input["guest"].as_bool() == Some(true);
    if let Some(raw) = input["credential"]
        .as_str()
        .filter(|value| source != "desktop" && !value.trim().is_empty())
    {
        let parsed = cred::read_text(raw)?;
        for (key, value) in [
            ("accessToken", cred::access_only(&parsed)),
            ("refreshToken", cred::refresh(&parsed)),
            ("deviceId", cred::field(&parsed, &["deviceId", "device_id"])),
        ] {
            let present = match key {
                "accessToken" => cred::access_only(&input),
                "refreshToken" => cred::refresh(&input),
                _ => cred::field(&input, &["deviceId", "device_id"]),
            };
            if present.is_empty() && !value.is_empty() {
                input[key] = json!(value);
            }
        }
    }
    let raw = cred::access_only(&input);
    if source != "desktop"
        && (raw.contains("chatglm_token=")
            || raw.contains("chatglm_refresh_token=")
            || raw.trim_start().starts_with('{'))
    {
        let parsed = cred::read_text(&raw)?;
        input["accessToken"] = json!(cred::access_only(&parsed));
        if cred::refresh(&input).is_empty() {
            input["refreshToken"] = json!(cred::refresh(&parsed));
        }
    }
    let mut access = cred::access_only(&input);
    let mut refresh = cred::refresh(&input);
    if !cred::valid_token(&access) || !cred::valid_token(&refresh) {
        return Err(Error::http(400, "智谱令牌过大或格式无效"));
    }
    let requested_guest = explicit_guest && source != "desktop";
    if requested_guest && (!access.is_empty() || !refresh.is_empty()) {
        return Err(Error::http(400, "访客添加不接受已有登录令牌"));
    }
    let device = cred::device(&input)?;
    if requested_guest {
        let fresh = network::tokens(&device, None)?;
        access = cred::access(&fresh);
        refresh = cred::refresh(&fresh);
        source = "guest";
    } else if access.is_empty() && !refresh.is_empty() {
        let fresh = network::tokens(&device, Some(&refresh))?;
        access = cred::access(&fresh);
        refresh = cred::refresh(&fresh);
    } else if access.is_empty() {
        return Err(Error::http(400, "请提供智谱访问令牌，或明确选择添加访客"));
    }
    let user = match network::get(endpoints::USER, &access, &device) {
        Err(error) if error.status == Some(401) && !refresh.is_empty() => {
            let fresh = network::tokens(&device, Some(&refresh))?;
            access = cred::access(&fresh);
            refresh = cred::refresh(&fresh);
            network::get(endpoints::USER, &access, &device)?
        }
        result => result?,
    };
    let guest = user["is_guest"]
        .as_bool()
        .or_else(|| cred::jwt(&access).and_then(|value| value["is_guest"].as_bool()))
        .unwrap_or(requested_guest);
    if !user.is_object() {
        return Err(Error::http(502, "智谱没有返回有效用户信息，未保存账号"));
    }
    let uid = errors::identifier(&user, &["_id", "user_id", "id", "uid"]);
    if source == "desktop" && (guest || uid.is_empty()) {
        return Err(
            Error::http(502, "智谱未确认本次桌面凭据属于有效登录账号，未保存")
                .with_code("chatglm_desktop_identity_unconfirmed"),
        );
    }
    let nickname = errors::text(&user, &["nickname"]);
    let mut secret =
        json!({ "accessToken": access, "deviceId": device, "isGuest": guest, "source": source });
    if !refresh.is_empty() {
        secret["refreshToken"] = json!(refresh);
    }
    if !uid.is_empty() {
        secret["uid"] = json!(uid);
    }
    if !nickname.is_empty() {
        secret["nickname"] = json!(nickname);
    }
    let name = errors::text(params, &["name"]);
    let name = if name.is_empty() {
        errors::text(&input, &["name"])
    } else {
        name
    };
    if !name.is_empty() {
        secret["name"] = json!(name.chars().take(100).collect::<String>());
    }
    Ok(
        json!({ "identity": identity(&secret), "public": view(&secret), "secret": secret, "desktop": source == "desktop" }),
    )
}

pub struct Session {
    pub secret: Value,
    pub token: String,
    pub device: String,
    pub guest: bool,
    pub uid: String,
    pub account: String,
}

impl Session {
    fn from_secret(secret: Value, account: String) -> Result<Self> {
        let token = cred::access(&secret);
        if token.is_empty() || !cred::valid_token(&token) {
            return Err(Error::http(401, "智谱账号缺少有效令牌，请重新添加"));
        }
        let device = cred::device(&secret)?;
        Ok(Self {
            guest: cred::is_guest(&secret),
            uid: cred::uid(&secret),
            secret,
            token,
            device,
            account,
        })
    }
    pub fn check_current(&self) -> Result<()> {
        let record = sdk::account()?;
        if record["id"].as_str() != Some(self.account.as_str()) || record["secret"] != self.secret {
            return Err(Error::http(
                409,
                "智谱账号或凭据已在本次操作期间变化，未继续发送或交付旧结果",
            )
            .with_code("chatglm_account_conflict"));
        }
        Ok(())
    }
    pub fn get(&self, path: &str) -> Result<Value> {
        self.check_current()?;
        let result = network::get(path, &self.token, &self.device)?;
        self.check_current()?;
        Ok(result)
    }
    pub fn raw_post(&self, path: &str, body: &Value) -> Result<Value> {
        self.check_current()?;
        let result = network::json("POST", path, &self.token, &self.device, Some(body))?;
        self.check_current()?;
        Ok(result)
    }
    pub fn redact(&self, value: &str) -> String {
        errors::redact(value, &[&self.token, &cred::refresh(&self.secret)])
    }
    pub fn sanitize(&self, value: &Value) -> Value {
        errors::sanitize(value, &[&self.token, &cred::refresh(&self.secret)])
    }
}

/// 调用者已持有账号锁；补设备号必须先成功落盘，不能只在这次请求里随机生成。
pub fn load_locked() -> Result<Session> {
    let record = sdk::account()?;
    let mut secret = record["secret"].clone();
    let device = cred::device(&secret)?;
    if cred::field(&secret, &["deviceId", "device_id"]) != device {
        secret["deviceId"] = json!(device);
        update(&record["secret"], &secret)?;
    }
    let id = record["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::http(502, "主程序没有返回智谱账号标识"))?
        .to_string();
    Session::from_secret(secret, id)
}

fn update(before: &Value, after: &Value) -> Result<()> {
    let result = sdk::host(
        "account.update",
        json!({ "expect": before, "secret": after, "public": view(after) }),
    )?;
    if result["updated"] != true {
        return Err(Error::http(409, "智谱凭证已被另一轮操作更新，请重试")
            .with_code("chatglm_account_conflict"));
    }
    Ok(())
}

pub fn refresh_locked(session: &Session) -> Result<Session> {
    session.check_current()?;
    let refresh = cred::refresh(&session.secret);
    if !session.guest && refresh.is_empty() {
        return Err(Error::http(401, "智谱登录账号缺少刷新令牌，请重新登录"));
    }
    let fresh = network::tokens(
        &session.device,
        if session.guest { None } else { Some(&refresh) },
    )?;
    let mut next = session.secret.clone();
    next["accessToken"] = json!(cred::access(&fresh));
    if !cred::refresh(&fresh).is_empty() {
        next["refreshToken"] = json!(cred::refresh(&fresh));
    }
    let uid = errors::identifier(&fresh, &["uid", "user_id"]);
    let uid = if uid.is_empty() {
        cred::uid(&fresh)
    } else {
        uid
    };
    if !session.guest
        && (!uid.is_empty() && !session.uid.is_empty() && session.uid != uid
            || cred::jwt(&cred::access(&fresh)).is_some_and(|claims| claims["is_guest"] == true))
    {
        return Err(
            Error::http(409, "智谱续期结果的账号身份与原账号不一致，未覆盖凭据")
                .with_code("chatglm_identity_conflict"),
        );
    }
    if !uid.is_empty() {
        next["uid"] = json!(uid);
    }
    next["deviceId"] = json!(session.device);
    session.check_current()?;
    update(&session.secret, &next)?;
    Session::from_secret(next, session.account.clone())
}

pub fn ready_locked() -> Result<Session> {
    let session = load_locked()?;
    if cred::expiring(&session.secret) {
        refresh_locked(&session)
    } else {
        Ok(session)
    }
}

pub fn ensure() -> Result<String> {
    let _lock = AccountLock::acquire()?;
    Ok(ready_locked()?.token)
}

pub fn refresh() -> Result<String> {
    let before = sdk::account()?["secret"].clone();
    let _lock = AccountLock::acquire()?;
    let current = load_locked()?;
    if cred::access(&before) != current.token {
        return Ok(current.token);
    }
    Ok(refresh_locked(&current)?.token)
}
