//! 使用当前真实每日活动；没有活动与已签到都返回中性结果，不虚构领取成功。

use crate::{checkin_state::Attempt, errors, network::Client, usage};
use agent2api_wasm_sdk::{json, Error, Result, Value};

fn flag(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(value) => Some(*value),
        Value::String(value) if value.eq_ignore_ascii_case("true") || value == "1" => Some(true),
        Value::String(value) if value.eq_ignore_ascii_case("false") || value == "0" => Some(false),
        Value::Number(value) => match value.as_i64() {
            Some(1) => Some(true),
            Some(0) => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn entry<'a>(value: &'a Value, scene: &str) -> Result<Option<&'a Value>> {
    let entries = value["results"]
        .as_array()
        .ok_or_else(|| Error::http(502, "Tabbit 签到状态缺少 results，未发送签到请求"))?;
    if entries.len() > 16 || entries.iter().any(|entry| !entry.is_object()) {
        return Err(Error::http(502, "Tabbit 签到活动列表格式无效"));
    }
    let mut entries = entries.iter().filter(|entry| entry["scene_code"] == scene);
    let current = entries.next();
    if entries.next().is_some() {
        return Err(Error::http(502, "Tabbit 同一签到活动返回冲突状态"));
    }
    Ok(current)
}

fn report(outcome: &str, credit: Option<String>, message: impl Into<String>) -> Value {
    json!({ "outcome": outcome, "credit": credit, "streak": null, "message": message.into() })
}

fn unconfirmed() -> Value {
    report("failed", None, "Tabbit 签到结果未确认，本次未重复提交")
}

pub fn claim(client: &mut Client) -> Result<Value> {
    let path = if client.site.id == "tabbit" {
        "/api/commerce/activity/v1/sign-in/status?scene_codes=desktop_pet"
    } else {
        "/api/commerce/activity/v1/sign-in/status?scene_codes=daily_sign_in&scene_codes=desktop_pet"
    };
    let status = client.signed_json("GET", path, None, "/member/usage")?;
    let (scene, current) = match entry(
        &status,
        if client.site.id == "tabbit" {
            "desktop_pet"
        } else {
            "daily_sign_in"
        },
    )? {
        Some(current) => (
            if client.site.id == "tabbit" {
                "desktop_pet"
            } else {
                "daily_sign_in"
            },
            current,
        ),
        None => {
            return Ok(report(
                "inactive",
                None,
                "Tabbit 当前没有开放的每日签到活动",
            ));
        }
    };
    if flag(&current["activity_open"]) == Some(false) {
        return Ok(report("inactive", None, "Tabbit 今日签到活动未开启"));
    }
    if flag(&current["signed_today"]) == Some(true) {
        return Ok(report("done", None, "今日已签到"));
    }
    if flag(&current["activity_open"]) != Some(true)
        || flag(&current["signed_today"]) != Some(false)
    {
        return Err(Error::http(502, "Tabbit 签到状态未确认，未发送签到请求"));
    }
    // 原daily场景才接受真实用量奖励字段；desktop_pet沿已确认协议仅发送活动和幂等号。
    let percent = if scene == "daily_sign_in" {
        Some(usage::checkin_percentage(client)?)
    } else {
        None
    };
    let Some(mut attempt) = Attempt::prepare(client, &status, scene)? else {
        return Ok(unconfirmed());
    };
    let mut body = json!({ "request_no": attempt.request_no(), "scene_codes": [scene] });
    if let Some(percent) = percent {
        body["usage_percentage"] = json!(percent);
    }
    let result = client.signed_json(
        "POST",
        "/api/commerce/activity/v1/sign-in",
        Some(&body),
        "/member/usage",
    )?;
    let Some(entry) = entry(&result, scene)? else {
        return Ok(unconfirmed());
    };
    let outcome = entry["sign_in_result"].as_str().unwrap_or("").trim();
    let result = match outcome {
        "success" | "signed" | "signed_in" => {
            let credit = entry["usage_reward_result"]
                .as_str()
                .filter(|text| !text.is_empty() && !text.eq_ignore_ascii_case("skipped"))
                .map(str::to_string);
            report("claimed", credit, "Tabbit 签到成功")
        }
        "already_signed" => report("done", None, "今日已签到"),
        _ => unconfirmed(),
    };
    if matches!(
        outcome,
        "success" | "signed" | "signed_in" | "already_signed"
    ) {
        attempt.finish(client, outcome)?;
    }
    client.current()?;
    Ok(errors::sanitize(&result, &client.credentials))
}
