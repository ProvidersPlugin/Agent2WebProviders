use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use base64::Engine;
use md5::{Digest, Md5};

const SIGN_SECRET: &str = "8a1317a7468aa3ad86e997d08f3f31cb";

pub fn now() -> Result<i64> {
    let value = sdk::now();
    if value > 0 {
        Ok(value)
    } else {
        Err(Error::new("主程序未提供有效时间，未发送智谱请求"))
    }
}

pub fn uuid() -> Result<String> {
    let value = sdk::host("random.bytes", json!({ "length": 16 }))?;
    let mut bytes = base64::engine::general_purpose::STANDARD
        .decode(value.as_str().unwrap_or(""))
        .map_err(|_| Error::new("主程序随机数格式无效"))?;
    if bytes.len() != 16 {
        return Err(Error::new("主程序随机数长度无效"));
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

pub fn valid_device(value: &str) -> bool {
    let parts: Vec<_> = value.split('-').collect();
    parts.len() == 5
        && parts.iter().zip([8, 4, 4, 4, 12]).all(|(part, length)| {
            part.len() == length && part.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

/// 客户端 getXTimestamp：以其余数字和模10替换倒数第二位，不可直接发送 Unix 毫秒串。
fn timestamp() -> Result<String> {
    let mut bytes = now()?.to_string().into_bytes();
    if bytes.len() < 2 {
        return Err(Error::new("智谱签名时间无效"));
    }
    let at = bytes.len() - 2;
    let sum: u32 = bytes
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != at)
        .map(|(_, byte)| u32::from(*byte - b'0'))
        .sum();
    bytes[at] = b'0' + (sum % 10) as u8;
    String::from_utf8(bytes).map_err(|_| Error::new("智谱签名时间无法编码"))
}

pub fn headers(token: &str, device: &str, accept: &str) -> Result<Value> {
    if !valid_device(device) {
        return Err(Error::http(400, "智谱设备号必须是有效 UUID"));
    }
    let timestamp = timestamp()?;
    let nonce = uuid()?.replace('-', "");
    let signature = format!(
        "{:x}",
        Md5::digest(format!("{timestamp}-{nonce}-{SIGN_SECRET}"))
    );
    let mut result = json!({ "Content-Type": "application/json;charset=utf-8", "App-Name": "chatglm",
        "X-Device-Id": device, "X-App-Platform": "pc", "X-App-Version": "2.0.7",
        "X-Timestamp": timestamp, "X-Nonce": nonce, "X-Sign": signature,
        "Origin": crate::endpoints::ORIGIN, "Referer": format!("{}/", crate::endpoints::ORIGIN),
        "User-Agent": "undici", "Accept": accept });
    if !token.trim().is_empty() {
        result["Authorization"] = json!(format!("Bearer {}", token.trim()));
    }
    Ok(result)
}
