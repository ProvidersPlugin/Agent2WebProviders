use agent2api_wasm_sdk::{json, Error, Result, Value};

use crate::{
    credentials::{self, text, Credentials},
    network::Client,
    sign, Site,
};

/// 与旧原生包完全相同：有 uid 用 uid-*，否则用 token 的 SHA256。
pub fn identity(secret: &Value) -> String {
    let uid = text(secret, &["uid", "user_id", "userId"]);
    if !uid.is_empty() {
        return format!("uid-{uid}");
    }
    let token = text(secret, &["accessToken", "access_token", "token"]);
    if token.is_empty() {
        String::new()
    } else {
        format!("token-{}", sign::sha256_hex(token.as_bytes()))
    }
}

pub fn view(secret: &Value) -> Value {
    let parsed = Credentials::from_secret(secret).ok();
    let token = parsed
        .as_ref()
        .map(|value| value.token.clone())
        .unwrap_or_else(|| text(secret, &["accessToken", "access_token", "token"]));
    let uid = parsed
        .as_ref()
        .map(|value| value.uid.clone())
        .unwrap_or_else(|| text(secret, &["uid", "user_id", "userId"]));
    let name = text(secret, &["name", "nickname"]);
    let name = if !name.is_empty() {
        name
    } else if !uid.is_empty() {
        format!("Tabbit 账号 {uid}")
    } else {
        "Tabbit 账号".into()
    };
    // 宿主公开形态最多允许六位尾号；身份计算仍保留旧规则，尾号不是主键。
    let mut public = json!({ "name": name, "uid": uid, "tokenTail": sign::tail(&token),
        "hasSession": parsed.is_some(), "canRefresh": false,
        "unlockPro": credentials::boolean(secret, "unlockPro", true) });
    if let Some(exp) = credentials::jwt(&token).and_then(|value| credentials::jwt_expiry_ms(&value))
    {
        public["expiresAt"] = json!(exp);
    }
    public
}

fn payload(site: Site, params: &Value) -> Result<Value> {
    let source = &params["source"];
    match source["kind"].as_str() {
        Some("manual") => {
            let payload = &source["payload"];
            if let Some(index) = payload.get("methodIndex") {
                let index = index
                    .as_u64()
                    .ok_or_else(|| Error::http(400, format!("{}登录方式无效", site.label)))?;
                if index != 0 && !(site.id == "tabbit-intl" && index == 1) {
                    return Err(Error::http(400, format!("{}登录方式无效", site.label)));
                }
                let inputs = payload["inputs"]
                    .as_object()
                    .ok_or_else(|| Error::http(400, "Tabbit 登录字段格式无效"))?;
                if inputs.iter().any(|(key, value)| {
                    !(if index == 1 {
                        key == "credential"
                    } else {
                        matches!(
                            key.as_str(),
                            "cookie" | "token" | "unlockPro" | "clientVersion" | "name"
                        )
                    }) || !value.is_string()
                }) {
                    return Err(Error::http(400, "Tabbit 登录包含未申报字段"));
                }
                if index == 1 {
                    let raw = inputs
                        .get("credential")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    return credential_json(raw);
                }
                expand_fields(&Value::Object(inputs.clone()))
            } else if payload["inputs"].is_object() {
                expand_fields(&payload["inputs"])
            } else if let Some(raw) = payload.as_str() {
                credentials::from_text(raw)
            } else if payload.is_object() {
                expand_fields(payload)
            } else {
                Err(Error::http(400, "Tabbit 凭证必须是文本或字段对象"))
            }
        }
        Some("file_text") => credentials::from_text(source["content"].as_str().unwrap_or("")),
        _ => Err(Error::http(
            400,
            format!(
                "{}凭证导入请使用 Cookie / JWT 或文本文件，不支持本机登录态导入",
                site.label
            ),
        )),
    }
}

fn credential_json(raw: &str) -> Result<Value> {
    if raw.len() > 128 * 1024 {
        return Err(Error::http(400, "Tabbit 凭据 JSON 过大"));
    }
    let value: Value =
        serde_json::from_str(raw).map_err(|_| Error::http(400, "Tabbit 凭据 JSON 格式无效"))?;
    if !value.is_object() {
        return Err(Error::http(400, "Tabbit 凭据 JSON 必须是一份账号对象"));
    }
    Ok(value)
}

/// 单账号 JSON 仍走当前站点在线核对；不从文本中的 URL 决定请求目标。
fn expand_fields(fields: &Value) -> Result<Value> {
    let mut expanded = fields.clone();
    for key in ["token", "cookie"] {
        let Some(raw) = fields[key]
            .as_str()
            .map(str::trim)
            .filter(|raw| raw.starts_with('{'))
        else {
            continue;
        };
        let other = if key == "token" { "cookie" } else { "token" };
        if fields[other]
            .as_str()
            .is_some_and(|value| !value.trim().is_empty())
        {
            return Err(Error::http(
                400,
                "Tabbit 凭据 JSON 请勿与另一份 Cookie / JWT 混合填写",
            ));
        }
        expanded = credential_json(raw)?;
        for option in ["name", "unlockPro", "clientVersion"] {
            if let Some(value) = fields
                .get(option)
                .filter(|value| value.as_str().is_some_and(|value| !value.trim().is_empty()))
            {
                expanded[option] = value.clone();
            }
        }
        break;
    }
    Ok(expanded)
}

pub fn normalized_login(mut secret: Value) -> Result<Value> {
    let credentials = Credentials::from_secret(&secret)?;
    credentials.ensure_valid()?;
    let uid = credentials.uid.clone();
    if uid.is_empty() || uid.chars().any(char::is_control) {
        return Err(Error::http(
            502,
            "Tabbit 登录响应缺少有效账号身份，未保存账号",
        ));
    }
    check_import_identity(&secret, &credentials, &uid)?;
    secret["accessToken"] = json!(credentials.token);
    secret["cookie"] = json!(credentials.cookie);
    secret["uid"] = json!(credentials.uid);
    secret["unlockPro"] = json!(credentials.unlock_pro);
    secret["clientVersion"] = json!(credentials.client_version);
    if text(&secret, &["nickname"]).is_empty() {
        if let Some(claims) = credentials::jwt(&credentials.token) {
            let name = text(&claims, &["name", "display_name"]);
            if !name.is_empty() {
                secret["nickname"] = json!(name.chars().take(100).collect::<String>());
            }
        }
    }
    Ok(
        json!({ "identity": identity(&secret), "public": view(&secret), "secret": secret, "desktop": false }),
    )
}

fn identity_mismatch() -> Error {
    Error::http(400, "Tabbit 导入身份与上游当前用户不一致，未导入账号")
}

fn check_identity_fields(value: &Value, keys: &[&str], verified_uid: &str) -> Result<()> {
    for key in keys {
        if let Some(value) = value.get(*key) {
            if !value.as_str().is_some_and(|value| {
                value.len() <= 256
                    && !value.chars().any(char::is_control)
                    && (value.trim().is_empty() || value.trim() == verified_uid)
            }) {
                return Err(identity_mismatch());
            }
        }
    }
    Ok(())
}

fn check_import_identity(
    fields: &Value,
    credentials: &Credentials,
    verified_uid: &str,
) -> Result<()> {
    if verified_uid.is_empty() {
        return Err(identity_mismatch());
    }
    // 所有已提供的身份只能与同次上游真值核对，不能作为验证缺失时的候补。
    check_identity_fields(fields, &["uid", "user_id", "userId"], verified_uid)?;
    if !credentials.uid.is_empty() && credentials.uid != verified_uid {
        return Err(identity_mismatch());
    }
    // 只检查实际发送的 Cookie；同名项已由既有凭证归一逻辑固定。
    for part in credentials.cookie.split(';') {
        if let Some((name, value)) = part.trim().split_once('=') {
            if matches!(name, "user_id" | "SAPISID") && value.trim() != verified_uid {
                return Err(identity_mismatch());
            }
        }
    }
    // 只看本次实际所用 token 的声明，不验签、不接受声明代替上游身份。
    if let Some(claims) = credentials::jwt(&credentials.token) {
        if !claims.is_object() {
            return Err(identity_mismatch());
        }
        check_identity_fields(
            &claims,
            &["id", "user_id", "uid", "sub", "userId"],
            verified_uid,
        )?;
    }
    Ok(())
}

pub fn normalize(site: Site, params: &Value) -> Result<Value> {
    let fields = payload(site, params)?;
    let credentials = Credentials::from_secret(&fields)?;
    credentials.ensure_valid()?;
    let mut client = Client::temporary(site, credentials);
    let (verified_uid, nickname) = client.user_info().map_err(|mut error| {
        error.message = format!(
            "Tabbit 当前地区登录态校验失败，未导入账号：{}",
            error.message
        );
        error
    })?;
    check_import_identity(&fields, &client.credentials, &verified_uid)?;
    let credentials = client.credentials;
    let mut secret = json!({ "accessToken": credentials.token, "cookie": credentials.cookie,
        "uid": verified_uid, "unlockPro": credentials.unlock_pro,
        "clientVersion": credentials.client_version, "source": "manual" });
    // 账号接口补回 uid 后，把后续额度查询所需 Cookie 补齐；仍只保存许可的五项。
    let complete = Credentials::from_secret(&secret)?;
    secret["cookie"] = json!(complete.cookie);
    let mut name = text(params, &["name"]);
    if name.is_empty() {
        name = text(&fields, &["name"]);
    }
    if !name.is_empty() {
        secret["name"] = json!(name.chars().take(100).collect::<String>());
    }
    let nickname = if nickname.is_empty() {
        text(&fields, &["nickname"])
    } else {
        nickname
    };
    if !nickname.is_empty() {
        secret["nickname"] = json!(nickname.chars().take(100).collect::<String>());
    }
    Ok(
        json!({ "identity": identity(&secret), "public": view(&secret), "secret": secret, "desktop": false }),
    )
}
