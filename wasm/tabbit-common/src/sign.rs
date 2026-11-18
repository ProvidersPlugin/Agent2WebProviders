//! 保留原生请求的签名字节和指纹布局；随机数改由受控宿主提供，不使用时间伪随机。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result};
use base64::Engine;
use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use sha2::Sha256;

const CLIENT_ID: &str = "2dd8eb4c1ed9c344d173";

fn hex(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push_str(&format!("{byte:02x}"));
    }
    result
}

fn random_bytes(length: usize) -> Result<Vec<u8>> {
    let value = sdk::host("random.bytes", json!({ "length": length }))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value.as_str().unwrap_or(""))
        .map_err(|_| Error::new("主程序随机数格式无效"))?;
    if bytes.len() != length {
        return Err(Error::new("主程序随机数长度无效"));
    }
    Ok(bytes)
}

fn uuid(bytes: &[u8]) -> String {
    format!(
        "{}-{}-{}-{}-{}",
        hex(&bytes[..4]),
        hex(&bytes[4..6]),
        hex(&bytes[6..8]),
        hex(&bytes[8..10]),
        hex(&bytes[10..16])
    )
}

pub fn random_uuid() -> Result<String> {
    let mut bytes = random_bytes(16)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(uuid(&bytes))
}

pub fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    hex(&Sha256::digest(bytes.as_ref()))
}
pub fn md5_empty_hex() -> String {
    hex(&Md5::digest([]))
}
pub fn base64_text(text: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(text)
}

pub fn unique_uuid(default_browser: bool, now_ms: i64) -> Result<String> {
    let timestamp = format!("{:08x}", now_ms.div_euclid(1000).max(0));
    let timestamp = &timestamp[timestamp.len().saturating_sub(8)..];
    let positions = [2usize, 7, 11, 14, 18, 21, 25, 28];
    let random = random_bytes(32)?;
    let digits = b"0123456789abcdef";
    let non_one = b"023456789abcdef";
    let mut bytes = [0u8; 32];
    for index in 0..32 {
        bytes[index] = if let Some(slot) = positions.iter().position(|position| *position == index)
        {
            timestamp.as_bytes()[slot]
        } else if index == 5 {
            if default_browser {
                b'1'
            } else {
                non_one[usize::from(random[index]) % non_one.len()]
            }
        } else {
            digits[usize::from(random[index]) % digits.len()]
        };
    }
    let text = String::from_utf8_lossy(&bytes);
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &text[..8],
        &text[8..12],
        &text[12..16],
        &text[16..20],
        &text[20..]
    ))
}

fn device_id(uid: &str) -> String {
    let mut bytes = Sha256::digest(format!("tabbit-device:{uid}"));
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid(&bytes[..16])
}

pub fn chrome_consistency(uid: &str) -> String {
    format!("version=1,client_id={CLIENT_ID},device_id={},sync_account_id={uid},signin_mode=all_accounts,signout_mode=show_confirmation", device_id(uid))
}

/// 名称必须原样保留：x-signature 是随机 UUID，x-nonce 才是 HMAC。
pub fn signature_headers(
    key: &str,
    body: &str,
    now_ms: i64,
) -> Result<Vec<(&'static str, String)>> {
    let timestamp = now_ms.to_string();
    let signature = random_uuid()?;
    let message = format!("{timestamp}.{signature}.{}", sha256_hex(body.as_bytes()));
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes())
        .map_err(|_| Error::new("Tabbit 签名密钥格式无效"))?;
    mac.update(message.as_bytes());
    Ok(vec![
        ("x-timestamp", timestamp),
        ("x-signature", signature),
        ("x-nonce", hex(&mac.finalize().into_bytes())),
    ])
}

pub fn tail(token: &str) -> String {
    if token.chars().count() <= 8 {
        return String::new();
    }
    token
        .chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

pub fn escape(value: &str) -> String {
    let mut result = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            result.push(char::from(byte));
        } else {
            result.push_str(&format!("%{byte:02X}"));
        }
    }
    result
}
