//! Tabbit 请求签名与客户端指纹。
//!
//! ── 签名（实测，按线上行为为准）──────────────────────────────
//! ```text
//!   x-timestamp  Date.now() 毫秒
//!   x-signature  随机 UUID v4
//!   x-nonce      HMAC-SHA256(signKey, "{ts}.{uuid}.{sha256hex(body)}") 的 hex
//! ```
//! **命名是反的**：`x-signature` 是那个随机 UUID，`x-nonce` 才是 HMAC。
//! 签名 key 先取 `GET /chat/sign-key`，服务端会轮换；请求返回 499 时刷新
//! 一次 key 重试。两站的兜底 key 不同（见 `config.rs`）。
//!
//! ── 指纹头（上游按它判客户端形态）────────────────────────────
//! - `x-req-ctx`：base64 的客户端版本号，形如 `1.17.16(10117016)`；
//! - `unique-uuid`：第 5 位是「是否默认浏览器」标记位，位置
//!   `[2,7,11,14,18,21,25,28]` 编码当前时间的 hex（防重放）；
//! - `x-chrome-id-consistency-request`：固定的 Chrome 集成三元组。
//!
//! 本适配器按账号上的 `unlockPro` 决定第 5 位（默认开启，见 `mod.rs` 的说明）。

use base64::Engine as _;
use hmac::{Hmac, Mac};
use md5::{Digest as _, Md5};
use sha2::Sha256;

use super::config::CLIENT_ID;

type HmacSha256 = Hmac<Sha256>;

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    if getrandom::getrandom(&mut bytes).is_ok() {
        return bytes;
    }
    // 极端情况下退化成时间派生：上游只把它当随机标识，不参与验签。
    let seed = format!("tabbit-random:{}", crate::http::now_ms());
    let digest = sha2::Sha256::digest(seed.as_bytes());
    for (index, slot) in bytes.iter_mut().enumerate() {
        *slot = digest[index % digest.len()];
    }
    bytes
}

/// v4 UUID 字符串。
pub fn random_uuid() -> String {
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&random_bytes(16));
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{}-{}-{}-{}-{}",
        hex(&bytes[0..4]),
        hex(&bytes[4..6]),
        hex(&bytes[6..8]),
        hex(&bytes[8..10]),
        hex(&bytes[10..16])
    )
}

pub fn sha256_hex(input: impl AsRef<[u8]>) -> String {
    hex(&sha2::Sha256::digest(input.as_ref()))
}

/// 上游 `entity.key` 的固定值（前端对一个空 tab 取的 md5）。
pub fn md5_empty_hex() -> String {
    hex(&Md5::digest([]))
}

pub fn base64_text(input: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(input.as_bytes())
}

/// `unique-uuid`：第 5 位是「默认浏览器」标记，另 8 个位置编码当前秒的 hex。
pub fn unique_uuid(default_browser: bool, now_ms: i64) -> String {
    let seconds = now_ms.div_euclid(1000).max(0) as u64;
    let timestamp = format!("{seconds:08x}");
    let timestamp = &timestamp[timestamp.len().saturating_sub(8)..];
    let positions = [2usize, 7, 11, 14, 18, 21, 25, 28];
    let random = random_bytes(32);
    let digits = b"0123456789abcdef";
    let non_one = b"023456789abcdef";
    let mut out = [0u8; 32];
    for index in 0..32 {
        if let Some(slot) = positions.iter().position(|position| *position == index) {
            out[index] = timestamp.as_bytes()[slot];
            continue;
        }
        if index == 5 {
            out[index] = if default_browser {
                b'1'
            } else {
                non_one[(random[index] as usize) % non_one.len()]
            };
            continue;
        }
        out[index] = digits[(random[index] as usize) % digits.len()];
    }
    let text = String::from_utf8_lossy(&out).to_string();
    format!(
        "{}-{}-{}-{}-{}",
        &text[0..8],
        &text[8..12],
        &text[12..16],
        &text[16..20],
        &text[20..32]
    )
}

/// 由 uid 派生稳定的设备 UUID（同一账号每次一致，避免上游把设备当新设备）。
pub fn device_id(uid: &str) -> String {
    let mut digest = sha2::Sha256::digest(format!("tabbit-device:{uid}").as_bytes()).to_vec();
    digest.truncate(16);
    if digest.len() < 16 {
        digest.resize(16, 0);
    }
    digest[6] = (digest[6] & 0x0f) | 0x40;
    digest[8] = (digest[8] & 0x3f) | 0x80;
    format!(
        "{}-{}-{}-{}-{}",
        hex(&digest[0..4]),
        hex(&digest[4..6]),
        hex(&digest[6..8]),
        hex(&digest[8..10]),
        hex(&digest[10..16])
    )
}

/// 签名头三件套。
pub fn signature_headers(sign_key: &str, body: &str, now_ms: i64) -> Vec<(String, String)> {
    let timestamp = now_ms.to_string();
    let uuid = random_uuid();
    let digest = sha256_hex(body.as_bytes());
    let message = format!("{timestamp}.{uuid}.{digest}");
    let nonce = match HmacSha256::new_from_slice(sign_key.as_bytes()) {
        Ok(mut mac) => {
            mac.update(message.as_bytes());
            hex(&mac.finalize().into_bytes())
        }
        Err(_) => String::new(),
    };
    vec![
        ("x-timestamp".to_string(), timestamp),
        ("x-signature".to_string(), uuid),
        ("x-nonce".to_string(), nonce),
    ]
}

/// `x-chrome-id-consistency-request` 的固定三元组。
pub fn chrome_consistency(uid: &str) -> String {
    format!(
        "version=1,client_id={CLIENT_ID},device_id={},sync_account_id={uid},signin_mode=all_accounts,signout_mode=show_confirmation",
        device_id(uid)
    )
}

/// 用户可见的短令牌尾巴。
pub fn token_tail(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() <= 8 {
        return String::new();
    }
    chars[chars.len() - 8..].iter().collect()
}
