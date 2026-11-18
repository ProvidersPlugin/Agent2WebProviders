//! 余额只认 member_info.left_score / 100；任务与流水本来就是积分，不重复除100。

use crate::{account::Session, endpoints, errors};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        .filter(|value| value.is_finite())
}
fn pick(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| number(&value[*key]))
}
pub fn score(raw: f64) -> f64 {
    if raw.is_finite() {
        raw.round() / 100.0
    } else {
        0.0
    }
}

fn epoch_ms(value: f64) -> i64 {
    if value > 0.0 && value < 100_000_000_000.0 {
        (value * 1000.0) as i64
    } else {
        value.max(0.0) as i64
    }
}

fn record(entry: &Value) -> Option<Value> {
    let amount = number(&entry["score"])?;
    let title = errors::text(entry, &["title"]);
    let mut row = json!({ "name": if title.is_empty() { "积分变动" } else { &title }, "record": true, "amount": amount, "unit": "积分" });
    if let Some(time) = number(&entry["created_at"]).filter(|time| *time > 0.0) {
        row["at"] = json!((time * 1000.0) as i64);
    }
    if let Some(time) = number(&entry["expired_at"]).filter(|time| *time > 0.0) {
        row["expireAt"] = json!((time * 1000.0) as i64);
    }
    let remark = errors::text(entry, &["remark"]);
    if !remark.is_empty() {
        row["remark"] = json!(remark);
    }
    Some(row)
}

pub fn query(session: &Session) -> Result<Value> {
    if session.guest {
        return Err(Error::http(
            400,
            "智谱清言访客没有积分体系，只能对话；添加登录账号后可查询积分和签到",
        )
        .with_code("usage_not_configured"));
    }
    let member = session.get(endpoints::MEMBER)?;
    let raw = pick(&member, &["left_score", "true_left_score"]);
    // 缺数值保留为空，不把上游未返回误报成零余额。
    let current = raw.map(score);
    let mut wallets = vec![json!({ "type": "score", "displayName": "积分", "balance": current })];
    let percent = errors::text(&member, &["token_left_percent"]);
    if !percent.is_empty() {
        wallets
            .push(json!({ "type": "token_quota", "displayName": "令牌额度", "percent": percent }));
    }
    let mut credits = vec![json!({ "name": "积分", "remain": current })];
    let rule = errors::text(&member, &["score_rule"]);
    if !rule.is_empty() {
        credits[0]["rule"] = json!(rule);
    }
    let activity = session.get(endpoints::ACTIVITY).ok();
    if let Some(activity) = &activity {
        if let Some(status) = activity["status"]
            .as_i64()
            .filter(|status| (1..=4).contains(status))
        {
            let mut row = json!({ "name": "会员积分活动", "remain": number(&activity["current_score"]), "status": status });
            if let Some(target) = number(&activity["target_score"]) {
                row["target"] = json!(target);
            }
            if let Some(days) = number(&activity["remain_days"]) {
                row["remainDays"] = json!(days);
            }
            credits.push(row);
        }
    }
    let records = session.get(endpoints::RECORDS).ok();
    let rows: Vec<Value> = records
        .as_ref()
        .and_then(|value| value["list"].as_array())
        .into_iter()
        .flatten()
        .filter_map(record)
        .collect();
    credits.extend(rows.iter().cloned());
    let level = errors::text(&member, &["level"]);
    if !level.is_empty() {
        credits[0]["plan"] = json!(level);
    }
    let vip = pick(&member, &["vip_expired_at", "member_expiration_time"]).map(epoch_ms);
    let svip = pick(&member, &["svip_expired_at"]).map(epoch_ms);
    let is_member = member["is_member"].as_bool().unwrap_or(false);
    let mut result = json!({ "available": current, "unit": "积分", "wallets": wallets, "credits": credits,
        "raw": { "member": member, "activity": activity, "leftScoreRaw": raw, "leftTokenRaw": number(&member["left_token"]),
            "recordsReturned": rows.len(), "recordsHasMore": records.as_ref().is_some_and(|value| value["has_more"] == true) } });
    if is_member || !level.is_empty() || vip.is_some() || svip.is_some() {
        result["subscription"] = json!({ "planName": level, "isMember": is_member, "vipExpireAt": vip, "svipExpireAt": svip });
    }
    result["quotaSources"] = match current {
        Some(current) => {
            json!([{ "id": "account", "label": "账号主额度", "provider": "chatglm", "unit": "积分",
            "remaining": current, "total": null, "used": null, "unlimited": false }])
        }
        None => json!([]),
    };
    session.check_current()?;
    Ok(session.sanitize(&result))
}
