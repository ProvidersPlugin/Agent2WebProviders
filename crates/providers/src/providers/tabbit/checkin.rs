//! Tabbit 每日签到（`daily_sign_in`）。
//!
//! 上游是真实运营功能：`GET /api/commerce/activity/v1/sign-in/status` 查状态，
//! `POST /api/commerce/activity/v1/sign-in` 签到。返回按 `scene_code` 分组：
//! `daily_sign_in` 是每日签到，`desktop_pet` 是养宠（本适配只关心前者）。
//!
//! `activity_open=false` 时如实返回「今日无活动」，不去空打一次签到接口 ——
//! 一个必然失败的复选框比没有更糟。

use agent2api_provider_protocol as proto;
use serde_json::{json, Value};

use crate::errors::GatewayError;

use super::client::{self, Credentials};
use super::config::Realm;

fn bool_of(value: &Value) -> bool {
    match value {
        Value::Bool(flag) => *flag,
        Value::String(text) => text.eq_ignore_ascii_case("true") || text == "1",
        Value::Number(number) => number.as_i64().unwrap_or(0) != 0,
        _ => false,
    }
}

fn daily_entry(value: &Value) -> Option<&Value> {
    value
        .get("results")
        .and_then(Value::as_array)
        .and_then(|results| {
            results.iter().find(|entry| {
                entry.get("scene_code").and_then(Value::as_str) == Some("daily_sign_in")
            })
        })
}

fn report(
    outcome: proto::CheckinOutcome,
    credit: Option<String>,
    message: Option<String>,
) -> proto::CheckinReport {
    let mut report = proto::CheckinReport::new(outcome);
    report.credit = credit;
    report.message = message;
    report
}

pub async fn claim(
    creds: &Credentials,
    realm: Realm,
    client: &reqwest::Client,
) -> Result<proto::CheckinReport, GatewayError> {
    let status = client::signed_json(
        creds,
        realm,
        client,
        "GET",
        "/api/commerce/activity/v1/sign-in/status?scene_codes=daily_sign_in&scene_codes=desktop_pet",
        None,
        "/member/usage",
    )
    .await?;
    if let Some(entry) = daily_entry(&status) {
        if !bool_of(entry.get("activity_open").unwrap_or(&Value::Bool(true))) {
            return Ok(report(
                proto::CheckinOutcome::Inactive,
                None,
                Some("Tabbit 今日签到活动未开启".to_string()),
            ));
        }
        if bool_of(entry.get("signed_today").unwrap_or(&Value::Bool(false))) {
            return Ok(report(
                proto::CheckinOutcome::Done,
                None,
                Some("今日已签到".to_string()),
            ));
        }
    }

    // 上游把当前已用百分比一起收下（奖励口径）。查不到就按 0.00% 发。
    let percent = match super::usage::query(creds, realm, client).await {
        Ok(report) => report
            .get("credits")
            .and_then(Value::as_array)
            .and_then(|rows| rows.first())
            .and_then(|row| row.get("usedPercent"))
            .and_then(client::as_f64)
            .map(|value| format!("{value:.2}%"))
            .unwrap_or_else(|| "0.00%".to_string()),
        Err(_) => "0.00%".to_string(),
    };
    let body = json!({
        "request_no": super::sign::random_uuid(),
        "scene_codes": ["daily_sign_in"],
        "usage_percentage": percent,
    });
    let value = client::signed_json(
        creds,
        realm,
        client,
        "POST",
        "/api/commerce/activity/v1/sign-in",
        Some(body),
        "/member/usage",
    )
    .await?;
    let Some(entry) = daily_entry(&value) else {
        return Ok(report(
            proto::CheckinOutcome::Inactive,
            None,
            Some("上游未返回每日签到的结果".to_string()),
        ));
    };
    let result = entry
        .get("sign_in_result")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    match result.as_str() {
        "success" => {
            let credit = entry
                .get("usage_reward_result")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty() && !text.eq_ignore_ascii_case("skipped"))
                .map(str::to_string);
            Ok(report(
                proto::CheckinOutcome::Claimed,
                credit,
                Some("Tabbit 签到成功".to_string()),
            ))
        }
        "already_signed" => Ok(report(
            proto::CheckinOutcome::Done,
            None,
            Some("今日已签到".to_string()),
        )),
        "" => Ok(report(
            proto::CheckinOutcome::Inactive,
            None,
            Some("上游没有给出明确的签到结论".to_string()),
        )),
        other => Ok(report(
            proto::CheckinOutcome::Failed,
            None,
            Some(format!("签到未成功：{other}")),
        )),
    }
}
