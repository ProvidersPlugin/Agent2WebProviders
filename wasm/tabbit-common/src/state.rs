//! 每项状态存进宿主按账号加密的 state.v1；不依赖 WASM 实例内的全局变量。
//! Account scope 没有自动 TTL，因此数据内显式保存 expires_at 并在读时检查。
//! 写入使用版本比较；调用者持有账号锁，冲突时不覆写另一个请求的状态。

use agent2api_wasm_sdk::{
    json,
    state::{self, Scope},
    Error, Result, Value,
};

const SCHEMA: u32 = 1;

pub fn get(key: &str, binding: &str) -> Result<Option<Value>> {
    let snapshot = state::get(Scope::Account, key, SCHEMA)?;
    if !snapshot.compatible || snapshot.value["binding"] != binding {
        return Ok(None);
    }
    let now = super::now()?;
    let since = snapshot.value["saved_at"].as_i64().unwrap_or(0);
    let expires = snapshot.value["expires_at"].as_i64().unwrap_or(0);
    if since > now || expires <= now {
        return Ok(None);
    }
    Ok(snapshot
        .value
        .get("data")
        .filter(|value| !value.is_null())
        .cloned())
}

pub fn put(key: &str, binding: &str, data: Value, ttl_ms: i64) -> Result<()> {
    let snapshot = state::get(Scope::Account, key, SCHEMA)?;
    let now = super::now()?;
    let value = json!({ "binding": binding, "saved_at": now,
        "expires_at": now.saturating_add(ttl_ms), "data": data });
    if !state::compare_exchange(
        Scope::Account,
        key,
        SCHEMA,
        snapshot.revision,
        value,
        ttl_ms,
    )? {
        return Err(Error::http(409, "Tabbit 状态已被另一轮操作更新，请重试")
            .with_code("tabbit_state_conflict"));
    }
    Ok(())
}

#[allow(dead_code)]
pub fn clear(key: &str) -> Result<()> {
    let snapshot = state::get(Scope::Account, key, SCHEMA)?;
    if !state::compare_exchange(
        Scope::Account,
        key,
        SCHEMA,
        snapshot.revision,
        Value::Null,
        1000,
    )? {
        return Err(
            Error::http(409, "Tabbit 会话状态更新冲突，请重试").with_code("tabbit_state_conflict")
        );
    }
    Ok(())
}
