//! 会话严格保持顺序：join 返回头但保留连接 → runs 投递 → 读取 join 事件。
//! 账号锁跨整个回答；仅 runs 调用前的真实 join HTTP 409 可沿用一次换房。
//! 已进入 runs 后，缺回执不证明未被接受，流内错误/截断/409 都不能换房重发。

use agent2api_wasm_sdk::{http, json, AccountLock, Error, Result, Value};

use crate::{credentials, errors, models, network::Client, prompt, sign, stream, Site};

fn session(client: &mut Client) -> Result<String> {
    // 上游房间自己记住历史。每次请求发送完整历史时不能复用账号级房间，
    // 否则无关客户端相互串话，后续轮次还会重复叠加已有历史。
    let path = if client.site.id == "tabbit" {
        "/api/v3/chat/rooms"
    } else {
        "/panel/session"
    };
    let body = (client.site.id == "tabbit").then(|| json!({}));
    let value = client.signed_json("POST", path, body.as_ref(), "/newtab")?;
    let id = value[if client.site.id == "tabbit" {
        "room_id"
    } else {
        "chat_session_id"
    }]
    .as_str()
    .filter(|id| !id.trim().is_empty() && id.len() <= 1024 && !id.chars().any(char::is_control))
    .ok_or_else(|| Error::http(502, "Tabbit 没有返回有效会话 id"))?
    .to_string();
    client.current()?;
    Ok(id)
}

fn join_body(client: &Client, room: &str) -> Result<String> {
    let seed = format!("{}|{room}", client.credentials.uid);
    Ok(json!({ "page_instance_id": sign::random_uuid()?,
        "device_id_hash": sign::sha256_hex(format!("tabbit-page:{seed}")),
        "surface": if client.site.id == "tabbit" { "session" } else { "panel" }, "last_event_id": null })
    .to_string())
}

fn run_body(prompt: &str, model: &str, site: Site) -> Result<String> {
    let run = format!("tab_{}", sign::random_uuid()?);
    let html = prompt
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    Ok(json!({
        "client_run_id": run, "client_message_id": run, "agent_plan_mode_enabled": site.id == "tabbit",
        "input_payload": { "task_name": "chat", "content": prompt, "parent_message_id": null,
            "selected_model": [model], "references": [], "metadatas": { "html_content": format!("<p>{html}</p>") },
            "page_info_list": [], "is_mobile": false, "agent_mode": false,
            "entity": { "key": sign::md5_empty_hex(), "extras": { "type": "tab", "url": "" } } }
    }).to_string())
}

fn join(client: &mut Client, room: &str) -> Result<http::Response> {
    let body = join_body(client, room)?;
    let path = format!("/api/v3/chat/rooms/{}/join", sign::escape(room));
    for attempt in 0..2 {
        let mut headers = client.signed_headers(&format!("/panel/{room}"), &body, false)?;
        headers["Accept"] = json!("text/event-stream");
        headers["Content-Type"] = json!("application/json");
        headers["Cache-Control"] = json!("no-cache");
        let response = client.open("POST", &path, headers, Some(&body))?;
        if response.status == 499 && attempt == 0 {
            drop(response);
            client.refresh_sign_key()?;
            continue;
        }
        let status = response.status;
        client.current()?;
        if !(200..300).contains(&status) {
            let mut error = match client.json_response(response) {
                Err(error) => error,
                Ok(value) => errors::upstream(status, &value, &client.credentials),
            };
            if status == 409 && error.status == Some(409) {
                // 只由本次 join 的真实 HTTP 状态生成；此时还没有调用本次 runs。
                error.code = Some("tabbit_join_conflict_before_run".into());
            }
            return Err(error);
        }
        let mime = response.headers["content-type"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase();
        if mime.contains("application/json") {
            let value = client.json_response(response)?;
            let message = errors::text(&value);
            let status = errors::event_status(&value.to_string(), &message);
            return Err(errors::upstream(status, &value, &client.credentials));
        }
        if !mime.contains("text/event-stream") {
            return Err(Error::http(
                502,
                "Tabbit join 没有返回事件流，请检查网页登录态",
            ));
        }
        return Ok(response);
    }
    Err(Error::http(502, "Tabbit 签名密钥刷新后仍被拒绝"))
}

fn run(client: &mut Client, room: &str, body: &str) -> Result<String> {
    // 与原生版一致：runs 带 Cookie/trace，不附加 join 所用的签名或 Bearer。
    let mut headers = client.headers(&format!("/panel/{room}"))?;
    headers["Accept"] = json!("application/json");
    headers["Content-Type"] = json!("application/json");
    headers["trace-id"] = json!(sign::random_uuid()?);
    headers["Cookie"] = json!(client.credentials.cookie);
    let path = format!("/api/v3/chat/rooms/{}/runs", sign::escape(room));
    let response = client
        .open("POST", &path, headers, Some(body))
        .map_err(errors::unconfirmed)?;
    let status = response.status;
    let value = client.json_response(response).map_err(|error| {
        // 明确4xx拒绝可以分类；网络/服务端/HTTP成功坏正文都不能证明未接受。
        if (400..500).contains(&status) {
            error
        } else {
            errors::unconfirmed(error)
        }
    })?;
    let id = credentials::text(&value, &["run_id", "runId"]);
    if id.is_empty() || id.len() > 1024 || id.chars().any(char::is_control) {
        return Err(errors::unconfirmed(Error::http(
            502,
            "Tabbit 未返回有效 run 信息",
        )));
    }
    Ok(id)
}

pub fn forward(site: Site, params: &Value) -> Result<()> {
    let prompt = prompt::build(&params["body"])?;
    let _lock = AccountLock::acquire()?;
    let mut client = Client::for_account(site)?;
    let requested = params["wire_model"]
        .as_str()
        .filter(|model| !model.is_empty())
        .or_else(|| params["body"]["model"].as_str())
        .unwrap_or("");
    let (catalog, _) = models::load(&mut client, false)?;
    let selected = models::select(&catalog, requested)?;
    let display = if requested.trim().is_empty() {
        selected.as_str()
    } else {
        requested
    };
    for attempt in 0..2 {
        let room = session(&mut client)?;
        let response = match join(&mut client, &room) {
            Ok(response) => response,
            Err(mut error)
                if error.status == Some(409)
                    && error.code.as_deref() == Some("tabbit_join_conflict_before_run") =>
            {
                // 只有尚未发送 runs 的 join 冲突可换成一间新的空房，不删除上游房间。
                error.code = Some("tabbit_upstream".into());
                if attempt == 0 {
                    continue;
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        let body = run_body(&prompt, &selected, site)?;
        // 此调用可能已被上游接受，即使响应丢失/解析失败也不能再次投递。
        let run = run(&mut client, &room, &body)?;
        // response 未读也未销毁，投递 runs 时 join 连接仍保持。
        client.current()?;
        return stream::forward(response, &run, display, &client).map_err(|failure| {
            let mut error = failure.error;
            if failure.started {
                error.code = Some("plugin_response_started".into());
            } else {
                // 已返回run后即使尚无正文，失联/坏帧也不可被账号顺延重建任务。
                error = errors::unconfirmed(error);
            }
            error
        });
    }
    Err(Error::http(502, "Tabbit 会话反复冲突，请稍后重试"))
}
