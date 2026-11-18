//! 原生 DeepSeekHashV1 的纯 Rust 23 轮 Keccak-256；不是标准 24 轮 SHA3。
//! 保留正常挑战/计算/提交，不伪造答案、不移入本机 helper、不调用线程或 WASI。
//! 每 1024 次查询宿主时钟并保留 20 秒截止；同调用分段不会补充燃料。
//! 难度上限 2,000,000 是输入护栏，不代表宿主 20 亿燃料能计算该上限。
//! 参考旧 native deepseek/pow.rs 及 github.com/zyj1733683202/deepseek-2api/src/pow.ts。

use agent2api_wasm_sdk::{json, Error, Result, Value};
use base64::Engine;

use crate::util;

const MAX_DIFFICULTY: u64 = 2_000_000;
const CHECK_INTERVAL: u64 = 1024;
const MAX_MILLISECONDS: i64 = 20_000;

pub fn solve_for(challenge: &Value, path: &str) -> Result<String> {
    if !matches!(path, "/api/v0/chat/completion" | "/api/v0/file/upload_file") {
        return Err(Error::http(400, "DeepSeek 验证目标路径无效"));
    }
    if challenge["algorithm"].as_str() != Some("DeepSeekHashV1") {
        return Err(Error::http(502, "DeepSeek 网页验证算法已变化，请更新插件"));
    }
    let target = challenge["challenge"]
        .as_str()
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| Error::http(502, "DeepSeek 验证摘要无效"))?;
    let mut expected = [0u8; 32];
    for (index, byte) in expected.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&target[index * 2..index * 2 + 2], 16)
            .map_err(|_| Error::http(502, "DeepSeek 验证摘要无效"))?;
    }
    let difficulty = challenge["difficulty"]
        .as_u64()
        .filter(|value| *value > 0 && *value <= MAX_DIFFICULTY)
        .ok_or_else(|| Error::http(502, "DeepSeek 验证难度过高或无效"))?;
    let salt = challenge["salt"]
        .as_str()
        .filter(|value| {
            !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
        })
        .ok_or_else(|| Error::http(502, "DeepSeek 验证盐无效"))?;
    let signature = challenge["signature"]
        .as_str()
        .filter(|value| {
            !value.is_empty() && value.len() <= 8192 && !value.chars().any(char::is_control)
        })
        .ok_or_else(|| Error::http(502, "DeepSeek 验证签名缺失或无效"))?;
    if challenge
        .get("target_path")
        .is_some_and(|target| target.as_str() != Some(path))
    {
        return Err(Error::http(502, "DeepSeek 验证挑战不属于本次目标路径"));
    }
    let expiry = challenge["expire_at"]
        .as_i64()
        .filter(|expiry| *expiry > 0)
        .ok_or_else(|| Error::http(502, "DeepSeek 验证缺少有效期"))?;
    // expire_at 逐字用于挑战前缀；不猜单位，不把它改写成另一时间戳。
    let prefix = format!("{salt}_{expiry}_");
    let mut candidate = Vec::with_capacity(prefix.len() + 20);
    candidate.extend_from_slice(prefix.as_bytes());
    let started = util::now()?;
    for answer in 0..=difficulty {
        if answer % CHECK_INTERVAL == 0 {
            let now = util::now()?;
            if now < started || now.saturating_sub(started) > MAX_MILLISECONDS {
                return Err(
                    Error::http(504, "DeepSeek 网页验证计算超时，未提交对应请求")
                        .with_code("deepseek_pow_timeout"),
                );
            }
        }
        candidate.truncate(prefix.len());
        decimal(answer, &mut candidate);
        if hash(&candidate) == expected {
            let now = util::now()?;
            if now < started || now.saturating_sub(started) > MAX_MILLISECONDS {
                return Err(
                    Error::http(504, "DeepSeek 网页验证计算超时，未提交对应请求")
                        .with_code("deepseek_pow_timeout"),
                );
            }
            let value = json!({ "algorithm": "DeepSeekHashV1", "challenge": target, "salt": salt, "answer": answer,
                "signature": signature, "target_path": path });
            return Ok(base64::engine::general_purpose::STANDARD.encode(value.to_string()));
        }
    }
    Err(
        Error::http(502, "DeepSeek 网页验证没有找到有效答案，未提交对应请求")
            .with_code("deepseek_pow_unsolved"),
    )
}

fn decimal(mut value: u64, output: &mut Vec<u8>) {
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    output.extend_from_slice(&digits[at..]);
}

const ROUND: [u64; 24] = [
    0x1,
    0x8082,
    0x800000000000808a,
    0x8000000080008000,
    0x808b,
    0x80000001,
    0x8000000080008081,
    0x8000000000008009,
    0x8a,
    0x88,
    0x80008009,
    0x8000000a,
    0x8000808b,
    0x800000000000008b,
    0x8000000000008089,
    0x8000000000008003,
    0x8000000000008002,
    0x8000000000000080,
    0x800a,
    0x800000008000000a,
    0x8000000080008081,
    0x8000000000008080,
    0x80000001,
    0x8000000080008008,
];
const ROTATION: [[u32; 5]; 5] = [
    [0, 36, 3, 41, 18],
    [1, 44, 10, 45, 2],
    [62, 6, 43, 15, 61],
    [28, 55, 25, 21, 56],
    [27, 20, 39, 8, 14],
];

fn permutation(state: &mut [u64; 25]) {
    for round in &ROUND[1..] {
        let mut c = [0u64; 5];
        for x in 0..5 {
            for y in 0..5 {
                c[x] ^= state[x + 5 * y];
            }
        }
        for x in 0..5 {
            let d = c[(x + 4) % 5] ^ c[(x + 1) % 5].rotate_left(1);
            for y in 0..5 {
                state[x + 5 * y] ^= d;
            }
        }
        let mut b = [0u64; 25];
        for x in 0..5 {
            for y in 0..5 {
                b[y + 5 * ((2 * x + 3 * y) % 5)] = state[x + 5 * y].rotate_left(ROTATION[x][y]);
            }
        }
        for x in 0..5 {
            for y in 0..5 {
                state[x + 5 * y] =
                    b[x + 5 * y] ^ ((!b[(x + 1) % 5 + 5 * y]) & b[(x + 2) % 5 + 5 * y]);
            }
        }
        state[0] ^= round;
    }
}

fn absorb(state: &mut [u64; 25], block: &[u8]) {
    for (index, lane) in block.chunks_exact(8).enumerate() {
        let mut value = [0u8; 8];
        value.copy_from_slice(lane);
        state[index] ^= u64::from_le_bytes(value);
    }
    permutation(state);
}

fn hash(data: &[u8]) -> [u8; 32] {
    const RATE: usize = 136;
    let mut state = [0u64; 25];
    let mut blocks = data.chunks_exact(RATE);
    for block in &mut blocks {
        absorb(&mut state, block);
    }
    let rest = blocks.remainder();
    let mut last = [0u8; RATE];
    last[..rest.len()].copy_from_slice(rest);
    last[rest.len()] = 0x06;
    last[RATE - 1] ^= 0x80;
    absorb(&mut state, &last);
    let mut output = [0u8; 32];
    for (index, lane) in output.chunks_exact_mut(8).enumerate() {
        lane.copy_from_slice(&state[index].to_le_bytes());
    }
    output
}
