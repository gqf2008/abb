//! #200 Phase 2：buzz-acp 执行层——bridge dispatch 的 buzz 路径。
//!
//! 后端为 buzz 时不再 spawn CLI 子进程，改为把消息写入 mini-relay（kind 9），
//! buzz-acp 订阅后触发 buzz-agent session/prompt，agent 回复（kind 9）回流到
//! AgentReply 通道，由 service 的回流任务发回聊天平台。
//!
//! 进程管理：ABB service 拉起 buzz-acp（环境变量配置 relay 地址/agent 命令/
//! 身份密钥）。**句柄必须被长期持有**：Child 置 kill_on_drop(true)，drop 即
//! SIGKILL——service 的 mini-relay-acp 巡检任务持有句柄并负责崩溃重拉，关停
//! 时随任务 drop 收进程。

use std::process::Stdio;

/// buzz-acp 子进程管理（service spawn，bridge 只调用 publish/subscribe）。
pub struct BuzzAcpProcess {
    child: tokio::process::Child,
}

impl BuzzAcpProcess {
    /// 拉起 buzz-acp。relay_url 指向 ABB 的 mini-relay 端口。
    pub fn spawn(
        exe: &str,
        relay_url: &str,
        private_key: &str,
        agent_command: &str,
        agent_owner: &str,
    ) -> std::io::Result<Self> {
        let child = tokio::process::Command::new(exe)
            .env("BUZZ_RELAY_URL", relay_url)
            .env("BUZZ_PRIVATE_KEY", private_key)
            .env("BUZZ_ACP_AGENT_COMMAND", agent_command)
            .env("BUZZ_AGENT_PROVIDER", "anthropic")
            .env("BUZZ_AGENT_OWNER", agent_owner)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        Ok(Self { child })
    }

    /// 进程是否存活。
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}
