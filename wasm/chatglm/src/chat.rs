use crate::{account, endpoints, errors, models, network, plan, sse::Decoder, stream::Stream};
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

pub fn forward(params: &Value) -> Result<()> {
    plan::validate(&params["body"])?;
    let session = {
        let _lock = AccountLock::acquire()?;
        account::ready_locked()?
    };
    let requested = params["wire_model"]
        .as_str()
        .filter(|model| !model.is_empty())
        .or_else(|| params["body"]["model"].as_str())
        .unwrap_or("");
    let model = if requested.trim().is_empty() {
        let catalog = models::cached()?;
        catalog["models"]
            .as_array()
            .and_then(|models| models.first())
            .and_then(|model| model["id"].as_str())
            .ok_or_else(|| Error::http(403, "智谱当前账号的目录明确为空，请指定上游允许的模型"))?
            .to_string()
    } else {
        requested.to_string()
    };
    let body = plan::build(&params["body"], &model)?;
    session.check_current()?;
    let mut response = network::open(
        "POST",
        endpoints::CHAT,
        &session.token,
        &session.device,
        Some(&body),
        "text/event-stream",
    )?;
    let status = response.status;
    let mime = response
        .headers
        .as_object()
        .and_then(|headers| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("content-type"))
        })
        .and_then(|(_, value)| value.as_str())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if !(200..300).contains(&status) || mime == "application/json" {
        let bytes = response.bytes(4 * 1024 * 1024).map_err(|mut error| {
            error.message = session.redact(&error.message);
            error
        })?;
        let value = serde_json::from_slice(&bytes)
            .unwrap_or(json!({ "message": "智谱上游未返回有效对话" }));
        return Err(errors::upstream(
            status,
            &value,
            &[
                &session.token,
                &crate::credentials::refresh(&session.secret),
            ],
        ));
    }
    if !mime.contains("text/event-stream") {
        return Err(Error::http(502, "智谱对话没有返回事件流"));
    }
    let mut decoder = Decoder::default();
    let mut output = Stream::new(&model)?;
    let result: Result<()> = (|| {
        while let Some(chunk) = response.read()? {
            session.check_current()?;
            for event in decoder.push(&chunk)? {
                output.consume(&event, &session)?;
            }
            if output.finished {
                return output.complete();
            }
        }
        session.check_current()?;
        for event in decoder.finish()? {
            output.consume(&event, &session)?;
        }
        if output.finished {
            output.complete()
        } else {
            Err(Error::http(502, "智谱上游流在完成前结束，响应被截断"))
        }
    })();
    if let Err(mut error) = result {
        error.message = session.redact(&error.message);
        if output.started {
            let _ = sdk::stream_event(&errors::event(&error));
        }
        return Err(error);
    }
    Ok(())
}
