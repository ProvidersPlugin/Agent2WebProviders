use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

use agent2api_provider_protocol as proto;
use base64::Engine;
use futures::StreamExt;
use serde_json::Value;

const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_IMAGES: usize = 50;
const UPLOAD_PATH: &str = "/api/v0/file/upload_file";

pub struct ImageInput {
    pub source: String,
}

pub fn input(part: &Value) -> Result<Option<ImageInput>, String> {
    let source = match part["type"].as_str() {
        Some("image_url" | "input_image") => {
            let value = part.get("image_url").unwrap_or(part);
            value.as_str().or_else(|| value["url"].as_str()).map(str::to_string)
        }
        Some("image") => match part.pointer("/source/type").and_then(Value::as_str) {
            Some("base64") => part.pointer("/source/data").and_then(Value::as_str).map(|data| {
                format!("data:{};base64,{data}", part.pointer("/source/media_type").and_then(Value::as_str).unwrap_or("image/png"))
            }),
            Some("url") => part.pointer("/source/url").and_then(Value::as_str).map(str::to_string),
            _ => None,
        },
        _ => return Ok(None),
    };
    let source = source.filter(|source| !source.trim().is_empty()).ok_or_else(|| {
        super::bad_input("DeepSeek 图片缺少 data URL 或公网图片 URL")
    })?;
    Ok(Some(ImageInput { source }))
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, _, _] = ip.octets();
            !ip.is_private() && !ip.is_loopback() && !ip.is_link_local() && !ip.is_broadcast()
                && !ip.is_documentation() && !ip.is_unspecified() && !ip.is_multicast()
                && a != 0 && a < 224 && !(a == 100 && (64..=127).contains(&b))
                && !(a == 198 && (b == 18 || b == 19))
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() { return public_ip(IpAddr::V4(v4)); }
            let [first, second, third, _, _, _, _, _] = ip.segments();
            !ip.is_loopback() && !ip.is_unspecified() && !ip.is_multicast()
                && first & 0xfe00 != 0xfc00 && first & 0xffc0 != 0xfe80
                && first & 0xe000 == 0x2000 && !(first == 0x2001 && second == 0x0db8)
                && !(first == 0x2001 && second < 0x0200)
                && !(first == 0x2002 && third == 0)
                && first & 0xfff0 != 0x3ff0
        }
    }
}

async fn download(source: &str, proxy: Option<&proto::WireProxy>, gateway: &str) -> Result<Vec<u8>, String> {
    if source.len() > 8192 { return Err(super::bad_input("DeepSeek 图片 URL 过长，请使用 base64 图片")); }
    let url = url::Url::parse(source).map_err(|_| super::bad_input("DeepSeek 图片 URL 无效"))?;
    if !matches!(url.scheme(), "http" | "https") || !url.username().is_empty() || url.password().is_some() {
        return Err(super::bad_input("DeepSeek 图片仅接受无用户名密码的公网 HTTP(S) URL"));
    }
    let host = url.host_str().ok_or_else(|| super::bad_input("DeepSeek 图片 URL 缺少主机"))?.to_string();
    let port = url.port_or_known_default().ok_or_else(|| super::bad_input("DeepSeek 图片 URL 端口无效"))?;
    let target = host.clone();
    let addresses = tokio::time::timeout(Duration::from_secs(10), tokio::task::spawn_blocking(move || {
        (target.as_str(), port).to_socket_addrs().map(|values| values.collect::<Vec<SocketAddr>>())
    })).await.map_err(|_| "DeepSeek 图片地址解析超时")?
        .map_err(|_| "DeepSeek 图片地址解析任务失败")?
        .map_err(|_| "DeepSeek 图片地址无法解析")?;
    if addresses.is_empty() || addresses.iter().any(|address| !public_ip(address.ip())) {
        return Err(super::bad_input("DeepSeek 图片 URL 不允许访问本机、内网或保留地址"));
    }
    let client = super::client_builder(proxy, gateway)?.resolve_to_addrs(&host, &addresses).build()
        .map_err(|_| "DeepSeek 图片下载客户端创建失败")?;
    let response = client.get(url).timeout(Duration::from_secs(30)).send().await
        .map_err(|_| "DeepSeek 公网图片下载失败")?;
    if !response.status().is_success() { return Err("DeepSeek 图片下载被拒绝，重定向链接请改用 base64 图片".into()); }
    if response.content_length().is_some_and(|size| size > MAX_IMAGE_BYTES as u64) {
        return Err(super::bad_input("DeepSeek 单张图片不能超过 32 MiB"));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "DeepSeek 图片下载中断")?;
        if bytes.len().saturating_add(chunk.len()) > MAX_IMAGE_BYTES {
            return Err(super::bad_input("DeepSeek 单张图片不能超过 32 MiB"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn bytes(input: &ImageInput, proxy: Option<&proto::WireProxy>, gateway: &str) -> Result<Vec<u8>, String> {
    if let Some(data) = input.source.strip_prefix("data:") {
        let (metadata, encoded) = data.split_once(',').ok_or_else(|| super::bad_input("DeepSeek 图片 data URL 无效"))?;
        if !metadata.starts_with("image/") || !metadata.split(';').skip(1).any(|part| part == "base64") {
            return Err(super::bad_input("DeepSeek 内联图片必须使用 image/*;base64 格式"));
        }
        if encoded.len() > (MAX_IMAGE_BYTES + 2) / 3 * 4 {
            return Err(super::bad_input("DeepSeek 单张图片不能超过 32 MiB"));
        }
        let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)
            .map_err(|_| super::bad_input("DeepSeek 图片 base64 无效"))?;
        if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
            return Err(super::bad_input("DeepSeek 图片为空或超过 32 MiB"));
        }
        return Ok(bytes);
    }
    download(&input.source, proxy, gateway).await
}

fn format(bytes: &[u8]) -> Result<(&'static str, &'static str), String> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") { return Ok(("image/png", "png")); }
    if bytes.starts_with(b"\xff\xd8\xff") { return Ok(("image/jpeg", "jpg")); }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") { return Ok(("image/gif", "gif")); }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") { return Ok(("image/webp", "webp")); }
    Err(super::bad_input("DeepSeek 图片请使用 PNG、JPEG、GIF 或 WebP；不会把非图片文件当作图片上传"))
}

async fn ready(client: &reqwest::Client, headers: &[(String, String)], file_id: &str) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let response = super::get(client, "/api/v0/file/fetch_files", headers, &[("file_ids", file_id)]).await?;
        let file = response.pointer("/data/biz_data/files").and_then(Value::as_array)
            .and_then(|files| files.iter().find(|file| file["id"] == file_id));
        match file.and_then(|file| file["status"].as_str()) {
            Some("SUCCESS") => return Ok(()),
            Some("PENDING" | "PARSING") | None => {}
            _ => return Err("DeepSeek 图片解析失败，请检查图片格式与内容".into()),
        }
        if tokio::time::Instant::now() >= deadline { return Err("DeepSeek 图片解析超时".into()); }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

pub async fn upload(
    images: &[ImageInput], client: &reqwest::Client, headers: &[(String, String)],
    mode: &str, thinking: bool, proxy: Option<&proto::WireProxy>, gateway: &str,
) -> Result<Vec<String>, String> {
    if images.len() > MAX_IMAGES { return Err(super::bad_input("DeepSeek 单次最多上传 50 张图片")); }
    let mut files = Vec::new();
    let mut total = 0usize;
    for (index, input) in images.iter().enumerate() {
        let bytes = bytes(input, proxy, gateway).await?;
        total = total.saturating_add(bytes.len());
        if total > MAX_TOTAL_BYTES { return Err(super::bad_input("DeepSeek 单次图片总量不能超过 64 MiB")); }
        let (mime, extension) = format(&bytes)?;
        let length = bytes.len();
        let part = reqwest::multipart::Part::bytes(bytes).file_name(format!("image-{}.{}", index + 1, extension))
            .mime_str(mime).map_err(|_| "DeepSeek 图片类型无效")?;
        let proof = super::proof(client, headers, UPLOAD_PATH).await?;
        let mut request = client.post(format!("{}{UPLOAD_PATH}", super::BASE))
            .multipart(reqwest::multipart::Form::new().part("file", part))
            .header("x-ds-pow-response", proof).header("x-model-type", mode)
            .header("x-thinking-enabled", if thinking { "1" } else { "0" })
            .header("x-file-size", length).timeout(Duration::from_secs(60));
        for (name, value) in headers {
            if !name.eq_ignore_ascii_case("content-type") { request = request.header(name, value); }
        }
        let response = request.send().await.map_err(|_| "DeepSeek 图片上传失败")?;
        let result = super::control_response(response).await?;
        let file_id = result.pointer("/data/biz_data/id").and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 256).ok_or("DeepSeek 图片上传未返回文件标识")?;
        ready(client, headers, file_id).await?;
        files.push(file_id.to_string());
    }
    Ok(files)
}
