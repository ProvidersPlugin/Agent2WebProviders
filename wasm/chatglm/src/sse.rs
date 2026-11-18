//! SSE 数据行缓冲沿用旧状态机语义；保留事件名与完整 UTF-8 帧，EOF 刷新残留。

use agent2api_wasm_sdk::{Error, Result};

pub struct Event {
    pub name: String,
    pub data: String,
}

#[derive(Default)]
pub struct Decoder {
    line: Vec<u8>,
    data: Vec<u8>,
    event: String,
    skip_lf: bool,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Event>> {
        let mut output = Vec::new();
        for byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if *byte == b'\n' {
                    continue;
                }
            }
            if matches!(*byte, b'\r' | b'\n') {
                self.line(&mut output)?;
                self.skip_lf = *byte == b'\r';
            } else {
                if self.line.len() + self.data.len() + self.event.len() >= 1024 * 1024 {
                    return Err(Error::http(502, "智谱单条流式事件超过大小限制"));
                }
                self.line.push(*byte);
            }
        }
        Ok(output)
    }

    fn line(&mut self, output: &mut Vec<Event>) -> Result<()> {
        if self.line.is_empty() {
            if !self.data.is_empty() || !self.event.is_empty() {
                output.push(Event {
                    name: std::mem::take(&mut self.event),
                    data: String::from_utf8(std::mem::take(&mut self.data))
                        .map_err(|_| Error::http(502, "智谱响应不是有效 UTF-8"))?,
                });
            }
        } else if let Some(data) = self.line.strip_prefix(b"data:") {
            if !self.data.is_empty() {
                self.data.push(b'\n');
            }
            self.data
                .extend_from_slice(data.strip_prefix(b" ").unwrap_or(data));
        } else if let Some(event) = self.line.strip_prefix(b"event:") {
            self.event = std::str::from_utf8(event.strip_prefix(b" ").unwrap_or(event))
                .map_err(|_| Error::http(502, "智谱事件名不是有效 UTF-8"))?
                .to_string();
        }
        self.line.clear();
        Ok(())
    }

    pub fn finish(&mut self) -> Result<Vec<Event>> {
        let mut output = Vec::new();
        self.line(&mut output)?;
        self.line(&mut output)?;
        Ok(output)
    }
}
