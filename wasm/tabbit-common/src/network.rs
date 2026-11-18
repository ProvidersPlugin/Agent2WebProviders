use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};

use crate::{
    credentials::{self, Credentials},
    errors, sign, state, Site, ROOM_TTL_MS,
};

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";
const SEC_CH_UA: &str =
    "\"Tabbit Browser\";v=\"149\", \"Chromium\";v=\"149\", \"Not)A;Brand\";v=\"24\"";
const SIGN_STATE: &str = "tabbit.sign-key";

pub fn prelogin_headers(site: Site) -> Result<Value> {
    Ok(json!({
        "User-Agent": USER_AGENT, "sec-ch-ua": SEC_CH_UA,
        "sec-ch-ua-platform": "\"Windows\"", "sec-ch-ua-mobile": "?0",
        "Accept": "*/*", "Content-Type": "application/json", "Origin": site.origin,
        "Referer": format!("{}/login?callback=close", site.origin),
        "x-req-ctx": sign::base64_text(crate::CLIENT_VERSION),
        "unique-uuid": sign::unique_uuid(false, crate::now()?)?,
        "trace-id": sign::random_uuid()?, "Cookie": "NEXT_LOCALE=zh"
    }))
}

pub struct Client {
    pub site: Site,
    pub credentials: Credentials,
    pub binding: String,
    persistent: bool,
    original: Option<Value>,
    sign_key: Option<String>,
}

impl Client {
    /// 调用者持有账号锁；每个请求及响应仍复核原宿主账号，不用锁代替切号保护。
    pub fn for_account(site: Site) -> Result<Self> {
        let record = sdk::account()?;
        if record["provider"] != site.id || sdk::account_disabled(&record) {
            return Err(account_changed());
        }
        let credentials = Credentials::from_secret(&record["secret"])?;
        credentials.ensure_valid()?;
        let mut client = Self::new(site, credentials, true);
        client.original = Some(account_binding(&record));
        Ok(client)
    }

    pub fn current(&self) -> Result<()> {
        if let Some(original) = &self.original {
            let record = sdk::account()?;
            if sdk::account_disabled(&record) || account_binding(&record) != *original {
                return Err(account_changed());
            }
        }
        Ok(())
    }

    pub fn checkin_binding(&self) -> Result<String> {
        let original = self.original.as_ref().ok_or_else(account_changed)?;
        self.current()?;
        Ok(sign::sha256_hex(format!(
            "{}\0{}\0{}",
            self.site.id, original["id"], original["identity"]
        )))
    }

    pub fn temporary(site: Site, credentials: Credentials) -> Self {
        Self::new(site, credentials, false)
    }

    fn new(site: Site, credentials: Credentials, persistent: bool) -> Self {
        let binding = sign::sha256_hex(format!(
            "{}\0{}\0{}\0{}\0{}",
            site.id,
            credentials.uid,
            credentials.cookie,
            credentials.unlock_pro,
            credentials.client_version
        ));
        Self {
            site,
            credentials,
            binding,
            persistent,
            original: None,
            sign_key: None,
        }
    }

    pub fn headers(&self, referer: &str) -> Result<Value> {
        Ok(json!({
            "User-Agent": USER_AGENT, "sec-ch-ua": SEC_CH_UA,
            "sec-ch-ua-platform": "\"Windows\"", "sec-ch-ua-mobile": "?0",
            "x-chrome-id-consistency-request": sign::chrome_consistency(&self.credentials.uid),
            "Referer": format!("{}{referer}", self.site.origin), "Origin": self.site.origin,
            "x-req-ctx": sign::base64_text(&self.credentials.client_version),
            "unique-uuid": sign::unique_uuid(self.credentials.unlock_pro, crate::now()?)?
        }))
    }

    pub fn open(
        &self,
        method: &str,
        path: &str,
        headers: Value,
        body: Option<&str>,
    ) -> Result<http::Response> {
        // 路径来自本实现，永远不能把响应提供的地址或粘贴的 cURL 当请求目标。
        if !path.starts_with('/') || path.starts_with("//") || path.contains(['\r', '\n', '#']) {
            return Err(Error::http(400, "Tabbit 请求路径无效"));
        }
        self.current()?;
        http::open_raw(
            method,
            &format!("{}{path}", self.site.origin),
            headers,
            body.map(str::as_bytes),
        )
        .map_err(|error| self.safe(error))
    }

    fn safe(&self, mut error: Error) -> Error {
        error.message = self.credentials.redact(&error.message);
        error
    }

    pub fn json_response(&self, response: http::Response) -> Result<Value> {
        let status = response.status;
        let bytes = response
            .bytes(4 * 1024 * 1024)
            .map_err(|error| self.safe(error))?;
        self.current()?;
        let value = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
        if !(200..300).contains(&status) || errors::business_failure(&value) {
            return Err(errors::upstream(status, &value, &self.credentials));
        }
        if !value.is_object() && !value.is_array() {
            return Err(Error::http(502, "Tabbit 上游没有返回有效 JSON"));
        }
        Ok(value)
    }

    fn fetch_sign_key(&self) -> Result<String> {
        let mut headers = self.headers("/")?;
        headers["Accept"] = json!("text/plain");
        headers["Cookie"] = json!(self.credentials.cookie);
        let response = self.open("GET", "/chat/sign-key", headers, None)?;
        let status = response.status;
        let bytes = response.bytes(4096).map_err(|error| self.safe(error))?;
        self.current()?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| Error::http(502, "Tabbit 签名密钥格式无效"))?
            .trim();
        if status != 200
            || text.is_empty()
            || text.len() > 512
            || text.contains(['<', '>', ' ', '\n', '\r', '{', '}', '"'])
        {
            return Err(Error::http(502, "Tabbit 签名密钥未能获取"));
        }
        Ok(text.to_string())
    }

    fn key(&mut self) -> Result<String> {
        if let Some(key) = &self.sign_key {
            return Ok(key.clone());
        }
        if self.persistent {
            if let Some(value) = state::get(SIGN_STATE, &self.binding)? {
                if let Some(key) = value
                    .as_str()
                    .filter(|key| !key.is_empty() && key.len() <= 512)
                {
                    self.sign_key = Some(key.to_string());
                    return Ok(key.to_string());
                }
            }
        }
        let key = match self.fetch_sign_key() {
            Ok(key) => {
                self.current()?;
                if self.persistent {
                    state::put(SIGN_STATE, &self.binding, json!(key), ROOM_TTL_MS)?;
                }
                key
            }
            Err(error) if error.status == Some(409) || error.status == Some(401) => {
                return Err(error)
            }
            Err(_) => self.site.fallback_sign_key.to_string(),
        };
        self.sign_key = Some(key.clone());
        Ok(key)
    }

    pub fn refresh_sign_key(&mut self) -> Result<()> {
        let key = self.fetch_sign_key()?;
        if self.persistent {
            state::put(SIGN_STATE, &self.binding, json!(key), ROOM_TTL_MS)?;
        }
        self.sign_key = Some(key);
        Ok(())
    }

    pub fn signed_headers(
        &mut self,
        referer: &str,
        body: &str,
        authorization: bool,
    ) -> Result<Value> {
        let mut headers = self.headers(referer)?;
        headers["Cookie"] = json!(self.credentials.cookie);
        headers["trace-id"] = json!(sign::random_uuid()?);
        if authorization {
            headers["Authorization"] = json!(format!("Bearer {}", self.credentials.token));
        }
        let key = self.key()?;
        for (name, value) in sign::signature_headers(&key, body, crate::now()?)? {
            headers[name] = json!(value);
        }
        Ok(headers)
    }

    pub fn signed_json(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        referer: &str,
    ) -> Result<Value> {
        // /panel/session 的空 body 必须签空串，不能变成 JSON null。
        let body = body
            .filter(|value| !value.is_null())
            .map(Value::to_string)
            .unwrap_or_default();
        let mut headers = self.signed_headers(referer, &body, true)?;
        headers["Accept"] = json!("application/json");
        if method == "POST" {
            headers["Content-Type"] = json!("application/json");
        }
        let response = self.open(
            method,
            path,
            headers,
            (method == "POST").then_some(body.as_str()),
        )?;
        self.json_response(response)
    }

    pub fn user_info(&mut self) -> Result<(String, String)> {
        // 只核对本次固定站点的 base-info，不把通用 JSON 成功视为身份已验证。
        // 国内当前网页走 /proxy/v0；国际站保留已确认的原路径，不跨地区回退。
        let path = if self.site.id == "tabbit" {
            "/proxy/v0/user/base-info"
        } else {
            "/api/v0/user/base-info"
        };
        let value = self.signed_json("GET", path, None, "/")?;
        let root = value.as_object().ok_or_else(user_info_failed)?;
        let info = root
            .get("user_info")
            .filter(|value| value.is_object())
            .ok_or_else(user_info_failed)?;
        if ![&value, info].into_iter().all(user_info_business_ok) {
            return Err(user_info_failed());
        }
        // 原协议只确认 user_info 下这三个身份字段；不猜 data 包装或从 JWT 补回。
        // 参考 tabbit2api-panel c164b1db 的 internal/upstream/client.go:GetUserInfo。
        let mut uid: Option<&str> = None;
        for key in ["user_id", "uid", "id"] {
            if let Some(value) = info.get(key) {
                let current = value
                    .as_str()
                    .filter(|value| value.len() <= 256 && !value.chars().any(char::is_control))
                    .map(str::trim)
                    .filter(|value| !value.is_empty() && !value.contains(';'))
                    .ok_or_else(user_info_failed)?;
                if uid.is_some_and(|uid| uid != current) {
                    return Err(user_info_failed());
                }
                uid = Some(current);
            }
        }
        let uid = uid.ok_or_else(user_info_failed)?.to_string();
        let nickname = credentials::text(info, &["display_name", "nickname", "name", "user_name"]);
        Ok((uid, nickname))
    }
}

fn account_binding(record: &Value) -> Value {
    json!({ "id": record["id"], "provider": record["provider"],
        "identity": record["identity"], "secret": record["secret"], "desktop": record["desktop"] })
}

fn account_changed() -> Error {
    Error::http(
        409,
        "Tabbit 原账号已停用或凭据已变化，本轮未继续发送或交付结果",
    )
    .with_code("tabbit_account_changed")
}

fn user_info_failed() -> Error {
    Error::http(502, "Tabbit 当前用户信息未通过校验，未导入账号")
}

/// base-info 没有已证必带的成功码；已出现的业务标记不得失败、含糊或格式异常。
fn user_info_business_ok(value: &Value) -> bool {
    !value
        .get("success")
        .is_some_and(|value| value.as_bool() != Some(true))
        && !value
            .get("code")
            .is_some_and(|value| value.as_i64() != Some(0) && value.as_str() != Some("0"))
        && !["error", "error_message", "detail"]
            .into_iter()
            .any(|key| value.get(key).is_some_and(|value| !value.is_null()))
}
