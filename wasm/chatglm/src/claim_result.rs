//! 仅供签到/领取解析原响应；不发请求，不改变普通查询的兼容规则。

use crate::{errors, usage};
use agent2api_wasm_sdk::{Error, Result, Value};

const UNCONFIRMED: &str = "chatglm_claim_unconfirmed";

pub fn unconfirmed(detail: &str) -> Error {
    Error::http(502, format!("结果未确认：{detail}")).with_code(UNCONFIRMED)
}

pub fn rejected(error: &Error) -> bool {
    error.code.as_deref() == Some("chatglm_upstream")
        && (error.status == Some(401)
            || error
                .upstream_code
                .is_some_and(|code| !matches!(code, 0 | 200)))
}

pub fn action_error(mut error: Error, action: &str) -> Error {
    error.message = if rejected(&error) {
        format!("{action}失败：{}", error.message)
    } else {
        format!("{action}结果未确认：{}", error.message)
    };
    error
}

/// status 必须是实际 JSON 整数；不能把缺失、null、字符串或小数当作 0。
/// 0/200 的兼容范围由具体调用点指定；辅助错误字段不能推翻失败、制造成功。
pub fn envelope<'a>(raw: &'a Value, accepted: &[i64], token: &str) -> Result<&'a Value> {
    let status = raw["status"]
        .as_i64()
        .ok_or_else(|| unconfirmed("上游缺少整数业务状态"))?;
    if !accepted.contains(&status) {
        if !matches!(status, 0 | 200) {
            return Err(errors::upstream(200, raw, &[token]));
        }
        return Err(unconfirmed("上游业务状态不在该领取动作的已知成功范围内"));
    }
    for key in ["code", "error_code"] {
        if let Some(value) = raw.get(key).filter(|value| !value.is_null()) {
            let code = value
                .as_i64()
                .ok_or_else(|| unconfirmed("上游附加业务状态格式异常"))?;
            if !matches!(code, 0 | 200) {
                return Err(errors::upstream(200, raw, &[token]));
            }
        }
    }
    if raw["success"] == false
        || raw.get("error").is_some_and(|error| {
            !error.is_null() && error.as_str().is_none_or(|text| !text.trim().is_empty())
        })
    {
        return Err(unconfirmed("上游成功状态与失败标记冲突"));
    }
    // 登记任务与季节领取的成功确认可能没有 result；金额消费者仍须单独验字段。
    Ok(&raw["result"])
}

pub fn integer(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| {
            value
                .as_str()
                .and_then(|text| text.trim().parse::<i64>().ok())
        })
        .filter(|value| *value >= 0)
}

/// member-api 的单位为百分之一积分。负数、非有限值以及小于有效精度的正数均未知。
pub fn score(value: &Value) -> Result<f64> {
    let raw = usage::number(value)
        .filter(|value| *value >= 0.0)
        .ok_or_else(|| unconfirmed("上游缺少有效的非负奖励数值"))?;
    let score = usage::score(raw);
    if !score.is_finite() || raw > 0.0 && score <= 0.0 {
        return Err(unconfirmed("奖励数值超出已确认的积分精度"));
    }
    Ok(score)
}

pub fn failure(error: &Error) -> Value {
    agent2api_wasm_sdk::json!({
        "outcome": "failed", "success": false, "confirmed": rejected(error),
        "message": error.message, "points": null,
        "status": error.status, "upstreamCode": error.upstream_code
    })
}
