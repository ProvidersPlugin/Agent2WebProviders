//! 原顺序 center → click → complete；新手任务仅有末尾原定的一次 center 回查。
//! 只按明确业务确认累计结果，未知金额/状态不补零，不据本地推算结清。

use crate::{
    account::{self, Session},
    claim_result as result, credentials, endpoints, errors, memory, network, sign, usage,
};
use agent2api_wasm_sdk::{json, Result, Value};

fn category(center: &Value, code: &str) -> Result<Value> {
    let categories = center["categories"]
        .as_array()
        .ok_or_else(|| result::unconfirmed("任务中心缺少有效分类清单"))?;
    let mut selected = categories
        .iter()
        .filter(|category| category["code"] == code);
    let category = selected
        .next()
        .ok_or_else(|| result::unconfirmed("任务中心未返回所需分类，不能当作没有任务"))?;
    if selected.next().is_some() || !category["tasks"].is_array() {
        return Err(result::unconfirmed("任务分类重复或缺少有效任务清单"));
    }
    Ok(category.clone())
}

fn tasks(category: &Value) -> &[Value] {
    category["tasks"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn task_id(task: &Value) -> Option<&str> {
    task["task_id"]
        .as_str()
        .filter(|id| !id.trim().is_empty() && id.len() <= 512 && !id.chars().any(char::is_control))
        .map(str::trim)
}

fn valid_ids(category: &Value) -> bool {
    let mut ids = std::collections::BTreeSet::new();
    tasks(category)
        .iter()
        .all(|task| task_id(task).is_some_and(|id| ids.insert(id)))
}

fn done(task: &Value) -> Option<bool> {
    // 旧协议明确只有 1 未开始 / 2 已点击 / 3 已完成，不把未知大状态码当完成。
    match task["status"].as_i64() {
        Some(1 | 2) => Some(false),
        Some(3) => Some(true),
        _ => None,
    }
}

fn points(task: &Value) -> Option<i64> {
    result::integer(&task["reward"]["points"])
}

fn balance(center: &Value) -> Value {
    usage::number(&center["user_info"]["points_balance"])
        .filter(|value| *value >= 0.0)
        .map(Value::from)
        .unwrap_or(Value::Null)
}

/// 只在本领取模块严格解析；不改变余额、目录、登录等普通 GET 的兼容行为。
fn center(session: &Session) -> Result<Value> {
    session.check_current()?;
    let raw = network::json(
        "GET",
        endpoints::TASK_CENTER,
        &session.token,
        &session.device,
        None,
    )?;
    session.check_current()?;
    result::envelope(&raw, &[0, 200], &session.token).cloned()
}

fn guest() -> Value {
    json!({ "tasks": [], "earned": 0, "total": 0, "unclaimed": 0, "settled": false,
        "note": "访客账号没有任务中心，仅登录账号可参与" })
}

fn payload(category: &Value) -> Value {
    let tasks = tasks(category);
    let mut earned = Some(0_i64);
    let mut total = Some(0_i64);
    let mut unknown = 0usize;
    let mut unclaimed = Some(0usize);
    let ids_confirmed = valid_ids(category);
    let mut rows = Vec::new();
    for task in tasks {
        let id = errors::text(task, &["task_id"]);
        let done = done(task);
        let points = points(task);
        if done.is_none() || points.is_none() {
            unknown += 1;
        }
        total = total.and_then(|sum| points.and_then(|points| sum.checked_add(points)));
        earned = match done {
            Some(true) => earned.and_then(|sum| points.and_then(|points| sum.checked_add(points))),
            Some(false) => earned,
            None => None,
        };
        unclaimed = match done {
            Some(false) => unclaimed.map(|count| count + 1),
            Some(true) => unclaimed,
            None => None,
        };
        rows.push(json!({ "key": id,
            "title": errors::text(task, &["title"]), "group": category["name"].as_str().unwrap_or("新手任务"),
            "points": points, "done": done }));
    }
    let confirmed = unknown == 0 && ids_confirmed && total.is_some() && earned.is_some();
    let mut value = json!({ "tasks": rows, "earned": earned, "total": total, "unclaimed": unclaimed,
        "settled": confirmed && unclaimed == Some(0) && !tasks.is_empty(),
        "statusConfirmed": confirmed });
    if !confirmed {
        value["note"] = json!("任务状态结果未确认：部分状态或奖励金额缺失、异常，未结清");
    }
    value
}

fn saved(session: &Session) -> Result<Option<Value>> {
    // 记忆边界先验容器、身份与全部旧字段，任何坏值都不能折成 None 继续发送。
    session.check_current()?;
    memory::onboarding(&session.uid)
}

fn served(snapshot: &Value) -> Value {
    json!({ "tasks": snapshot["tasks"], "earned": snapshot["earned"], "total": snapshot["total"],
        "unclaimed": 0, "settled": true, "settledAt": snapshot["at"], "statusConfirmed": true })
}

fn append_note(payload: &mut Value, note: &str) {
    let previous = payload["note"].as_str().unwrap_or("");
    payload["note"] = json!(if previous.is_empty() {
        note.to_string()
    } else {
        format!("{previous}；{note}")
    });
}

fn remember(session: &Session, payload: &mut Value, previous: Option<&Value>) -> Result<()> {
    session.check_current()?;
    if payload["statusConfirmed"] != true || !payload["settled"].is_boolean() {
        return Err(result::unconfirmed(
            "当前任务结算状态未知，保留原记忆，未自动清空",
        ));
    }
    if payload["settled"] == false {
        return memory::save_onboarding(&session.uid, Value::Null);
    }
    let at = match previous.filter(|previous| previous["tasks"] == payload["tasks"]) {
        Some(value) => value["at"].as_i64().unwrap_or(sign::now()?),
        None => sign::now()?,
    };
    let snapshot = json!({ "at": at, "tasks": payload["tasks"], "earned": payload["earned"], "total": payload["total"] });
    memory::save_onboarding(&session.uid, snapshot)?;
    payload["settledAt"] = json!(at);
    Ok(())
}

pub fn status(session: &Session, force: bool) -> Result<Value> {
    if session.guest {
        return Ok(guest());
    }
    let previous = saved(session)?;
    if !force {
        if let Some(previous) = previous.as_ref() {
            return Ok(served(previous));
        }
    }
    let refreshed = if credentials::expiring(&session.secret) {
        Some(account::refresh_locked(session)?)
    } else {
        None
    };
    let session = refreshed.as_ref().unwrap_or(session);
    let center = center(session)?;
    let mut result = payload(&category(&center, "newbie")?);
    result["balance"] = balance(&center);
    if let Err(error) = remember(session, &mut result, previous.as_ref()) {
        append_note(
            &mut result,
            &format!("任务状态已取得，但结算记忆保存失败：{}", error.message),
        );
    }
    Ok(session.sanitize(&result))
}

fn settle(session: &Session, task: &Value, category: &Value) -> Result<i64> {
    let id =
        task_id(task).ok_or_else(|| result::unconfirmed("智谱任务缺少有效 task_id，未发送领取"))?;
    let body = json!({ "task_id": id });
    let daily = category["code"] == "daily";
    let completed = memory::once(
        session,
        &format!("{}:{id}", if daily { "daily-task" } else { "newbie-task" }),
        daily,
        || {
            let clicked = session
                .raw_post(endpoints::TASK_CLICK, &body)
                .map_err(|error| {
                    let mut error = result::action_error(error, "任务登记");
                    if result::rejected(&error) {
                        error.code = Some("chatglm_action_rejected".into());
                    }
                    error
                })?;
            result::envelope(&clicked, &[0, 200], &session.token).map_err(|mut error| {
                if result::rejected(&error) {
                    error.code = Some("chatglm_action_rejected".into());
                }
                error
            })?;
            let completed =
                session
                    .raw_post(endpoints::TASK_COMPLETE, &body)
                    .map_err(|mut error| {
                        error.code = Some("chatglm_claim_unconfirmed".into());
                        result::action_error(error, "任务结算")
                    })?;
            let value =
                result::envelope(&completed, &[0, 200], &session.token).map_err(|mut error| {
                    error.code = Some("chatglm_claim_unconfirmed".into());
                    error
                })?;
            let points = result::integer(&value["reward"]["points_awarded"]).ok_or_else(|| {
                result::unconfirmed("任务结算未返回有效的非负整分奖励，不能确认到账或已领")
            })?;
            Ok(json!(points))
        },
    )?;
    result::integer(&completed).ok_or_else(|| result::unconfirmed("已确认任务记忆缺少有效奖励数值"))
}

struct Batch {
    rows: Vec<Value>,
    notes: Vec<String>,
    claimed: usize,
    already: usize,
    failed: usize,
    unconfirmed: usize,
    confirmed_points: Option<i64>,
    stopped: bool,
    invalid_identity: bool,
}

impl Batch {
    fn new() -> Self {
        Self {
            rows: Vec::new(),
            notes: Vec::new(),
            claimed: 0,
            already: 0,
            failed: 0,
            unconfirmed: 0,
            confirmed_points: Some(0),
            stopped: false,
            invalid_identity: false,
        }
    }

    fn success(&self) -> bool {
        self.failed == 0 && self.confirmed_points.is_some() && !self.stopped
    }

    fn fields(&self, value: &mut Value, amount: &str) {
        value["results"] = json!(self.rows);
        value["claimed"] = json!(self.claimed);
        value["already"] = json!(self.already);
        // failed 保持包含未确认的兼容计数，另用 unconfirmed 精确区分。
        value["failed"] = json!(self.failed);
        value["unconfirmed"] = json!(self.unconfirmed);
        value["confirmedPoints"] = json!(self.confirmed_points);
        value[amount] = json!(if self.unconfirmed == 0 {
            self.confirmed_points
        } else {
            None
        });
        value["pointsComplete"] = json!(self.unconfirmed == 0 && self.confirmed_points.is_some());
        value["stopped"] = json!(self.stopped || self.invalid_identity);
    }

    fn message(&self, empty: &str) -> String {
        let mut notes = self.notes.clone();
        if self.confirmed_points.is_none() {
            notes.push("奖励汇总结果未确认：数值超出范围，逐项已确认结果仍保留".into());
        }
        if self.stopped {
            notes.push("账号授权已失效，已停止后续任务；此前确认结果仍保留，未自动重发".into());
        }
        if notes.is_empty() {
            empty.into()
        } else {
            notes.join("；")
        }
    }
}

fn run(session: &Session, category: &Value) -> Batch {
    let mut batch = Batch::new();
    if !valid_ids(category) {
        batch.failed = 1;
        batch.unconfirmed = 1;
        batch.invalid_identity = true;
        batch
            .notes
            .push("结果未确认：任务标识缺失、过长、含控制字符或重复；本批未发送任何领取".into());
        return batch;
    }
    for task in tasks(category) {
        let id = errors::text(task, &["task_id"]);
        let title = errors::text(task, &["title"]);
        let result = match done(task) {
            Some(true) if points(task).is_some() => {
                batch.already += 1;
                batch
                    .rows
                    .push(json!({ "key": id, "ok": true, "already": true,
                    "points": 0, "outcome": "done", "confirmed": true, "source": "center" }));
                continue;
            }
            Some(true) => Err(result::unconfirmed(
                "任务标记已完成，但奖励金额异常；未再次发送领取",
            )),
            Some(false) => settle(session, task, category),
            None => Err(result::unconfirmed("任务状态缺失或未知，未发送领取")),
        };
        match result {
            Ok(points) => {
                let already = points == 0;
                if already {
                    batch.already += 1;
                } else {
                    batch.claimed += 1;
                }
                batch.confirmed_points = batch
                    .confirmed_points
                    .and_then(|sum| sum.checked_add(points));
                batch.rows.push(json!({ "key": id, "ok": true, "already": already, "points": points,
                    "outcome": if already { "done" } else { "claimed" }, "confirmed": true, "source": "complete" }));
                batch.notes.push(if already {
                    format!("{title} 此前已结算（明确返回 0 积分）")
                } else {
                    format!("{title} 已确认 +{points}")
                });
            }
            Err(error) => {
                batch.failed += 1;
                let confirmed = result::rejected(&error);
                if !confirmed {
                    batch.unconfirmed += 1;
                }
                let message = if confirmed {
                    format!("{title} 失败：{}", error.message)
                } else {
                    format!("{title} 结果未确认：{}", error.message)
                };
                batch.notes.push(message.clone());
                batch.rows.push(
                    json!({ "key": id, "ok": false, "already": false, "points": null,
                    "outcome": "failed", "confirmed": confirmed, "error": message,
                    "status": error.status, "upstreamCode": error.upstream_code }),
                );
                if error.status == Some(401)
                    || error.code.as_deref() == Some("chatglm_account_conflict")
                {
                    // 账号授权或原凭据变化后不继续任务，不交付别人的后续结果。
                    batch.stopped = true;
                    break;
                }
            }
        }
    }
    batch
}

pub fn claim(session: &Session) -> Result<Value> {
    if session.guest {
        let mut result = guest();
        let batch = Batch::new();
        batch.fields(&mut result, "claimedPoints");
        result["success"] = json!(false);
        return Ok(result);
    }
    if let Some(snapshot) = saved(session)? {
        let mut result = served(&snapshot);
        let batch = Batch::new();
        batch.fields(&mut result, "claimedPoints");
        result["success"] = json!(true);
        return Ok(result);
    }
    let refreshed = if credentials::expiring(&session.secret) {
        Some(account::refresh_locked(session)?)
    } else {
        None
    };
    let session = refreshed.as_ref().unwrap_or(session);
    let before_center = center(session)?;
    let before = category(&before_center, "newbie")?;
    let batch = run(session, &before);
    // 仅原来这一处、最多一次回查；401路径原本提前结束，不能新增回查。
    let after = if batch.stopped || batch.invalid_identity {
        Err(result::unconfirmed("本批已停止，未执行领取后状态查询"))
    } else {
        center(session).and_then(|center| Ok((category(&center, "newbie")?, center)))
    };
    let mut result;
    let mut current = false;
    match after {
        Ok((category, center)) => {
            result = payload(&category);
            result["balance"] = balance(&center);
            current = result["statusConfirmed"] == true;
            if let Err(error) = remember(session, &mut result, None) {
                current = false;
                append_note(
                    &mut result,
                    &format!("本次领取结果已取得，但结算记忆保存失败：{}", error.message),
                );
            }
        }
        Err(error) => {
            result = payload(&before);
            // 旧快照只供显示原任务，不能冒充领取后的剩余数或已结清。
            result["earned"] = Value::Null;
            result["unclaimed"] = Value::Null;
            result["balance"] = Value::Null;
            result["settled"] = json!(false);
            result["statusConfirmed"] = json!(false);
            append_note(
                &mut result,
                &format!(
                    "领取后状态结果未确认：{}；此前逐项确认的领取结果仍保留",
                    error.message
                ),
            );
        }
    }
    batch.fields(&mut result, "claimedPoints");
    result["success"] = json!(batch.success() && current);
    result["msg"] = json!(batch.message("当前没有需要领取的新手任务，未新增奖励"));
    if let Some(note) = result["note"].as_str() {
        result["msg"] = json!(format!("{}；{note}", result["msg"].as_str().unwrap_or("")));
    }
    Ok(session.sanitize(&result))
}

pub fn daily(session: &Session) -> Result<Value> {
    if session.guest {
        return Ok(json!({ "success": false, "msg": "访客账号没有任务中心，跳过每日任务" }));
    }
    let center = center(session)?;
    let category = category(&center, "daily")?;
    let batch = run(session, &category);
    // 不新增领取后读取；progress 是原查询的真实快照，缺失仍为空，不推算新进度。
    let mut value = json!({ "success": batch.success(), "msg": batch.message(
        if tasks(&category).is_empty() { "当前没有每日任务，未新增奖励" } else { "每日任务此前已全部完成" }),
        "progress": category.get("progress").cloned().unwrap_or(Value::Null), "progressSource": "before" });
    batch.fields(&mut value, "gained");
    Ok(session.sanitize(&value))
}
