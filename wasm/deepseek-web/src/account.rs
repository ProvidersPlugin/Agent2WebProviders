use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use md5::{Digest, Md5};

use crate::network;

fn strip_bearer(mut value: &str) -> &str {
    value = value.trim();
    while value
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer "))
    {
        value = value[7..].trim();
    }
    value
}

fn token_field(value: &Value) -> String {
    let field = ["accessToken", "token", "apiKey", "userToken", "value"]
        .into_iter()
        .filter_map(|key| value.get(key))
        .find(|value| {
            !value.is_null() && value.as_str().is_none_or(|text| !text.trim().is_empty())
        });
    let Some(field) = field else {
        return String::new();
    };
    let text = field
        .as_str()
        .or_else(|| field["value"].as_str())
        .unwrap_or("")
        .trim();
    let decoded = if text.starts_with(['{', '"']) {
        serde_json::from_str::<Value>(text).ok()
    } else {
        None
    };
    let token = decoded
        .as_ref()
        .and_then(|value| value.as_str().or_else(|| value["value"].as_str()))
        .unwrap_or(text);
    strip_bearer(token).to_string()
}

// 仅作为手工导入/旧记录的原有兜底，桌面捕获始终保存真实窗口 UA。
const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";

fn field<'a>(value: &'a Value, keys: &[&str]) -> &'a str {
    keys.iter()
        .filter_map(|key| value[*key].as_str())
        .map(str::trim)
        .find(|text| !text.is_empty())
        .unwrap_or("")
}

fn read_text(input: &str) -> Result<Value> {
    if input.len() > 64 * 1024 {
        return Err(Error::http(400, "DeepSeek 凭证文本过大"));
    }
    let input = input.trim();
    if input.is_empty() {
        return Err(Error::http(400, "DeepSeek 凭证文本为空"));
    }
    match serde_json::from_str::<Value>(input) {
        Ok(value) if value.is_object() => Ok(value),
        Ok(Value::String(token)) => Ok(json!({ "accessToken": token })),
        Ok(_) => Err(Error::http(
            400,
            "DeepSeek 凭证 JSON 必须是对象或令牌字符串",
        )),
        Err(_) if input.starts_with('{') || input.starts_with('[') => {
            Err(Error::http(400, "DeepSeek 凭证 JSON 格式无效"))
        }
        Err(_) => Ok(json!({ "accessToken": input })),
    }
}

fn input(params: &Value) -> Result<Value> {
    let source = &params["source"];
    match source["kind"].as_str() {
        Some("file_text") => read_text(source["content"].as_str().unwrap_or("")),
        Some("manual") => {
            let payload = &source["payload"];
            if payload.get("methodIndex").is_some() {
                if payload["methodIndex"].as_u64() != Some(0) {
                    return Err(Error::http(400, "DeepSeek 网页登录方式索引无效"));
                }
                let fields = payload["inputs"]
                    .as_object()
                    .ok_or_else(|| Error::http(400, "DeepSeek 网页登录字段必须是对象"))?;
                if fields.iter().any(|(key, value)| {
                    !matches!(
                        key.as_str(),
                        "accessToken" | "credential" | "cookie" | "userAgent"
                    ) || !value.is_string()
                }) {
                    return Err(Error::http(400, "DeepSeek 网页登录包含未申报的字段"));
                }
                let mut supplied = fields
                    .get("credential")
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                    .map(read_text)
                    .transpose()?
                    .unwrap_or_else(|| json!({}));
                for key in ["accessToken", "cookie", "userAgent"] {
                    if let Some(value) = fields
                        .get(key)
                        .filter(|value| value.as_str().is_some_and(|text| !text.trim().is_empty()))
                    {
                        supplied[key] = value.clone();
                    }
                }
                Ok(supplied)
            } else if payload.is_object() {
                Ok(payload.clone())
            } else if let Some(text) = payload.as_str() {
                read_text(text)
            } else {
                Err(Error::http(400, "DeepSeek 网页凭证输入无效"))
            }
        }
        _ => Err(Error::http(
            400,
            "DeepSeek 网页请使用网页令牌导入或桌面独立登录窗口",
        )),
    }
}

pub struct Session {
    pub token: String,
    pub cookie: String,
    pub user_agent: String,
    record: Option<Value>,
}

impl Session {
    pub fn from_secret(value: &Value) -> Result<Self> {
        let token = token_field(value);
        let cookie = match value.get("cookie") {
            None | Some(Value::Null) => "",
            Some(Value::String(cookie)) => cookie.trim(),
            _ => return Err(Error::http(400, "DeepSeek 网页 Cookie 必须是文本")),
        };
        if token.is_empty()
            || token.len() > 16384
            || token.starts_with("sk-")
            || matches!(token.as_str(), "null" | "undefined")
            || !token.bytes().all(|byte| (33..=126).contains(&byte))
            || cookie.len() > 32768
            || !cookie.bytes().all(|byte| (32..=126).contains(&byte))
        {
            return Err(Error::http(
                400,
                "请填写有效的 DeepSeek 网页访问令牌与 Cookie，不是收费 API Key",
            ));
        }
        let user_agent = match value.get("userAgent") {
            None | Some(Value::Null) => DEFAULT_USER_AGENT,
            Some(Value::String(value)) if value.is_empty() => DEFAULT_USER_AGENT,
            Some(Value::String(value))
                if value.len() <= 2048 && value.bytes().all(|byte| (32..=126).contains(&byte)) =>
            {
                value
            }
            _ => return Err(Error::http(400, "DeepSeek 网页浏览器标识无效，请重新登录")),
        };
        Ok(Self {
            token,
            cookie: cookie.into(),
            user_agent: user_agent.into(),
            record: None,
        })
    }

    pub fn load() -> Result<Self> {
        let record = sdk::account()?;
        let mut session = Self::from_secret(&record["secret"])
            .map_err(|_| Error::http(401, "DeepSeek 网页账号缺少有效凭证，请重新导入"))?;
        session.record = Some(record);
        Ok(session)
    }

    pub fn check(&self) -> Result<()> {
        let Some(before) = &self.record else {
            return Ok(());
        };
        let current = sdk::account()?;
        if sdk::account_disabled(&current)
            || current["public"]["userAgent"] != before["public"]["userAgent"]
            || ["id", "provider", "identity", "desktop", "secret"]
                .into_iter()
                .any(|key| current[key] != before[key])
        {
            return Err(
                Error::http(409, "DeepSeek 账号或凭据已变化，未继续请求或交付旧账号结果")
                    .with_code("deepseek_account_changed"),
            );
        }
        Ok(())
    }

    pub fn redact(&self, input: &str) -> String {
        let mut value = input.replace(&self.token, "[已隐藏凭证]");
        if !self.cookie.is_empty() {
            value = value.replace(&self.cookie, "[已隐藏凭证]");
            for pair in self.cookie.split(';') {
                if let Some((_, secret)) = pair.trim().split_once('=') {
                    if !secret.is_empty() {
                        value = value.replace(secret, "[已隐藏凭证]");
                    }
                }
            }
        }
        value
    }

    pub fn binding(&self) -> String {
        // 仅用于缓存失效，不作为鉴权证据。不会把凭证本体写进展示目录。
        format!(
            "{:x}",
            Md5::digest(format!(
                "{}\0{}\0{}",
                self.token, self.cookie, self.user_agent
            ))
        )
    }
}

fn user_id(user: &Value) -> Option<String> {
    user.get("id")
        .or_else(|| user.get("user_id"))
        .and_then(|value| {
            value
                .as_str()
                .map(str::trim)
                .filter(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
                .map(str::to_string)
                .or_else(|| value.as_u64().filter(|id| *id > 0).map(|id| id.to_string()))
        })
}

pub fn identity(secret: &Value) -> Result<String> {
    let session = Session::from_secret(secret)?;
    if let Some(id) = secret["userId"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
    {
        return Ok(format!("{}-user-{id}", crate::ID));
    }
    // 旧记录没有服务端ID，继续原摘要规则；不把易变令牌伪装成稳定用户身份。
    Ok(format!(
        "{}-{:x}",
        crate::ID,
        Md5::digest(session.token.as_bytes())
    ))
}

pub fn view(secret: &Value) -> Result<Value> {
    let session = Session::from_secret(secret)?;
    let tail: String = if session.token.len() <= 8 {
        String::new()
    } else {
        session
            .token
            .chars()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    };
    let user = secret["userId"].as_str().filter(|id| !id.is_empty());
    let uid = user
        .map(str::to_string)
        .unwrap_or_else(|| format!("{:x}", Md5::digest(session.token.as_bytes())));
    let name = field(secret, &["name", "displayName"]);
    let name = if name.is_empty() {
        if tail.is_empty() {
            "DeepSeek 网页账号".into()
        } else {
            format!("DeepSeek {tail}")
        }
    } else {
        session
            .redact(name)
            .chars()
            .filter(|character| !character.is_control())
            .take(100)
            .collect()
    };
    Ok(
        json!({ "name": name, "tokenTail": tail, "uid": uid, "credentialIdentity": user.is_none(),
        "hasToken": true, "hasCookie": !session.cookie.is_empty(), "canRefresh": false }),
    )
}

fn verified_user(response: &Value) -> Option<&Value> {
    if crate::errors::number(response.get("code")) != Some(0)
        || response
            .pointer("/data/biz_code")
            .is_some_and(|value| crate::errors::number(Some(value)) != Some(0))
    {
        return None;
    }
    let data = response.get("data").filter(|data| data.is_object())?;
    let payload = data.get("biz_data").unwrap_or(data);
    let user = payload.get("user").unwrap_or(payload);
    if user.is_object()
        && (user_id(user).is_some()
            || [
                "email",
                "mobile_number",
                "mobile",
                "phone",
                "username",
                "nickname",
                "name",
            ]
            .into_iter()
            .any(|key| {
                user[key]
                    .as_str()
                    .is_some_and(|value| !value.trim().is_empty())
            }))
    {
        Some(user)
    } else {
        None
    }
}

pub fn normalize(params: &Value) -> Result<Value> {
    normalize_fields(&input(params)?, params["name"].as_str())
}

pub fn normalize_fields(input: &Value, name: Option<&str>) -> Result<Value> {
    let session = Session::from_secret(input)?;
    // 与旧原生及官方网页相同，只验当前用户；PoW/人机验证不是保存账号的前提。
    let response = network::get(&session, "/api/v0/users/current")?;
    let user = verified_user(&response)
        .ok_or_else(|| Error::http(401, "DeepSeek 登录态校验失败：未返回有效用户信息"))?;
    let mut secret = json!({ "accessToken": session.token, "cookie": session.cookie, "userAgent": session.user_agent });
    if let Some(id) = user_id(user) {
        secret["userId"] = json!(id);
    }
    // 只保存在线核实的最小身份，不接受导入 JSON 自报的用户 ID。
    let display = field(
        user,
        &[
            "nickname",
            "name",
            "username",
            "email",
            "mobile_number",
            "mobile",
            "phone",
        ],
    );
    if !display.is_empty() {
        secret["displayName"] = json!(session
            .redact(display)
            .chars()
            .filter(|character| !character.is_control())
            .take(100)
            .collect::<String>());
    }
    let name = name.unwrap_or_else(|| field(input, &["name"])).trim();
    if !name.is_empty() {
        secret["name"] = json!(session
            .redact(name)
            .chars()
            .filter(|character| !character.is_control())
            .take(100)
            .collect::<String>());
    }
    Ok(
        json!({ "identity": identity(&secret)?, "public": view(&secret)?, "secret": secret, "desktop": false }),
    )
}
