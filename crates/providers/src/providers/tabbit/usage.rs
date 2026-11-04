//! Tabbit 额度查询（百分比制）。
//!
//! 上游 `GET /api/commerce/quota/v1/usage?user_id=…&timezone=…` 只回
//! `usage_percentage`（已用百分比）与 `member_level`、`remaining_reset_minutes`、
//! `unused_reset_coupon_count`、`current_cycle_start/end` 等字段 —— **没有**
//! 积分包或金额。所以余额列显示的是「剩 X%」，不是「剩 N 积分」。
//!
//! 只按 `user_id` 查（不带 Cookie 也能读），但这里仍走带 Cookie 的签名请求，
//! 与其它控制面一致。缺 `user_id` 时上游直接 422，本地先给可读错误。

use serde_json::{json, Value};

use crate::errors::GatewayError;

use super::client::{self, Credentials};
use super::config::Realm;

fn reset_text(reset_at: Option<i64>) -> String {
    let Some(ms) = reset_at else {
        return String::new();
    };
    let total_seconds = ms / 1000;
    let days = total_seconds / 86400;
    let hours = (total_seconds % 86400) / 3600;
    let minutes = (total_seconds % 3600) / 60;
    if days > 0 {
        format!("{days} 天 {hours} 小时后重置")
    } else if hours > 0 {
        format!("{hours} 小时 {minutes} 分后重置")
    } else {
        format!("{minutes} 分钟后重置")
    }
}

pub async fn query(
    creds: &Credentials,
    realm: Realm,
    client: &reqwest::Client,
) -> Result<Value, GatewayError> {
    let uid = if creds.uid.is_empty() {
        crate::crypto::decode_jwt_claims(&creds.token)
            .as_ref()
            .and_then(|claims| {
                ["user_id", "uid", "sub"]
                    .iter()
                    .find_map(|key| claims.get(*key).and_then(Value::as_str))
            })
            .unwrap_or("")
            .to_string()
    } else {
        creds.uid.clone()
    };
    if uid.is_empty() {
        return Err(GatewayError::with_status(
            400,
            "Tabbit 账号记录缺少 user_id，无法查询额度（请重新导入 Cookie）",
        ));
    }
    let path = format!(
        "/api/commerce/quota/v1/usage?user_id={}&timezone={}",
        url_encode(&uid),
        url_encode(client::timezone(realm))
    );
    let value = client::signed_json(creds, realm, client, "GET", &path, None, "/").await?;
    let data = value.get("data").filter(|data| data.is_object()).unwrap_or(&value);
    let used = ["usage_percentage", "usage"]
        .iter()
        .find_map(|key| data.get(*key).and_then(client::as_f64))
        .ok_or_else(|| {
            GatewayError::with_status(502, "Tabbit 额度响应里没有 usage_percentage")
        })?;
    let used = used.clamp(0.0, 100.0);
    let remain = (100.0 - used).max(0.0);
    let member_level = ["member_level", "plan", "plan_name"]
        .iter()
        .filter_map(|key| data.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|text| !text.is_empty())
        .unwrap_or("")
        .to_string();
    let reset_at = if let Some(minutes) = data
        .get("remaining_reset_minutes")
        .and_then(client::as_i64)
        .filter(|minutes| *minutes > 0)
    {
        Some(crate::http::now_ms() + minutes * 60 * 1000)
    } else if let Some(hours) = data
        .get("remaining_reset_hours")
        .and_then(client::as_i64)
        .filter(|hours| *hours > 0)
    {
        Some(crate::http::now_ms() + hours * 60 * 60 * 1000)
    } else {
        None
    };
    let coupons = data
        .get("unused_reset_coupon_count")
        .and_then(client::as_i64)
        .unwrap_or(0);
    let cycle_start = data
        .get("current_cycle_start")
        .or_else(|| data.get("currentCycleStart"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let cycle_end = data
        .get("current_cycle_end")
        .or_else(|| data.get("currentCycleEnd"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let reset = reset_text(reset_at);
    let balance_view = if reset.is_empty() {
        format!("剩 {}%", round1(remain))
    } else {
        format!("剩 {}% · {reset}", round1(remain))
    };
    let is_member = !member_level.is_empty() && !member_level.eq_ignore_ascii_case("free");
    let mut credits_row = json!({
        "name": "剩余额度",
        "remain": remain,
        "usedPercent": used,
        "percent": remain,
    });
    if !member_level.is_empty() {
        credits_row["plan"] = Value::String(member_level.clone());
    }
    if let Some(reset_at) = reset_at {
        credits_row["expireAt"] = Value::from(reset_at);
        credits_row["resetsAt"] = Value::from(reset_at);
    }
    if coupons > 0 {
        credits_row["resetCoupons"] = Value::from(coupons);
    }
    let mut windows_row = json!({
        "group": "额度",
        "label": "当前额度周期",
        "window": "当前周期",
        "remainingPercent": remain,
    });
    if let Some(reset_at) = reset_at {
        windows_row["resetsAt"] = Value::from(reset_at);
    }
    if !cycle_start.is_empty() || !cycle_end.is_empty() {
        windows_row["cycle"] = Value::String(format!("{cycle_start}–{cycle_end}"));
    }
    let report = json!({
        "available": remain,
        "unit": "%",
        "wallets": [{
            "type": "percent",
            "displayName": "剩余额度",
            "balance": remain,
            "total": 100.0,
            "remainingPercent": remain,
            "percent": remain,
            "balanceView": balance_view,
            "planName": member_level,
        }],
        "credits": [credits_row],
        "windows": [windows_row],
        "subscription": {
            "planName": member_level,
            "isMember": is_member,
            "resetAt": reset_at,
            "cycleStart": cycle_start,
            "cycleEnd": cycle_end,
            "resetCoupons": coupons,
        },
        "raw": value,
    });
    Ok(report)
}

fn round1(value: f64) -> String {
    let text = format!("{value:.1}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b':' | b'/') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}
