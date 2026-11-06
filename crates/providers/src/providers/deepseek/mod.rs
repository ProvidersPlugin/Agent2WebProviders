//! DeepSeek 网页账号使用官方会话与文件上传协议，遇到人机验证需手动登录。
//! 当前网页模型由 /api/v0/client/settings 的 model_configs 确认，只广告默认可用入口。
//! 图片先上传并等待 SUCCESS，再通过 ref_file_ids 交给模型；不使用 OCR 替代视觉。
//! 网页 completion 没有原生 tools 契约，不能把提示词模拟标成工具调用支持。
//! 未确认凭证续期、独立余额、签到或领取接口，不声明这些能力。

mod account;
mod images;
mod login;
mod models;
mod pow;
mod prompt;
mod response;
mod stream;

use std::time::Duration;
use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{control_error, ConversationSink, PluginCtx, Provider, ProviderMeta, SendFuture};
use futures::StreamExt;
use serde_json::{json, Value};

use response::{control as control_response, network_error};

pub const ID: &str = "deepseek-web";
const BASE: &str = "https://chat.deepseek.com";

pub struct DeepseekWebProvider;

pub fn id(prefix: &str) -> String {
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() { return format!("{prefix}-{}", crate::http::now_ms()); }
    format!("{prefix}-{}", bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>())
}

fn bad_input(message: &str) -> String {
    control_error(400, message, None, None)
}

fn client_builder(proxy: Option<&proto::WireProxy>, gateway: &str) -> Result<reqwest::ClientBuilder, String> {
    let mut builder = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).connect_timeout(Duration::from_secs(15));
    let url = proxy.map(crate::proxy::proxy_url).unwrap_or_else(|| gateway.to_string());
    if !url.trim().is_empty() && url != "direct" { builder = builder.proxy(reqwest::Proxy::all(&url).map_err(|_| "DeepSeek 出网代理无效")?); }
    Ok(builder)
}

fn client(proxy: Option<&proto::WireProxy>, gateway: &str) -> Result<reqwest::Client, String> {
    client_builder(proxy, gateway)?.build().map_err(|_| "DeepSeek 网络客户端创建失败".into())
}

fn record_client(record: &agent2api_provider_sdk::StoreAccount, gateway: &str) -> Result<reqwest::Client, String> {
    let config = crate::proxy::account_config(record);
    let proxy = crate::proxy::account_proxy(Some(&config)).map_err(|error| error.into_control_message())?;
    let gateway = if config.get("source").and_then(Value::as_str) == Some("direct") { "direct" } else { gateway };
    client(proxy.as_ref(), gateway)
}

fn headers(token: &str, cookie: &str, user_agent: &str) -> Vec<(String, String)> {
    let mut headers = vec![("Authorization".into(), format!("Bearer {token}")),
        ("User-Agent".into(), user_agent.into()),
        ("Accept".into(), "application/json".into()), ("Origin".into(), BASE.into()), ("Referer".into(), format!("{BASE}/")),
        ("x-client-platform".into(), "web".into()), ("x-client-version".into(), "2.7.0".into()), ("x-client-locale".into(), "zh_CN".into()),
        ("x-client-bundle-id".into(), "com.deepseek.chat".into())];
    if !cookie.is_empty() { headers.push(("Cookie".into(), cookie.into())); }
    headers
}

async fn post(client: &reqwest::Client, path: &str, headers: &[(String, String)], body: Value) -> Result<Value, String> {
    let mut request = client.post(format!("{BASE}{path}")).json(&body).timeout(Duration::from_secs(30));
    for (name, value) in headers { request = request.header(name, value); }
    control_response(request.send().await.map_err(network_error)?, headers).await
}

async fn get(client: &reqwest::Client, path: &str, headers: &[(String, String)], query: &[(&str, &str)]) -> Result<Value, String> {
    let mut request = client.get(format!("{BASE}{path}")).query(query).timeout(Duration::from_secs(30));
    for (name, value) in headers { request = request.header(name, value); }
    control_response(request.send().await.map_err(network_error)?, headers).await
}

async fn proof(client: &reqwest::Client, headers: &[(String, String)], path: &str) -> Result<String, String> {
    let challenge = post(client, "/api/v0/chat/create_pow_challenge", headers, json!({ "target_path": path })).await?;
    let challenge = challenge.pointer("/data/biz_data/challenge").cloned().ok_or("DeepSeek 没有返回验证 challenge")?;
    let path = path.to_string();
    tokio::task::spawn_blocking(move || pow::solve_for(&challenge, &path)).await.map_err(|_| "DeepSeek 验证任务失败")?
}

async fn prepare_and_forward(ctx: &PluginCtx, req: proto::ConversationRequest, sink: &mut ConversationSink<'_>, head_sent: &mut bool) -> Result<(), String> {
    let prompt = prompt::build(&req.body)?;
    let record = ctx.store_get(ID, &req.account).await.map_err(|_| "DeepSeek 网页账号不存在")?;
    let token = record.secret["accessToken"].as_str().filter(|token| !token.is_empty()).ok_or("DeepSeek 网页账号缺少访问令牌")?;
    let cookie = record.secret["cookie"].as_str().unwrap_or("");
    let headers = headers(token, cookie, account::user_agent(&record.secret)?);
    let client = client(req.proxy.as_ref(), &ctx.env.proxy)?;
    let model = if req.wire_model.is_empty() { req.body["model"].as_str().unwrap_or("") } else { &req.wire_model };
    if !models::accepts(model) { return Err(bad_input("DeepSeek 网页请选择 deepseek-web-flash")); }
    let catalog = models::load(&record.id, &client, &headers, false).await?;
    if !prompt.images.is_empty() && !catalog.images { return Err(bad_input("DeepSeek 当前网页模型没有图片输入能力")); }
    let thinking = prompt::thinking(&req.body);
    let file_ids = images::upload(&prompt.images, &client, &headers, &catalog.mode, thinking, req.proxy.as_ref(), &ctx.env.proxy).await?;
    let created = post(&client, "/api/v0/chat_session/create", &headers, json!({})).await?;
    let session = created.pointer("/data/biz_data/id").or_else(|| created.pointer("/data/biz_data/chat_session/id")).and_then(Value::as_str).ok_or("DeepSeek 没有返回会话 id")?;
    let proof = proof(&client, &headers, "/api/v0/chat/completion").await?;
    let mut request = client.post(format!("{BASE}/api/v0/chat/completion")).json(&json!({
        "chat_session_id": session, "parent_message_id": null, "model_type": catalog.mode, "prompt": prompt.text,
        "ref_file_ids": file_ids, "thinking_enabled": thinking, "search_enabled": false, "action": null, "preempt": false,
    })).header("x-ds-pow-response", proof).timeout(Duration::from_secs(600));
    for (name, value) in &headers { request = request.header(name, value); }
    let response = request.header("Accept", "text/event-stream").send().await.map_err(|error| format!("DeepSeek 对话发送失败：{error}"))?;
    if !response.status().is_success() { return Err(control_error(i32::from(response.status().as_u16()), "DeepSeek 对话被上游拒绝，请检查账号与 Cookie", None, None)); }
    if !response.headers().get("content-type").and_then(|value| value.to_str().ok()).is_some_and(|mime| mime.contains("text/event-stream")) {
        return Err(control_error(403, "DeepSeek 网页要求重新登录或人机验证", None, None));
    }
    let response_id = id("chatcmpl");
    let streaming = req.stream;
    let mut decoder = stream::Decoder::default();
    let mut utf8 = Vec::new();
    let mut source = response.bytes_stream();
    sink.head(proto::StreamHead { status: 200, headers: [("content-type".into(), "text/event-stream".into())].into_iter().collect(), upstream: proto::UpstreamKind::Chat }).await?;
    *head_sent = true;
    while let Some(bytes) = source.next().await {
        let bytes = bytes.map_err(|_| "DeepSeek 对话传输中断")?;
        let before_content = decoder.content.len();
        let before_reasoning = decoder.reasoning.len();
        utf8.extend_from_slice(&bytes);
        let valid = match std::str::from_utf8(&utf8) { Ok(text) => text.len(), Err(error) if error.error_len().is_none() => error.valid_up_to(), Err(_) => return Err("DeepSeek 响应不是 UTF-8".into()) };
        let text = std::str::from_utf8(&utf8[..valid]).map_err(|_| "DeepSeek 响应无法解码")?.replace("\r\n", "\n");
        decoder.feed(&text)?; utf8.drain(..valid);
        if decoder.content.len() + decoder.reasoning.len() > 8 * 1024 * 1024 { return Err("DeepSeek 响应超过大小上限".into()); }
        if streaming {
            let mut delta = json!({});
            if decoder.content.len() > before_content { delta["content"] = json!(&decoder.content[before_content..]); }
            if decoder.reasoning.len() > before_reasoning { delta["reasoning_content"] = json!(&decoder.reasoning[before_reasoning..]); }
            if delta.as_object().is_some_and(|value| !value.is_empty()) { send_delta(sink, &response_id, model, delta, None).await?; }
        }
    }
    if !utf8.is_empty() { return Err("DeepSeek 响应被截断".into()); }
    decoder.finish()?;
    if streaming {
        send_delta(sink, &response_id, model, json!({}), Some("stop")).await?;
        if decoder.total_tokens > 0 { sink.send(format!("data: {}\n\n", json!({ "id": response_id, "object": "chat.completion.chunk", "model": model, "choices": [], "usage": { "total_tokens": decoder.total_tokens } })).as_bytes()).await?; }
        sink.send(b"data: [DONE]\n\n").await?;
    } else {
        let body = stream::completion(&response_id, model, &decoder);
        send_result_stream(sink, &body).await?;
    }
    Ok(())
}

async fn send_result_stream(sink: &mut ConversationSink<'_>, body: &Value) -> Result<(), String> {
    let message = &body["choices"][0]["message"];
    let mut delta = message.clone();
    if let Some(calls) = delta.get_mut("tool_calls").and_then(Value::as_array_mut) { for (index, call) in calls.iter_mut().enumerate() { call["index"] = json!(index); } }
    let finish = body["choices"][0]["finish_reason"].as_str();
    send_delta(sink, body["id"].as_str().unwrap_or(""), body["model"].as_str().unwrap_or(""), delta, None).await?;
    send_delta(sink, body["id"].as_str().unwrap_or(""), body["model"].as_str().unwrap_or(""), json!({}), finish).await?;
    if let Some(usage) = body.get("usage") { sink.send(format!("data: {}\n\n", json!({ "id": body["id"], "object": "chat.completion.chunk", "model": body["model"], "choices": [], "usage": usage })).as_bytes()).await?; }
    sink.send(b"data: [DONE]\n\n").await
}

async fn send_delta(sink: &mut ConversationSink<'_>, id: &str, model: &str, delta: Value, finish: Option<&str>) -> Result<(), String> {
    sink.send(format!("data: {}\n\n", json!({ "id": id, "object": "chat.completion.chunk", "created": crate::http::now_ms() / 1000, "model": model,
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }] })).as_bytes()).await
}

impl Provider for DeepseekWebProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(ID, "DeepSeek 网页").with_capabilities(proto::Capabilities {
            chat: true, stateful: true, web_login: true, model_refresh: true, ..Default::default()
        }).with_models(models::fallback()).with_login(vec![
                proto::LoginMethod::Browser { button: "网页登录".into(), hint: Some("在桌面独立窗口中登录 DeepSeek 官网，自动读取登录态并添加账号；浏览器面板请粘贴令牌。".into()) },
                proto::LoginMethod::Token { label: "网页访问令牌".into(), placeholder: "chat.deepseek.com 的 userToken.value，不是收费 API Key".into(),
                    extra_fields: vec![proto::LoginField { key: "cookie".into(), label: "网页 Cookie".into(), placeholder: "官方网页正常登录后的 Cookie；风控时请手动更新".into(), optional: true, multiline: true }] },
            ])
            .with_account_features(proto::AccountFeatures { identifier: vec!["uid".into()], ..Default::default() })
    }
    fn conversation<'a>(&'a self, ctx: &'a PluginCtx, req: proto::ConversationRequest, sink: ConversationSink<'a>) -> SendFuture<'a, ()> {
        Box::pin(async move {
            let mut sink = sink;
            let mut head_sent = false;
            match prepare_and_forward(ctx, req, &mut sink, &mut head_sent).await {
                Ok(()) => Ok(()),
                Err(error) => {
                    if head_sent { return Err(error); }
                    let failure = error.strip_prefix("CONTROL_ERROR:").and_then(|raw| serde_json::from_str::<Value>(raw).ok());
                    if let Some(failure) = failure {
                        let status = failure["status"].as_u64().filter(|status| (400..600).contains(status)).unwrap_or(502) as u16;
                        sink.head(proto::StreamHead { status, headers: [("content-type".into(), "application/json".into())].into_iter().collect(), upstream: proto::UpstreamKind::Chat }).await?;
                        sink.send(json!({ "message": failure["message"] }).to_string().as_bytes()).await?;
                        Ok(())
                    } else { Err(error) }
                }
            }
        })
    }
    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move { ctx.store_get(ID, account).await.map_err(|error| error.to_string())?.secret["accessToken"].as_str().map(str::to_string).ok_or_else(|| "DeepSeek 网页缺少令牌".into()) })
    }
    fn models<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async move {
            let record = ctx.store_get(ID, account).await.map_err(|_| "DeepSeek 网页账号不存在")?;
            let token = record.secret["accessToken"].as_str().filter(|token| !token.is_empty()).ok_or("DeepSeek 网页账号缺少访问令牌")?;
            let client = record_client(&record, &ctx.env.proxy)?;
            let headers = headers(token, record.secret["cookie"].as_str().unwrap_or(""), account::user_agent(&record.secret)?);
            Ok(models::load(&record.id, &client, &headers, false).await?.list)
        })
    }
    fn refresh_models<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str, force: bool) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            let record = ctx.store_get(ID, account).await.map_err(|_| "DeepSeek 网页账号不存在")?;
            let token = record.secret["accessToken"].as_str().filter(|token| !token.is_empty()).ok_or("DeepSeek 网页账号缺少访问令牌")?;
            let client = record_client(&record, &ctx.env.proxy)?;
            let headers = headers(token, record.secret["cookie"].as_str().unwrap_or(""), account::user_agent(&record.secret)?);
            match models::load(&record.id, &client, &headers, force).await {
                Ok(catalog) => Ok(proto::RefreshOutcome::refreshed(catalog.list.models.len())),
                Err(error) => Ok(proto::RefreshOutcome::failed(error)),
            }
        })
    }
    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        let message = body["message"].as_str().unwrap_or("DeepSeek 网页请求失败").to_string();
        match status { 401 => proto::ErrorClass::TokenExpired { message }, 429 => proto::ErrorClass::QuotaLimited { reset_at: None, message, upstream_code: None, status },
            _ => proto::ErrorClass::Fatal { status, message, upstream_code: None } }
    }
    fn admin_action<'a>(&'a self, _ctx: &'a PluginCtx, _account: &'a str, action: &'a str, _payload: &'a Value) -> SendFuture<'a, Value> {
        Box::pin(async move {
            if action != "auth.methods" {
                return Err(control_error(501, format!("DeepSeek 网页没有实现管理动作「{action}」"), None, None));
            }
            Ok(json!({ "methods": [{
                "methodIndex": 1, "type": "oauth", "label": "网页登录", "localOnly": true,
                "instructions": "在桌面独立窗口中登录 DeepSeek 官网，自动读取登录态并添加账号；浏览器面板请粘贴令牌。",
                "prompts": [{ "key": "name", "type": "text", "label": "备注名", "placeholder": "可选，留空使用账号默认名称", "optional": true }]
            }, {
                "methodIndex": 0, "type": "api", "label": "粘贴网页访问令牌",
                "instructions": "在 chat.deepseek.com 登录本人账号后，从浏览器开发者工具的 Application → Local Storage 复制 userToken 的 value。Cookie 可选，需补充时粘贴该站点请求里的完整 Cookie；登录态失效或要求人机验证时，请回官方网页手动登录后更新。这里不接受收费 API Key。",
                "prompts": [
                    { "key": "accessToken", "type": "text", "label": "网页访问令牌", "placeholder": "chat.deepseek.com 的 userToken.value，不是收费 API Key", "secret": true },
                    { "key": "cookie", "type": "text", "label": "网页 Cookie", "placeholder": "官方网页正常登录后的 Cookie；风控时请手动更新", "optional": true, "secret": true }
                ]
            }] }))
        })
    }
    fn login_start<'a>(&'a self, _ctx: &'a PluginCtx, _method: &'a str, payload: &'a Value) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(async move { login::start(payload) })
    }
    fn login_callback<'a>(&'a self, ctx: &'a PluginCtx, callback: proto::LoginCallback) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async move { login::callback(ctx, callback).await })
    }
    fn login_poll<'a>(&'a self, _ctx: &'a PluginCtx, flow: &'a str) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async move { login::poll(flow) })
    }
    fn login_cancel<'a>(&'a self, _ctx: &'a PluginCtx, flow: &'a str) -> SendFuture<'a, ()> {
        Box::pin(async move { login::cancel(flow); Ok(()) })
    }
    fn normalize_account<'a>(&'a self, ctx: &'a PluginCtx, input: proto::NormalizeInput) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move { account::normalize(ctx, input).await })
    }
}
