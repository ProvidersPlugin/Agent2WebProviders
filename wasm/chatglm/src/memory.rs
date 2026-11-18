use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope, Snapshot},
    Error, Result, Value,
};

pub fn get(key: &str) -> Result<Option<Value>> {
    let snapshot = state::get(Scope::Account, key, 1)?;
    if !snapshot.compatible {
        return Ok(None);
    }
    Ok((!snapshot.value.is_null()).then_some(snapshot.value))
}

pub fn put(key: &str, value: Value) -> Result<()> {
    let before = state::get(Scope::Account, key, 1)?;
    if !state::compare_exchange(Scope::Account, key, 1, before.revision, value, 0)? {
        return Err(Error::http(409, "智谱账号状态已被另一轮操作更新，请重试")
            .with_code("chatglm_state_conflict"));
    }
    Ok(())
}

/// 写动作先登记在途状态；取消、超时或未知响应不会在下一次调用中盲目补发。
/// 每日动作跨日只放行此前已确认的结果；旧在途记录必须由真实状态澄清，不能随零点清空。
pub fn once(
    session: &crate::account::Session,
    action: &str,
    daily: bool,
    work: impl FnOnce() -> Result<Value>,
) -> Result<Value> {
    use sha2::{Digest, Sha256};
    session.check_current()?;
    let identity = if session.uid.is_empty() {
        format!("account:{}", session.account)
    } else {
        session.uid.clone()
    };
    validate_uid(&identity)?;
    let key = format!("chatglm.action.{:x}", Sha256::digest(action.as_bytes()));
    let day = if daily {
        crate::sign::now()?.saturating_add(28_800_000) / 86_400_000
    } else {
        0
    };
    let before = state::get(Scope::Account, &key, 1)?;
    if !before.compatible || before.revision == 0 && !before.value.is_null() {
        return Err(invalid("动作记忆格式不兼容，未按未执行处理"));
    }
    if !before.value.is_null() {
        let value = &before.value;
        if !value.is_object()
            || value["uid"].as_str() != Some(identity.as_str())
            || value["day"].as_i64().is_none()
            || !value.get("payload").is_some()
        {
            return Err(invalid("动作记忆与当前账号或日期不一致，未重复提交"));
        }
        match value["state"].as_str() {
            Some("pending") => return Err(invalid("此前动作已经提交但结果未确认，未自动重复提交")),
            Some("confirmed") if value["day"] == day => {
                session.check_current()?;
                let mut payload = value["payload"].clone();
                if payload.is_number() {
                    return Ok(json!(0));
                }
                if payload["outcome"] == "claimed" {
                    payload["outcome"] = json!("done");
                    payload["credit"] = Value::Null;
                    payload["points"] = json!(0);
                    payload["message"] = json!("此前领取已确认，未重复提交或新增奖励");
                }
                return Ok(payload);
            }
            Some("confirmed" | "rejected") => {}
            _ => return Err(invalid("动作记忆状态无效，未重复提交")),
        }
    }
    let pending = json!({ "uid": identity, "day": day, "state": "pending", "payload": null });
    if !state::compare_exchange(Scope::Account, &key, 1, before.revision, pending, 0)? {
        return Err(invalid("动作已经由另一轮操作登记，未重复发送"));
    }
    session.check_current()?;
    if daily && crate::sign::now()?.saturating_add(28_800_000) / 86_400_000 != day {
        return Err(invalid("动作登记期间跨日，未继续发送"));
    }
    let result = work();
    session.check_current()?;
    let (state_name, payload) = match &result {
        Ok(value) => ("confirmed", value.clone()),
        Err(error)
            if error.code.as_deref() == Some("chatglm_action_rejected")
                || crate::claim_result::rejected(error) && error.status == Some(401) =>
        {
            ("rejected", Value::Null)
        }
        Err(_) => return result,
    };
    if payload.to_string().len() > 64 * 1024 {
        return Err(invalid("动作结果超过记忆限制，原结果仍未确认，未自动重发"));
    }
    if !state::compare_exchange(
        Scope::Account,
        &key,
        1,
        before.revision.saturating_add(1),
        json!({ "uid": identity, "day": day, "state": state_name, "payload": payload }),
        0,
    )? {
        return Err(invalid("动作已返回但记忆保存冲突，未自动再次提交"));
    }
    result
}

const ONBOARDING_KEY: &str = "chatglm.onboardingSettled";
const ONBOARDING_SCHEMA: u32 = 1;

fn invalid(detail: &str) -> Error {
    crate::claim_result::unconfirmed(detail)
}

fn validate_uid(uid: &str) -> Result<()> {
    if uid.trim().is_empty() || uid.len() > 512 || uid.chars().any(char::is_control) {
        return Err(invalid("当前账号身份无效，未读取或改写结算记忆"));
    }
    Ok(())
}

/// 老格式只有 at/tasks/earned/total，不能强制要求新增加的确认字段。
/// 但若显式给了 settled/statusConfirmed/unclaimed，就不能与已结清事实冲突。
fn validate_snapshot(value: &Value, uid: &str) -> Result<()> {
    let valid = (|| {
        let object = value.as_object()?;
        if object
            .get("uid")
            .is_some_and(|value| value.as_str() != Some(uid))
        {
            return None;
        }
        for flag in ["settled", "statusConfirmed"] {
            if object
                .get(flag)
                .is_some_and(|value| value.as_bool() != Some(true))
            {
                return None;
            }
        }
        if object
            .get("unclaimed")
            .is_some_and(|value| value.as_u64() != Some(0))
        {
            return None;
        }
        let tasks = object
            .get("tasks")?
            .as_array()
            .filter(|tasks| !tasks.is_empty())?;
        let mut ids = std::collections::BTreeSet::new();
        let sum = tasks.iter().try_fold(0_i64, |sum, task| {
            let id = task.get("key")?.as_str()?;
            if task.get("done")?.as_bool() != Some(true)
                || id.trim().is_empty()
                || id.len() > 512
                || id.chars().any(char::is_control)
                || !ids.insert(id.trim())
            {
                return None;
            }
            sum.checked_add(crate::claim_result::integer(&task["points"])?)
        })?;
        (crate::claim_result::integer(&value["earned"]) == Some(sum)
            && crate::claim_result::integer(&value["total"]) == Some(sum))
        .then_some(())
    })()
    .is_some();
    if valid {
        Ok(())
    } else {
        Err(invalid(
            "已结清任务记忆的形状、完成状态、标识或金额异常，未继续领取",
        ))
    }
}

fn validate_state(before: &Snapshot, uid: &str) -> Result<()> {
    if !before.compatible || before.schema != ONBOARDING_SCHEMA {
        return Err(invalid("结算记忆格式版本不兼容，未回退旧台账或继续领取"));
    }
    if before.revision == 0 && !before.value.is_null() {
        return Err(invalid("结算记忆版本与内容不一致，未继续领取"));
    }
    if before.value.is_null() {
        return Ok(());
    }
    let object = before
        .value
        .as_object()
        .ok_or_else(|| invalid("结算记忆容器不是对象，未继续领取"))?;
    if object.get("uid").and_then(Value::as_str) != Some(uid) {
        return Err(invalid("结算记忆身份与当前账号不一致，未继续领取"));
    }
    let snapshot = object
        .get("snapshot")
        .ok_or_else(|| invalid("结算记忆缺少快照字段，不能当作未结算"))?;
    if !snapshot.is_null() {
        validate_snapshot(snapshot, uid)?;
    }
    Ok(())
}

/// present 与 Snapshot 必须来自同一次宿主返回；缺标记的旧宿主不能证明真正缺失。
fn read_onboarding(uid: &str) -> Result<(Snapshot, bool)> {
    let raw = sdk::host(
        "state.get",
        json!({
            "scope": "account", "key": ONBOARDING_KEY, "schema": ONBOARDING_SCHEMA
        }),
    )?;
    let present = raw["present"].as_bool().ok_or_else(|| {
        invalid("主程序未提供严格状态存在标志，请更新主程序；未迁移记忆或继续领取")
    })?;
    let before: Snapshot = serde_json::from_value(raw)
        .map_err(|_| invalid("主程序返回的结算记忆快照格式异常，未继续领取"))?;
    validate_state(&before, uid)?;
    if present && before.revision == 0
        || !present && (before.revision != 0 || !before.value.is_null())
    {
        return Err(invalid("结算记忆存在标志与版本或内容冲突，未继续领取"));
    }
    Ok((before, present))
}

fn save_at(revision: u64, uid: &str, snapshot: Value) -> Result<()> {
    if !state::compare_exchange(
        Scope::Account,
        ONBOARDING_KEY,
        ONBOARDING_SCHEMA,
        revision,
        json!({ "uid": uid, "snapshot": snapshot }),
        0,
    )? {
        return Err(Error::http(
            409,
            "结果未确认：结算记忆已被其他操作改变，本次未覆盖，未自动重试",
        )
        .with_code("chatglm_state_conflict"));
    }
    Ok(())
}

/// 仅严格入口明确 present=false 且零版本空快照时，进行原有一次旧台账读取。
/// 旧宿主会把坏外层 pluginState 折空且不带 present，因此明确拒绝，不猜版本。
/// Account scope 不使用 TTL；present 是存储事实，不是领取批准。
pub fn onboarding(uid: &str) -> Result<Option<Value>> {
    validate_uid(uid)?;
    let (before, present) = read_onboarding(uid)?;
    if present {
        // 已存在的显式 null（含 snapshot:null）已有未结算结论，不回旧台账。
        let snapshot = if before.value.is_null() {
            Value::Null
        } else {
            before.value["snapshot"].clone()
        };
        return Ok((!snapshot.is_null()).then_some(snapshot));
    }
    // 宿主严格 ledger.get 已确认同 provider/account；错误必须传播，不能折成 null。
    let legacy = sdk::host("ledger.get", json!({ "kind": "onboardingSettled" }))?;
    if !legacy.is_null() {
        validate_snapshot(&legacy, uid)?;
    }
    // 始终使用最初读取版本；不经通用 put 重读较新版本覆盖并发写入。
    save_at(before.revision, uid, legacy.clone())?;
    Ok((!legacy.is_null()).then_some(legacy))
}

pub fn save_onboarding(uid: &str, snapshot: Value) -> Result<()> {
    validate_uid(uid)?;
    if !snapshot.is_null() {
        validate_snapshot(&snapshot, uid)?;
    }
    let (before, present) = read_onboarding(uid)?;
    if !present {
        // 正常调用此前已读过 onboarding；期间状态消失不能自动重建以恢复发送资格。
        return Err(invalid("原结算记忆已不存在，本次未自动重建或清空"));
    }
    save_at(before.revision, uid, snapshot)
}
