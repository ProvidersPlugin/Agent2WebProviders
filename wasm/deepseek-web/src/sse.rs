//! 字节级 SSE 分帧，保留 event 名与 data 空白，跨分片 UTF-8/CRLF 不提前解码。

use agent2api_wasm_sdk::{http, Error, Result};

const MAX_FRAME: usize = 1024 * 1024;

pub struct Event {
    pub name: String,
    pub data: String,
}

pub struct Reader {
    response: http::Response,
    buffer: Vec<u8>,
    eof: bool,
}

impl Reader {
    pub fn new(response: http::Response) -> Self {
        Self {
            response,
            buffer: Vec::new(),
            eof: false,
        }
    }

    pub fn next(&mut self) -> Result<Option<Event>> {
        loop {
            if let Some(frame) = take_frame(&mut self.buffer) {
                if let Some(event) = parse(&frame)? {
                    return Ok(Some(event));
                }
                continue;
            }
            if self.buffer.len() > MAX_FRAME {
                return Err(Error::http(502, "DeepSeek SSE 帧超过大小上限"));
            }
            if self.eof {
                if self.buffer.is_empty() {
                    return Ok(None);
                }
                return parse(&std::mem::take(&mut self.buffer));
            }
            match self.response.read().map_err(crate::errors::network)? {
                Some(bytes) => self.buffer.extend_from_slice(&bytes),
                None => self.eof = true,
            }
        }
    }
}

fn take_frame(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let mut start = 0;
    for at in 0..buffer.len() {
        if buffer[at] != b'\n' {
            continue;
        }
        let end = if at > start && buffer[at - 1] == b'\r' {
            at - 1
        } else {
            at
        };
        if end == start {
            let frame = buffer[..start].to_vec();
            buffer.drain(..=at);
            return Some(frame);
        }
        start = at + 1;
    }
    None
}

fn parse(frame: &[u8]) -> Result<Option<Event>> {
    if frame.len() > MAX_FRAME {
        return Err(Error::http(502, "DeepSeek SSE 帧超过大小上限"));
    }
    let text = std::str::from_utf8(frame)
        .map_err(|_| Error::http(502, "DeepSeek SSE 数据不是完整 UTF-8"))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut name = String::new();
    let mut data = Vec::new();
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if let Some(value) = line.strip_prefix("event:") {
            name = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        } else if line == "data" {
            data.push("");
        }
    }
    Ok((!name.is_empty() || !data.is_empty()).then(|| Event {
        name,
        data: data.join("\n"),
    }))
}
