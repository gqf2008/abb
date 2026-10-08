//! abb-agent —— ABB 的 ACP 执行层，由 [rpi](https://github.com/bigfish1913/pi-rust)
//! 的库实现。
//!
//! 它替代 `crates/buzz-agent`（自维护分叉，含 7.9k 行自持 LLM 层与 1.9k 行自持
//! agent loop）。本包把那一层交给 rpi：LLM 协议适配用 `rpi-ai`，agent loop 用
//! `rpi-agent`，内置工具用 `rpi-tools`。
//!
//! ## 边界（谁管什么）
//!
//! 本包自己拥有：工作目录、约定链扫描、会话存储、provider 装配、MCP 客户端、
//! ACP 帧。rpi 只提供 loop / 工具 / provider 适配 / 事件。
//!
//! 值得记一笔：rpi 库层**不管路径**——`AgentHarnessOptions` 里没有 cwd、没有目录、
//! 没有配置路径（`resources.skills` 直接收 `Vec<Skill>`，`Session` 的存储是调用方
//! 给的 trait 对象），`~/.rpi/agent` 与 `RPI_CODING_AGENT_DIR` 的解析全在 `rpi-cli`
//! 那个应用层。所以「装哪个目录」是我们自己的决定，不是要迁就的约束。

pub mod acp;
pub mod builtin;
pub mod child_env;
pub mod hints;
pub mod mcp;
pub mod provider;
pub mod wire;

/// 测试用的传输替身：把出站行捕获到 channel，避免测试依赖真实 stdout。
#[cfg(test)]
pub mod testing {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use tokio::io::AsyncWrite;
    use tokio::sync::mpsc;

    use crate::wire::Writer;

    /// 造一个把行写进 channel 的 [`Writer`]；返回的接收端按行给出写出的内容。
    pub fn capture_writer() -> (Writer, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        (Writer::new(Box::new(CaptureSink { tx })), rx)
    }

    struct CaptureSink {
        tx: mpsc::UnboundedSender<String>,
    }

    impl AsyncWrite for CaptureSink {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            // `Writer::send` 每次都是一次 `write_all`（整条消息 + '\n'），
            // 所以这里按行切分即可，不必做跨 write 的缓冲。
            let text = String::from_utf8_lossy(buf);
            for line in text.split_inclusive('\n') {
                let trimmed = line.trim_end_matches('\n');
                if !trimmed.is_empty() {
                    let _ = self.tx.send(trimmed.to_string());
                }
            }
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
}
