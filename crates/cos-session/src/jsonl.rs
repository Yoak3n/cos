//! JSONL 持久化：第一行 header，随后一行一事件（逐字节可回放）。

use std::path::Path;

use crate::error::SessionError;
use crate::session::Session;
use crate::types::{SESSION_FORMAT_VERSION, SessionEvent, SessionHeader};

/// 保存会话：`header` 一行 + 每条事件一行（父目录不存在时自动创建）。
pub fn save_jsonl(
    session: &Session,
    header: &SessionHeader,
    path: impl AsRef<Path>,
) -> Result<(), SessionError> {
    let mut text = String::new();
    text.push_str(&serde_json::to_string(header)?);
    text.push('\n');
    for event in session.events() {
        text.push_str(&serde_json::to_string(&event)?);
        text.push('\n');
    }
    if let Some(parent) = path.as_ref().parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)?;
    Ok(())
}

/// 载入会话：校验版本 → 逐行解析。
///
/// 返回 (header, events)；调用方可用 [`Session::from_events`] 恢复（`next_seq` 自动续接）。
pub fn load_jsonl(
    path: impl AsRef<Path>,
) -> Result<(SessionHeader, Vec<SessionEvent>), SessionError> {
    let text = std::fs::read_to_string(path)?;
    let mut lines = text.lines();
    let header_line = lines
        .next()
        .ok_or_else(|| SessionError::Invalid("空日志文件".into()))?;
    let header: SessionHeader = serde_json::from_str(header_line)?;
    if header.version != SESSION_FORMAT_VERSION {
        return Err(SessionError::VersionMismatch {
            expected: SESSION_FORMAT_VERSION,
            found: header.version,
        });
    }
    let mut events = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        events.push(serde_json::from_str(line)?);
    }
    Ok((header, events))
}
