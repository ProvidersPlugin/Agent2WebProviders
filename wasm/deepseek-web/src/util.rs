use agent2api_wasm_sdk::{self as sdk, json, Error, Result};
use base64::Engine;

pub fn now() -> Result<i64> {
    sdk::host("clock.now", serde_json::Value::Null)?
        .as_i64()
        .filter(|time| *time > 0)
        .ok_or_else(|| Error::http(503, "主程序没有提供有效时间"))
}

pub fn id(prefix: &str) -> Result<String> {
    let value = sdk::host("random.bytes", json!({ "length": 16 }))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value.as_str().unwrap_or(""))
        .map_err(|_| Error::http(503, "主程序随机数据无效"))?;
    if bytes.len() != 16 {
        return Err(Error::http(503, "主程序随机数据长度无效"));
    }
    Ok(format!(
        "{prefix}-{}",
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

pub fn query(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}
