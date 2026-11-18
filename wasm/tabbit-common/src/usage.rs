//! 百分比额度，保留会员等级、周期、重置时刻和重置券；不把百分比伪装成积分。

use crate::{credentials, errors, network::Client, sign};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn integer(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| {
            value.as_f64().and_then(|value| {
                (value.is_finite()
                    && value.fract() == 0.0
                    && value >= i64::MIN as f64
                    && value < i64::MAX as f64)
                    .then_some(value as i64)
            })
        })
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
}

pub fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| {
            value.as_str().and_then(|value| {
                value
                    .trim()
                    .strip_suffix('%')
                    .unwrap_or(value.trim())
                    .parse()
                    .ok()
            })
        })
        .filter(|value| value.is_finite())
}

fn reset_text(remaining_ms: i64) -> String {
    let minutes = remaining_ms.max(0) / 60_000;
    if minutes >= 1440 {
        format!("{} 天 {} 小时后重置", minutes / 1440, minutes % 1440 / 60)
    } else if minutes >= 60 {
        format!("{} 小时 {} 分后重置", minutes / 60, minutes % 60)
    } else {
        format!("{minutes} 分钟后重置")
    }
}

fn fetch(client: &mut Client) -> Result<Value> {
    let uid = if client.credentials.uid.is_empty() {
        credentials::jwt_uid(&client.credentials.token)
    } else {
        client.credentials.uid.clone()
    };
    if uid.is_empty() {
        return Err(Error::http(
            400,
            "Tabbit 账号缺少 user_id，无法查询额度，请重新导入 Cookie",
        ));
    }
    let path = format!(
        "/api/commerce/quota/v1/usage?user_id={}&timezone={}",
        sign::escape(&uid),
        sign::escape(client.site.timezone)
    );
    client.signed_json("GET", &path, None, "/")
}

/// 签到的奖励口径必须来自本次原始 usage_percentage，不能取展示层的截断值或别名。
pub fn checkin_percentage(client: &mut Client) -> Result<String> {
    let value = fetch(client).map_err(|mut error| {
        error.message = format!("Tabbit 用量查询失败，未发送签到请求：{}", error.message);
        error
    })?;
    let invalid = || Error::http(502, "Tabbit 当前用量缺失或无效，未发送签到请求");
    let data = match value.get("data") {
        None => &value,
        Some(data) if data.is_object() => data,
        Some(_) => return Err(invalid()),
    };
    // HTTP 成功不掩盖明确的业务失败；不从失败信封里的旧读数拼签到参数。
    for response in [&value, data] {
        if response
            .get("success")
            .is_some_and(|value| value.as_bool() != Some(true))
            || response
                .get("code")
                .is_some_and(|value| value.as_i64() != Some(0) && value.as_str() != Some("0"))
            || ["error", "error_message", "detail"]
                .iter()
                .any(|key| response.get(*key).is_some_and(|value| !value.is_null()))
        {
            return Err(invalid());
        }
    }
    let used = match data.get("usage_percentage") {
        Some(Value::String(text)) => {
            let text = text.trim();
            let text = text.strip_suffix('%').unwrap_or(text);
            // 上游是十进制百分比字符串；多余百分号、非数值与下溢都不能变成零。
            if !text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.')
            {
                return Err(invalid());
            }
            let used = text.parse::<f64>().map_err(|_| invalid())?;
            if used == 0.0 && text.bytes().any(|byte| matches!(byte, b'1'..=b'9')) {
                return Err(invalid());
            }
            used
        }
        Some(value) => value.as_f64().ok_or_else(invalid)?,
        None => return Err(invalid()),
    };
    if !used.is_finite() || used.is_sign_negative() || !(0.0..=100.0).contains(&used) {
        return Err(invalid());
    }
    let percent = format!("{used:.2}%");
    if used > 0.0 && percent == "0.00%" {
        return Err(invalid());
    }
    Ok(percent)
}

pub fn query(client: &mut Client) -> Result<Value> {
    let value = fetch(client)?;
    let data = match value.get("data") {
        None => &value,
        Some(data) if data.is_object() => data,
        _ => return Err(Error::http(502, "Tabbit 额度响应结构无效，未生成余额")),
    };
    for response in [&value, data] {
        if response
            .get("success")
            .is_some_and(|value| value.as_bool() != Some(true))
            || response
                .get("code")
                .is_some_and(|value| value.as_i64() != Some(0) && value.as_str() != Some("0"))
            || ["error", "error_message", "detail"]
                .iter()
                .any(|key| response.get(*key).is_some_and(|value| !value.is_null()))
        {
            return Err(
                Error::http(502, "Tabbit 额度查询未返回成功结果，未采用响应中的旧读数")
                    .with_code("tabbit_quota_invalid_response"),
            );
        }
    }
    let used = number(data.get("usage_percentage").unwrap_or(&data["usage"]))
        .filter(|used| (0.0..=100.0).contains(used))
        .ok_or_else(|| {
            Error::http(
                502,
                "Tabbit 额度响应没有有效的 usage_percentage，未把异常读数当作余额为零",
            )
            .with_code("tabbit_quota_invalid_response")
        })?;
    let remaining = (100.0 - used).max(0.0);
    let plan = credentials::text(data, &["member_level", "plan", "plan_name"]);
    let interval = integer(&data["remaining_reset_minutes"])
        .filter(|value| *value > 0)
        .map(|value| value.saturating_mul(60_000))
        .or_else(|| {
            integer(&data["remaining_reset_hours"])
                .filter(|value| *value > 0)
                .map(|value| value.saturating_mul(3_600_000))
        });
    let now = crate::now()?;
    let reset_at = interval.map(|interval| now.saturating_add(interval));
    let coupons = data.get("unused_reset_coupon_count").and_then(|value| {
        number(value)
            .filter(|value| *value >= 0.0 && value.fract() == 0.0 && *value < i64::MAX as f64)
            .map(|value| value as i64)
    });
    let cycle_start = credentials::text(data, &["current_cycle_start", "currentCycleStart"]);
    let cycle_end = credentials::text(data, &["current_cycle_end", "currentCycleEnd"]);
    let number = format!("{remaining:.1}");
    let mut balance_view = format!("剩 {}%", number.trim_end_matches('0').trim_end_matches('.'));
    // 原生版将 Unix 时间误当剩余时长；这里仍返回相同绝对重置时刻，只纠正展示倒计时。
    if let Some(interval) = interval {
        balance_view.push_str(&format!(" · {}", reset_text(interval)));
    }
    let mut credit = json!({ "name": "剩余额度", "remain": remaining, "usedPercent": used, "percent": remaining });
    if !plan.is_empty() {
        credit["plan"] = json!(plan);
    }
    if let Some(reset_at) = reset_at {
        credit["expireAt"] = json!(reset_at);
        credit["resetsAt"] = json!(reset_at);
    }
    if let Some(coupons) = coupons {
        credit["resetCoupons"] = json!(coupons);
    }
    let mut window = json!({ "group": "额度", "label": "当前额度周期", "window": "当前周期", "remainingPercent": remaining });
    if let Some(reset_at) = reset_at {
        window["resetsAt"] = json!(reset_at);
    }
    if !cycle_start.is_empty() || !cycle_end.is_empty() {
        window["cycle"] = json!(format!("{cycle_start}–{cycle_end}"));
    }
    let report = json!({
        "available": remaining, "unit": "%",
        "wallets": [{ "type": "percent", "displayName": "剩余额度", "balance": remaining,
            "total": 100.0, "remainingPercent": remaining, "percent": remaining,
            "balanceView": balance_view, "planName": plan }],
        "credits": [credit], "windows": [window],
        "subscription": { "planName": plan, "isMember": !plan.is_empty() && !plan.eq_ignore_ascii_case("free"),
            "resetAt": reset_at, "cycleStart": cycle_start, "cycleEnd": cycle_end, "resetCoupons": coupons },
        "quotaSources": [{ "id": "account", "label": "账号主额度", "provider": client.site.id,
            "unit": "%", "remaining": remaining, "total": null, "used": null, "unlimited": false }],
        "raw": value
    });
    let report = errors::sanitize(&report, &client.credentials);
    client.current()?;
    Ok(report)
}
