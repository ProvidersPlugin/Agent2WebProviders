//! DeepSeekHashV1 是浏览器公开的 23 轮 Keccak-256 工作量证明，不使用绕过或伪造答案。
//! 正常验证流程的参考：github.com/zyj1733683202/deepseek-2api/src/pow.ts。

use serde_json::{json, Value};
use base64::Engine;
use std::time::{Duration, Instant};

pub fn solve(challenge: &Value) -> Result<String, String> {
    if challenge["algorithm"].as_str() != Some("DeepSeekHashV1") { return Err("DeepSeek 网页验证算法已变化，请更新插件".into()); }
    let target = challenge["challenge"].as_str().ok_or("DeepSeek 验证缺少 challenge")?;
    let mut expected = [0u8; 32];
    if target.len() != 64 || !target.bytes().all(|byte| byte.is_ascii_hexdigit()) { return Err("DeepSeek 验证摘要无效".into()); }
    for (index, byte) in expected.iter_mut().enumerate() { *byte = u8::from_str_radix(&target[index * 2..index * 2 + 2], 16).map_err(|_| "DeepSeek 验证摘要无效")?; }
    let difficulty = challenge["difficulty"].as_u64().filter(|value| *value <= 2_000_000).ok_or("DeepSeek 验证难度过高或无效")?;
    let salt = challenge["salt"].as_str().filter(|value| value.len() <= 256).ok_or("DeepSeek 验证盐无效")?;
    let expiry = challenge["expire_at"].as_i64().ok_or("DeepSeek 验证缺少有效期")?;
    let prefix = format!("{salt}_{expiry}_");
    let started = Instant::now();
    for answer in 0..=difficulty {
        if answer % 1024 == 0 && started.elapsed() > Duration::from_secs(20) { return Err("DeepSeek 网页验证计算超时，未提交对话".into()); }
        if hash(format!("{prefix}{answer}").as_bytes()) == expected {
            let value = json!({ "algorithm": "DeepSeekHashV1", "challenge": target, "salt": salt, "answer": answer,
                "signature": challenge.get("signature"), "target_path": "/api/v0/chat/completion" });
            return Ok(base64::engine::general_purpose::STANDARD.encode(value.to_string()));
        }
    }
    Err("DeepSeek 网页验证没有找到有效答案，未提交对话".into())
}

const ROUND: [u64; 24] = [
    0x1, 0x8082, 0x800000000000808a, 0x8000000080008000, 0x808b, 0x80000001,
    0x8000000080008081, 0x8000000000008009, 0x8a, 0x88, 0x80008009, 0x8000000a,
    0x8000808b, 0x800000000000008b, 0x8000000000008089, 0x8000000000008003,
    0x8000000000008002, 0x8000000000000080, 0x800a, 0x800000008000000a,
    0x8000000080008081, 0x8000000000008080, 0x80000001, 0x8000000080008008,
];
const ROTATION: [[u32; 5]; 5] = [[0, 36, 3, 41, 18], [1, 44, 10, 45, 2], [62, 6, 43, 15, 61], [28, 55, 25, 21, 56], [27, 20, 39, 8, 14]];

fn permutation(state: &mut [u64; 25]) {
    for round in &ROUND[1..] {
        let mut c = [0u64; 5];
        for x in 0..5 { for y in 0..5 { c[x] ^= state[x + 5 * y]; } }
        for x in 0..5 {
            let d = c[(x + 4) % 5] ^ c[(x + 1) % 5].rotate_left(1);
            for y in 0..5 { state[x + 5 * y] ^= d; }
        }
        let mut b = [0u64; 25];
        for x in 0..5 { for y in 0..5 { b[y + 5 * ((2 * x + 3 * y) % 5)] = state[x + 5 * y].rotate_left(ROTATION[x][y]); } }
        for x in 0..5 { for y in 0..5 { state[x + 5 * y] = b[x + 5 * y] ^ ((!b[(x + 1) % 5 + 5 * y]) & b[(x + 2) % 5 + 5 * y]); } }
        state[0] ^= round;
    }
}

fn hash(data: &[u8]) -> [u8; 32] {
    const RATE: usize = 136;
    let mut bytes = data.to_vec();
    bytes.push(0x06);
    let length = bytes.len().div_ceil(RATE) * RATE;
    bytes.resize(length, 0);
    bytes[length - 1] ^= 0x80;
    let mut state = [0u64; 25];
    for block in bytes.chunks_exact(RATE) {
        for (index, lane) in block.chunks_exact(8).enumerate() {
            let mut value = [0u8; 8]; value.copy_from_slice(lane); state[index] ^= u64::from_le_bytes(value);
        }
        permutation(&mut state);
    }
    let mut output = [0u8; 32];
    for (index, lane) in output.chunks_exact_mut(8).enumerate() { lane.copy_from_slice(&state[index].to_le_bytes()); }
    output
}
