//! 手机号、uuid 与滑块票据只在宿主 flow 的 login 状态中保存；返回的 ticket 不含它们。
//! 同一 flow 重发保持 uuid；改手机号会创建新的 attempt，旧验证码不能用于新号码。

use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope, Snapshot},
    Error, Result, Value,
};
use base64::Engine;

const KEY: &str = "tabbit.sms";
const SCHEMA: u32 = 1;
const LIFETIME_MS: i64 = 300_000;

pub struct Pending {
    pub revision: u64,
    pub value: Value,
}

pub fn now() -> Result<i64> {
    let value = sdk::now();
    if value <= 0 {
        Err(Error::new("主程序没有提供有效的登录时间"))
    } else {
        Ok(value)
    }
}

pub fn flow(params: &Value) -> Result<&str> {
    params["flow"]
        .as_str()
        .filter(|flow| {
            flow.starts_with("sms_")
                && flow.len() <= 128
                && flow[4..].len() >= 32
                && flow[4..].bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .ok_or_else(|| {
            Error::http(503, "主程序尚未提供短信登录流程上下文，请更新主程序后再试")
                .with_code("tabbit_sms_context_missing")
        })
}

pub fn random(length: usize) -> Result<String> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let value = sdk::host("random.bytes", json!({ "length": length }))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value.as_str().unwrap_or(""))
        .map_err(|_| Error::new("主程序登录随机数格式无效"))?;
    if bytes.len() != length {
        return Err(Error::new("主程序登录随机数长度无效"));
    }
    Ok(bytes
        .iter()
        .map(|byte| ALPHABET[usize::from(*byte) % ALPHABET.len()] as char)
        .collect())
}

fn snapshot(params: &Value) -> Result<Snapshot> {
    flow(params)?;
    let snapshot = state::get(Scope::Login, KEY, SCHEMA)?;
    if !snapshot.compatible {
        return Err(Error::http(409, "短信登录状态格式已变化，请重新发起"));
    }
    Ok(snapshot)
}

fn check_ticket(ticket: &Value, value: &Value) -> Result<()> {
    let valid = ticket.as_object().is_some_and(|map| map.len() == 2)
        && ticket["version"] == 1
        && ticket["attempt"].as_str().is_some_and(|attempt| {
            attempt.len() == 32 && attempt.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
        && ticket["attempt"] == value["attempt"];
    if !valid {
        return Err(Error::http(
            409,
            "短信关联信息不属于当前登录步骤，请重新获取验证码",
        ));
    }
    Ok(())
}

fn active(value: &Value, current: i64) -> Result<()> {
    if value["phase"] == "done"
        || value["phase"] == "cancelled"
        || value["expires_at"].as_i64().unwrap_or(0) <= current
    {
        return Err(Error::http(409, "短信登录已结束或过期，请重新发起"));
    }
    if matches!(value["phase"].as_str(), Some("sending" | "verifying")) {
        return Err(Error::http(409, "当前短信操作仍在进行，请稍后再试"));
    }
    Ok(())
}

pub fn prepare_send(params: &Value, phone: &str, device: Value) -> Result<Pending> {
    let snapshot = snapshot(params)?;
    let ticket = &params["payload"]["ticket"];
    let current = now()?;
    let mut pending = Pending {
        revision: snapshot.revision,
        value: snapshot.value,
    };
    let first = pending.value.is_null();
    if !first {
        active(&pending.value, current)?;
    }
    if !first && pending.value["expires_at"].as_i64().unwrap_or(current) - current < 30_000 {
        return Err(Error::http(409, "本轮短信登录即将过期，请重新发起登录"));
    }
    if !ticket.is_null() {
        check_ticket(ticket, &pending.value)?;
    }
    if !ticket.is_null() && pending.value["phone"] != phone {
        return Err(Error::http(409, "短信关联信息对应另一个手机号"));
    }
    let reset = ticket.is_null()
        && (pending.value["phone"] != phone
            || matches!(pending.value["phase"].as_str(), Some("sent" | "captcha")));
    if first || reset {
        let deadline = if first {
            current + LIFETIME_MS
        } else {
            pending.value["expires_at"].as_i64().unwrap_or(current)
        };
        pending.value = json!({ "phone": phone, "uuid": random(64)?, "attempt": random(32)?,
            "device": device, "expires_at": deadline, "sent_until": 0,
            "retry_at": 0, "attempts": 0, "phase": "ready", "captcha": null });
    } else if pending.value["device"] != device {
        return Err(Error::http(409, "短信设备上下文已变化，请重新发起登录"));
    }
    if pending.value["retry_at"].as_i64().unwrap_or(0) > current {
        return Err(Error::http(429, "请等待短信重发倒计时结束"));
    }
    pending.value["phase"] = json!("sending");
    pending.value["sent_until"] = json!(0);
    pending.value["retry_at"] = json!(current + 60_000);
    save(&mut pending)?;
    Ok(pending)
}

pub fn prepare_verify(params: &Value, phone: &str) -> Result<Pending> {
    let snapshot = snapshot(params)?;
    let mut pending = Pending {
        revision: snapshot.revision,
        value: snapshot.value,
    };
    active(&pending.value, now()?)?;
    check_ticket(&params["payload"]["ticket"], &pending.value)?;
    if pending.value["phone"] != phone
        || pending.value["phase"] != "sent"
        || pending.value["sent_until"].as_i64().unwrap_or(0) <= now()?
    {
        return Err(Error::http(400, "验证码已过期或不是当前手机号，请重新获取"));
    }
    let attempts = pending.value["attempts"].as_u64().unwrap_or(0);
    if attempts >= 5 {
        return Err(Error::http(429, "验证码尝试次数过多，请重新获取"));
    }
    pending.value["attempts"] = json!(attempts + 1);
    pending.value["phase"] = json!("verifying");
    save(&mut pending)?;
    Ok(pending)
}

pub fn before_send(pending: &Pending) -> Result<()> {
    let snapshot = state::get(Scope::Login, KEY, SCHEMA)?;
    if !snapshot.compatible
        || snapshot.revision != pending.revision
        || snapshot.value != pending.value
    {
        return Err(Error::http(409, "短信登录已取消或状态改变，未继续发送"));
    }
    if pending.value["expires_at"].as_i64().unwrap_or(0) <= now()? {
        return Err(Error::http(409, "短信登录已过期，未继续发送"));
    }
    Ok(())
}

pub fn save(pending: &mut Pending) -> Result<()> {
    let current = now()?;
    let ttl = pending.value["expires_at"]
        .as_i64()
        .unwrap_or(current + LIFETIME_MS)
        - current;
    if ttl <= 0 {
        return Err(Error::http(409, "短信登录已过期，请重新发起"));
    }
    // 直接用受控 state 方法取新 revision，不进行第二次读取/覆盖。
    let result = sdk::host(
        "state.compare_exchange",
        json!({ "scope": "login", "key": KEY,
        "schema": SCHEMA, "revision": pending.revision, "value": pending.value, "ttl_ms": ttl }),
    )?;
    if result["updated"] != true {
        return Err(Error::http(409, "短信登录状态已改变，未继续操作"));
    }
    pending.revision = result["revision"]
        .as_u64()
        .ok_or_else(|| Error::new("主程序未返回登录状态版本"))?;
    Ok(())
}

pub fn uuid(pending: &Pending) -> Result<&str> {
    pending.value["uuid"]
        .as_str()
        .filter(|uuid| uuid.len() == 64 && uuid.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        .ok_or_else(|| Error::http(409, "短信登录关联标识无效，请重新发起"))
}

pub fn ticket(pending: &Pending) -> Value {
    json!({ "version": 1, "attempt": pending.value["attempt"] })
}

pub fn failed(pending: &mut Pending, verifying: bool) {
    if matches!(pending.value["phase"].as_str(), Some("done" | "cancelled")) {
        return;
    }
    pending.value["phase"] = json!(if verifying { "sent" } else { "failed" });
    let _ = save(pending);
}

pub fn finish(pending: &mut Pending) -> Result<()> {
    let deadline = pending.value["expires_at"].clone();
    pending.value = json!({ "phase": "done", "expires_at": deadline });
    save(pending)
}

pub fn cancel(params: &Value) -> Result<()> {
    let snapshot = snapshot(params)?;
    if !state::compare_exchange(
        Scope::Login,
        KEY,
        SCHEMA,
        snapshot.revision,
        Value::Null,
        1000,
    )? {
        return Err(Error::http(409, "短信登录取消状态已变化"));
    }
    // 宿主 login.cancel 的守卫还会移除整个 provider:flow 命名空间。
    Ok(())
}
