//! `abb-agent` 可执行入口：stdio 上跑 ACP（行分隔 JSON-RPC 2.0）。
//!
//! 进程契约与 `buzz-agent` 一致（abb 的 `src/buzz/acp.rs` 是客户端）：
//! stdin 收请求/通知，stdout 出应答/通知。**stdout 只跑协议**——日志一律 stderr，
//! 否则会直接破坏帧。
//!
//! 读循环**必须保持可读**：回合由 [`Server::dispatch`] 内部派到独立任务里，
//! 这样回合进行中送进来的 `session/cancel` 才能被立刻处理（第一版这里是
//! `dispatch().await` 串行，cancel 完全失效——评审反证 (b)）。

use std::sync::Arc;

use abb_agent::acp::Server;
use abb_agent::wire::{parse_line, Reader, Writer};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    init_logging();

    // 配置错误要**响亮失败**（与被替代组件的 `die()` 同款，exit 2）：`BUZZ_AGENT_NO_HINTS`
    // 只在进程级收口「授权者看不到 owner 私有约定」，读不懂就不能拿它赌。
    let server = match Server::try_new(Writer::new(Box::new(tokio::io::stdout()))) {
        Ok(server) => Arc::new(server),
        Err(reason) => {
            tracing::error!("启动拒绝：{reason}");
            return std::process::ExitCode::from(2);
        }
    };
    let mut reader = Reader::new(Box::new(tokio::io::stdin()));

    loop {
        match reader.next_line().await {
            Ok(Some(line)) => match parse_line(&line) {
                Ok(Some(inbound)) => {
                    if let Err(error) = server.dispatch(inbound).await {
                        // 写不出去（父进程关了 stdout/管道断了）：无法再通信，收工。
                        tracing::error!("写出失败，退出：{error}");
                        return std::process::ExitCode::from(1);
                    }
                }
                // 空行 / 对端回包：按协议忽略。
                Ok(None) => {}
                // 单行畸形不该打垮整个进程——abb 能据此看到「agent 还在但拒了这行」。
                Err(error) => tracing::warn!("忽略无法解析的输入行：{error}"),
            },
            // EOF：父进程关闭 stdin ⇒ 正常收工（不是错误）。
            Ok(None) => {
                tracing::info!("stdin 已关闭，退出");
                return std::process::ExitCode::SUCCESS;
            }
            Err(error) => {
                tracing::error!("读 stdin 失败：{error}");
                return std::process::ExitCode::from(1);
            }
        }
    }
}

/// 日志只走 stderr；级别由 `RUST_LOG` 控制（默认 info）。
fn init_logging() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
}
