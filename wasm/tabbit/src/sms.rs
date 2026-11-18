//! 精确复用原生国内协议：发码遇外部滑块时不宣称短信已发送，用户完成后沿同 uuid 重发。
//! 上游验证码仍由人输入；不猜 responseCode，也不自动重复发送短信。

use agent2api_wasm_sdk::{
    json,
    login::sms::{self, Challenge, Request},
    Error, Result, Value,
};
use agent2api_wasm_tabbit_common::{self as common, CLIENT_VERSION};

use crate::{sms_http as upstream, sms_state as state, sms_ui};

fn request(params: &Value, verify: bool) -> Result<Request> {
    state::flow(params)?;
    let payload = params["payload"]
        .as_object()
        .ok_or_else(|| Error::http(400, "短信登录参数无效"))?;
    if payload.keys().any(|key| {
        !matches!(
            key.as_str(),
            "methodIndex" | "phone" | "countryCode" | "code" | "ticket" | "captcha"
        )
    }) {
        return Err(Error::http(400, "短信登录包含未申报参数"));
    }
    let request: Request = serde_json::from_value(params["payload"].clone())
        .map_err(|_| Error::http(400, "短信登录参数格式无效"))?;
    let phone = request.phone.as_bytes();
    if request.method_index != sms_ui::METHOD
        || !request.country_code.is_empty()
        || phone.len() != 11
        || phone.first() != Some(&b'1')
        || !phone
            .get(1)
            .is_some_and(|byte| (b'2'..=b'9').contains(byte))
        || !phone.iter().all(u8::is_ascii_digit)
    {
        return Err(Error::http(400, "请填写 11 位大陆手机号"));
    }
    if !request.captcha.is_null() {
        return Err(Error::http(
            400,
            "Tabbit 滑块请在官方验证页手动完成，不接受自填验证结果",
        ));
    }
    if verify
        && (request.code.len() != 6 || !request.code.bytes().all(|byte| byte.is_ascii_digit()))
        || !verify && !request.code.is_empty()
    {
        return Err(Error::http(400, "请在验证码确认步骤填写 6 位数字验证码"));
    }
    Ok(request)
}

fn base(phone: &str, uuid: &str) -> Value {
    json!({ "uuid": uuid, "platform": "1", "version": "", "app": "1000", "mobile": phone })
}

pub fn send(params: &Value) -> Result<Value> {
    let request = request(params, false)?;
    // 第一次发码前就确认设备上下文可用，不能让用户收到短信后才发现主程序缺能力。
    let device = upstream::device(params)?;
    let mut pending = state::prepare_send(params, &request.phone, device)?;
    let uuid = state::uuid(&pending)?.to_string();
    let result: Result<Value> = (|| {
        state::before_send(&pending)?;
        let response = upstream::post(upstream::SEND_PATH, &base(&request.phone, &uuid))?;
        // 旧上游风控可能是 HTTP 200，也可能 4xx：必须先检查 requestCode / verifyUrl。
        if let Some(challenge) = upstream::challenge(&response.value)? {
            let url = challenge["verifyUrl"].as_str().unwrap_or("").to_string();
            pending.value["captcha"] = challenge;
            pending.value["phase"] = json!("captcha");
            pending.value["retry_at"] = json!(state::now()?);
            pending.value["attempts"] = json!(0);
            state::save(&mut pending)?;
            return Ok(sms::captcha(
                state::ticket(&pending),
                Challenge::External { url },
            ));
        }
        if !(200..300).contains(&response.status)
            || upstream::code(&response.value["code"]) != Some(0)
        {
            return Err(upstream::failure(&response, &request.phone, "", &uuid));
        }
        let now = state::now()?;
        let remaining = pending.value["expires_at"].as_i64().unwrap_or(now) - now;
        if remaining < 30_000 {
            return Err(Error::http(409, "本轮短信登录即将过期，请重新发起登录"));
        }
        let expires = (remaining / 1000).min(sms_ui::EXPIRES_SECONDS as i64) as u64;
        pending.value["phase"] = json!("sent");
        pending.value["captcha"] = Value::Null;
        pending.value["attempts"] = json!(0);
        pending.value["sent_until"] = json!(now + expires as i64 * 1000);
        pending.value["retry_at"] = json!(now + sms_ui::RESEND_SECONDS as i64 * 1000);
        state::save(&mut pending)?;
        Ok(sms::sent(
            state::ticket(&pending),
            sms_ui::RESEND_SECONDS,
            expires,
        ))
    })();
    if result.is_err() {
        state::failed(&mut pending, false);
    }
    result
}

pub fn verify(params: &Value) -> Result<Value> {
    let request = request(params, true)?;
    let mut pending = state::prepare_verify(params, &request.phone)?;
    let uuid = state::uuid(&pending)?.to_string();
    let result: Result<Value> = (|| {
        let mut body = base(&request.phone, &uuid);
        body["smsCode"] = json!(request.code);
        body["device_info"] = pending.value["device"].clone();
        state::before_send(&pending)?;
        let response = upstream::post(upstream::VERIFY_PATH, &body)?;
        if !(200..300).contains(&response.status) || response.value["success"] != true {
            return Err(upstream::failure(
                &response,
                &request.phone,
                &request.code,
                &uuid,
            ));
        }
        let cookie = upstream::cookie(&response.cookies)?;
        let cookie_token = upstream::token(&cookie);
        let reported_access = upstream::text(&response.value, &["access_token", "accessToken"]);
        if !cookie_token.is_empty()
            && !reported_access.is_empty()
            && cookie_token != reported_access
        {
            return Err(Error::http(
                502,
                "Tabbit 登录 Cookie 与响应令牌不一致，未保存账号",
            ));
        }
        let access = if cookie_token.is_empty() {
            reported_access
        } else {
            cookie_token
        };
        if access.is_empty() {
            return Err(Error::http(
                502,
                "Tabbit 登录成功但没有有效凭证，请改用 Cookie 导入",
            ));
        }
        let uid = upstream::text(&response.value, &["user_id", "userId", "id"]);
        let mut secret = json!({ "accessToken": access, "cookie": cookie, "uid": uid,
            "unlockPro": true, "clientVersion": CLIENT_VERSION, "source": "sms" });
        let name = upstream::text(
            &response.value,
            &["display_name", "nickname", "name", "user_name"],
        );
        if !name.is_empty() {
            secret["nickname"] = json!(name.chars().take(100).collect::<String>());
        }
        let account = common::normalized_login(secret)?;
        // 清掉手机号、uuid、挑战；密码和访问令牌从来不写登录状态。
        state::finish(&mut pending)?;
        Ok(account)
    })();
    if result.is_err() {
        state::failed(&mut pending, true);
    }
    result
}
