//! ACP 的传输层：stdio 上的行分隔 JSON-RPC 2.0。
//!
//! 与 `crates/buzz-agent/src/wire.rs` 同构（刻意如此：这一层是纯协议，与执行层
//! 无关，搬家不引风险）。三条约定：
//!
//! 1. **一行一个 JSON 消息**，以 `\n` 分隔；stdout 只跑协议，日志一律走 stderr
//!    （混进 stdout 会直接破坏帧）。
//! 2. 入站有两种：**request**（有 `id`，必须应答）与 **notification**（无 `id`，
//!    不应答）。后者在 abb 侧是 `session/cancel`——把它当成 request 会多发一个
//!    无主应答，被 abb 的读循环当未知 id 丢掉。
//! 3. 出站写入器是共享的（回合流任务与主循环都会写），故内部加锁串行化，
//!    保证整行原子落盘、不会交错。

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("io 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("JSON 编解码错误：{0}")]
    Json(#[from] serde_json::Error),
}

/// 入站消息。
#[derive(Debug, Clone)]
pub enum Inbound {
    /// 带 `id`：必须回 `result` 或 `error`。
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// 不带 `id`：不应答（例如 `session/cancel`）。
    Notification { method: String, params: Value },
}

/// 解析一行。空行与「不是 JSON-RPC 方法调用」的行返回 `Ok(None)`：
/// ACP 的对端可能回包（有 id 无 method），本层不处理也不该报错。
pub fn parse_line(line: &str) -> Result<Option<Inbound>, WireError> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(trimmed)?;
    let Some(method) = value.get("method").and_then(Value::as_str) else {
        return Ok(None);
    };
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    Ok(Some(match value.get("id") {
        // 显式判 null：JSON-RPC 里 `"id": null` 等价于通知，不是有效请求 id。
        Some(id) if !id.is_null() => Inbound::Request {
            id: id.clone(),
            method: method.to_string(),
            params,
        },
        _ => Inbound::Notification {
            method: method.to_string(),
            params,
        },
    }))
}

/// 出站写入器（stdout 的共享句柄）。
///
/// 存 `dyn AsyncWrite`（poll 式、对象安全）而不是 `dyn AsyncWriteExt`：后者是
/// 扩展 trait（`async fn` 直挂），不能作 trait 对象。`write_all`/`flush` 仍可用，
/// 因为 tokio 的 `AsyncWriteExt` 对 `?Sized` 的 `AsyncWrite` 有 blanket impl。
#[derive(Clone)]
pub struct Writer {
    inner: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
}

impl Writer {
    pub fn new(out: Box<dyn AsyncWrite + Unpin + Send>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(out)),
        }
    }

    async fn send(&self, message: Value) -> Result<(), WireError> {
        let mut bytes = serde_json::to_vec(&message)?;
        bytes.push(b'\n');
        let mut guard = self.inner.lock().await;
        guard.write_all(&bytes).await?;
        guard.flush().await?;
        Ok(())
    }

    /// 对请求的正常应答。
    pub async fn respond(&self, id: Value, result: Value) -> Result<(), WireError> {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
            .await
    }

    /// 对请求的错误应答（JSON-RPC 约定：错误码 + message）。
    pub async fn fail(
        &self,
        id: Value,
        code: i64,
        message: impl Into<String>,
    ) -> Result<(), WireError> {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message.into() },
        }))
        .await
    }

    /// 主动通知（`session/update` 流走这里）。
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), WireError> {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }
}

/// stdin 行读取器。
pub struct Reader {
    lines: tokio::io::Lines<BufReader<Box<dyn tokio::io::AsyncRead + Unpin + Send>>>,
}

impl Reader {
    pub fn new(input: Box<dyn tokio::io::AsyncRead + Unpin + Send>) -> Self {
        Self {
            lines: BufReader::new(input).lines(),
        }
    }

    /// 读下一行；EOF 返回 `Ok(None)`（调用方据此正常退出）。
    pub async fn next_line(&mut self) -> Result<Option<String>, WireError> {
        Ok(self.lines.next_line().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_keeps_id_and_params() {
        let line =
            r#"{"jsonrpc":"2.0","id":7,"method":"session/prompt","params":{"sessionId":"s1"}}"#;
        match parse_line(line).unwrap().unwrap() {
            Inbound::Request { id, method, params } => {
                assert_eq!(id, json!(7));
                assert_eq!(method, "session/prompt");
                assert_eq!(params["sessionId"], "s1");
            }
            other => panic!("应为 Request，得到 {other:?}"),
        }
    }

    /// `session/cancel` 是通知：没有 id。若误判成 request，abb 会收到一个无主应答。
    #[test]
    fn cancel_is_a_notification() {
        let line = r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"s1"}}"#;
        assert!(matches!(
            parse_line(line).unwrap().unwrap(),
            Inbound::Notification { .. }
        ));
    }

    /// `"id": null` 按 JSON-RPC 语义等价通知，不得当成请求。
    #[test]
    fn explicit_null_id_is_a_notification() {
        let line = r#"{"jsonrpc":"2.0","id":null,"method":"session/cancel","params":{}}"#;
        assert!(matches!(
            parse_line(line).unwrap().unwrap(),
            Inbound::Notification { .. }
        ));
    }

    #[test]
    fn blank_and_non_call_lines_are_ignored() {
        assert!(parse_line("   ").unwrap().is_none());
        // 对端回包（有 id 无 method）：忽略，不报错。
        assert!(parse_line(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#)
            .unwrap()
            .is_none());
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(parse_line("{not json").is_err());
    }
}
