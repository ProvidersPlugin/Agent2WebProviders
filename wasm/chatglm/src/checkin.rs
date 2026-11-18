//! 沿用已接活动/领取次序；持久在途记忆防止未知结果自动重发，不新增补发或回查。

use crate::{
    account::{self, Session},
    claim_result as result, endpoints, errors, memory, network, tasks,
};
use agent2api_wasm_sdk::{json, Result, Value};

fn report(outcome: &str, credit: Option<String>, message: &str) -> Value {
    json!({ "outcome": outcome, "credit": credit, "streak": null, "message": message })
}

fn daily(session: &Session) -> Result<Value> {
    memory::once(session, "daily-login", true, || daily_once(session))
}

fn daily_once(session: &Session) -> Result<Value> {
    session.check_current()?;
    let activity = network::json(
        "GET",
        endpoints::ACTIVITY_OPEN,
        &session.token,
        &session.device,
        None,
    )
    .and_then(|raw| result::envelope(&raw, &[0, 200], &session.token).cloned())
    .ok();
    let open = activity.as_ref().and_then(|value| {
        value
            .as_bool()
            .or_else(|| value["open"].as_bool())
            .or_else(|| value["is_open"].as_bool())
    });
    let raw = match session.raw_post(endpoints::DAILY, &json!({})) {
        Ok(value) => value,
        Err(error)
            if matches!(error.status, Some(400 | 409)) && error.upstream_code == Some(10001) =>
        {
            return Ok(report("done", None, "今日登录积分已领取过"));
        }
        Err(error) => return Err(result::action_error(error, "每日登录积分领取")),
    };
    // 10001 是旧协议明确记录的已领业务码；不再依据模糊错误文案判断。
    if raw["status"].as_i64() == Some(10001) {
        return Ok(report("done", None, "今日登录积分已领取过"));
    }
    let value = result::envelope(&raw, &[0, 200], &session.token)?;
    let score = result::score(value.get("score").unwrap_or(value))?;
    if score > 0.0 {
        Ok(report(
            "claimed",
            Some(format!("{score} 积分")),
            &format!("今日登录积分 +{score}"),
        ))
    } else if open == Some(true) {
        Ok(report("done", None, "今日登录积分已领取过"))
    } else if open == Some(false) {
        Ok(report("inactive", None, "积分活动未开启，无登录积分可领"))
    } else {
        Err(result::unconfirmed(
            "本次返回零积分，但活动状态未知，不能确认已领或无活动",
        ))
    }
}

fn bonus_entry(outcome: &str, message: &str, points: Value) -> Value {
    json!({ "outcome": outcome, "message": message, "points": points, "confirmed": true })
}

fn draw(session: &Session, raw: &Value) -> Result<Value> {
    let value = result::envelope(raw, &[0], &session.token)?;
    let gained = result::score(value.get("score").unwrap_or(&value["reward"]))?;
    Ok(if gained > 0.0 {
        bonus_entry("claimed", &format!("积分活动 +{gained}"), json!(gained))
    } else {
        // 确切的 0 只说明本次未入账，不推断已经领取或活动不存在。
        bonus_entry("inactive", "积分活动本次返回 0 积分，未新增奖励", json!(0))
    })
}

fn season(session: &Session) -> Result<Value> {
    session.check_current()?;
    let raw = network::json(
        "GET",
        endpoints::SEASON_STATUS,
        &session.token,
        &session.device,
        None,
    )
    .map_err(|error| result::action_error(error, "季节活动状态查询"))?;
    let value = result::envelope(&raw, &[0, 200], &session.token)?;
    session.check_current()?;
    let claimed = value["claimed"].as_bool();
    let claimable = value["claimable"].as_bool();
    if ["claimed", "claimable"]
        .iter()
        .any(|key| value.get(*key).is_some_and(|value| !value.is_boolean()))
    {
        return Err(result::unconfirmed(
            "季节活动的领取状态不是布尔值，未发送领取",
        ));
    }
    if claimed == Some(true) && claimable == Some(true) {
        return Err(result::unconfirmed(
            "季节活动的已领与可领状态冲突，未发送领取",
        ));
    }
    if claimed == Some(true) {
        return Ok(bonus_entry("done", "季节活动此前已领取", Value::Null));
    }
    // 只有明确可领才能发送；仅 claimed=false 不能证明当前有活动可领。
    if claimable == Some(true) {
        memory::once(session, "season-summer", true, || {
            let raw = session
                .raw_post(endpoints::SEASON_CLAIM, &json!({}))
                .map_err(|error| result::action_error(error, "季节活动领取"))?;
            result::envelope(&raw, &[0], &session.token)?;
            Ok(bonus_entry(
                "claimed",
                "季节活动已领取（上游业务状态确认，奖励金额未知）",
                Value::Null,
            ))
        })
    } else if claimable == Some(false) {
        Ok(bonus_entry("inactive", "季节活动当前不可领取", Value::Null))
    } else {
        Err(result::unconfirmed(
            "季节活动未返回明确可领状态，未发送领取",
        ))
    }
}

pub fn bonus(session: &Session) -> Result<Value> {
    if session.guest {
        return Ok(json!({ "success": false, "msg": "访客账号没有积分体系，跳过活动领取" }));
    }
    let mut entries = Vec::new();
    let first = memory::once(session, "score-draw", true, || {
        let raw = session.raw_post(endpoints::ACTIVITY_DRAW, &json!({}))?;
        draw(session, &raw)
    })
    .map_err(|error| result::action_error(error, "积分活动领取"));
    let stop = first.as_ref().err().is_some_and(|error| {
        error.status == Some(401)
            || error.code.as_deref() == Some("chatglm_account_conflict")
            || !result::rejected(error)
    });
    entries.push(match first {
        Ok(value) => value,
        Err(error) => result::failure(&error),
    });
    if !stop {
        entries.push(match season(session) {
            Ok(value) => value,
            Err(error) => result::failure(&error),
        });
    }
    session.check_current()?;
    let claimed = entries
        .iter()
        .filter(|entry| entry["outcome"] == "claimed")
        .count();
    let already = entries
        .iter()
        .filter(|entry| entry["outcome"] == "done")
        .count();
    let failed = entries
        .iter()
        .filter(|entry| entry["outcome"] == "failed")
        .count();
    let unconfirmed = entries
        .iter()
        .filter(|entry| entry["confirmed"] == false)
        .count();
    let messages = entries
        .iter()
        .filter_map(|entry| entry["message"].as_str())
        .collect::<Vec<_>>();
    Ok(session.sanitize(&json!({
        "success": failed == 0 && claimed + already > 0,
        "msg": messages.join("；"), "claimed": claimed, "already": already,
        "failed": failed, "unconfirmed": unconfirmed, "details": entries
    })))
}

pub fn claim() -> Result<Value> {
    let mut session = account::ready_locked()?;
    if session.guest {
        return Ok(report(
            "inactive",
            None,
            "访客账号没有积分体系，跳过（仅登录账号可领取）",
        ));
    }
    // 保留原有、仅明确 401 触发的一次续期路径；结果未知不会进入重试。
    let result = match daily(&session) {
        Err(error) if error.status == Some(401) => {
            session = account::refresh_locked(&session)?;
            daily(&session)
        }
        other => other,
    };
    let mut result = match result {
        Ok(value) => value,
        Err(error) => {
            // 原主步骤失败即结束；回成兼容 failed，不新增后续活动或任务请求。
            let mut report = report("failed", None, &error.message);
            report["details"] = json!({ "login": result::failure(&error) });
            return Ok(session.sanitize(&report));
        }
    };
    let original = result.clone();
    // 与原生版顺序一致：登录积分 → 会员/季节活动 → 每日任务。
    let bonus = bonus(&session);
    let daily = tasks::daily(&session);
    let mut details = json!({ "login": original });
    let mut notes = Vec::new();
    let mut incomplete = false;
    match bonus {
        Ok(value) => {
            incomplete |= value["failed"].as_u64().is_some_and(|count| count > 0)
                || value["unconfirmed"].as_u64().is_some_and(|count| count > 0);
            notes.push(format!("活动领取：{}", errors::text(&value, &["msg"])));
            details["bonus"] = value;
        }
        Err(error) => {
            incomplete = true;
            notes.push(format!("活动领取结果未确认：{}", error.message));
            details["bonus"] = result::failure(&error);
        }
    }
    match daily {
        Ok(value) => {
            incomplete |= value["success"] != true;
            if let Some(gained) = value["confirmedPoints"].as_i64().filter(|value| *value > 0) {
                let previous = result["credit"].as_str().unwrap_or("");
                result["credit"] = json!(if previous.is_empty() {
                    format!("每日任务已确认 +{gained} 积分")
                } else {
                    format!("{previous}（每日任务已确认另 +{gained} 积分）")
                });
            }
            notes.push(format!("每日任务：{}", errors::text(&value, &["msg"])));
            details["dailyTasks"] = value;
        }
        Err(error) => {
            incomplete = true;
            notes.push(format!("每日任务结果未确认：{}", error.message));
            details["dailyTasks"] = result::failure(&error);
        }
    }
    let message = result["message"].as_str().unwrap_or("").to_string();
    result["message"] = json!(format!("{message}（{}）", notes.join("；")));
    if incomplete {
        // 公共 CheckinReport 只认这些既有枚举，不能用额外 success 字段掩盖 claimed。
        result["outcome"] = json!("failed");
    }
    result["details"] = details;
    session.check_current()?;
    Ok(session.sanitize(&result))
}
