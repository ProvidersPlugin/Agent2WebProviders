//! 通过主程序受控能力执行 DeepSeekHashV1 工作量证明计算，避免纯 Wasm 计算耗尽燃料。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub fn solve_for(challenge: &Value, path: &str) -> Result<String> {
    if !challenge.is_object() {
        return Err(Error::http(502, "DeepSeek 没有返回有效的验证挑战"));
    }
    let solved = sdk::host(
        "crypto.deepseek.pow.solve",
        json!({ "challenge": challenge, "path": path }),
    )?;
    solved["response"]
        .as_str()
        .filter(|value| !value.is_empty() && value.len() <= 16_384)
        .map(str::to_string)
        .ok_or_else(|| Error::http(502, "主程序没有返回有效的 DeepSeek 验证证明"))
}
