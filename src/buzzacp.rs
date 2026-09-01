//! #200 Phase 2：buzz-acp 执行层——bridge dispatch 的 buzz 路径。
//!
//! 后端为 buzz 时不再 spawn CLI 子进程，改为把消息写入 mini-relay（kind 9），
//! buzz-acp 订阅后触发 buzz-agent session/prompt，agent 回复（kind 9）回流到
//! AgentReply 通道，由 service 的回流任务发回聊天平台。
//!
//! 进程管理：ABB service 拉起 buzz-acp（环境变量配置 relay 地址/agent 命令/
//! 身份密钥，身份装配守 I1 fail-closed，见 service::spawn_buzz_acp）。**句柄必须
//! 被长期持有**：Child 置 kill_on_drop(true)，drop 即 SIGKILL——service 的
//! mini-relay-acp 巡检任务持有句柄：崩溃（非零退出）重拉，**主动退出（exit 0）
//! 按 I5 是终态不复活**（buzz docs/remote-agents.md 五不变量）；关停随任务
//! drop 收进程。

use std::process::Stdio;

/// buzz-acp 子进程管理（service spawn，bridge 只调用 publish/subscribe）。
///
/// **杀整组而非单进程**（审查 #205r2）：kill_on_drop 只 SIGKILL 直接子进程，
/// 而 buzz-acp 按设计持有 session 池（若干常驻 buzz-agent 孙进程）——单杀 acp
/// 会留下孤儿池：旧池继续占 API 额度、并在下次 ABB 启动后与新池同时消费同一频道
/// （重复回复）。故 unix 下自建进程组（process_group(0)），Drop 时 killpg 整组
/// （同 agent.rs::kill_agent_tree 立的规矩）。
pub struct BuzzAcpProcess {
    child: tokio::process::Child,
    pid: Option<u32>,
}

/// buzz-acp 的环境变量装配（纯函数——单测守住 env 名：写错名字 = owner 门
/// fail-closed 静默全丢，这类 bug 只有对着 buzz-acp 的 clap 定义才能发现）。
///
/// 正字表（buzz-acp/src/config.rs CliArgs）：BUZZ_RELAY_URL / BUZZ_PRIVATE_KEY /
/// BUZZ_ACP_AGENT_COMMAND / BUZZ_ACP_AGENT_OWNER / BUZZ_ACP_RESPOND_TO…。
/// BUZZ_AGENT_PROVIDER 是 **buzz-agent 专属**（persona 约定，其它适配器不读）。
/// BUZZ_ACP_AGENT_ARGS 不设置：clap 默认 "acp" 会被 buzz-acp 的
/// normalize_agent_args 按命令身份纠正（goose→["acp"]，buzz-agent/codex-acp/
/// claude-agent-acp→[]），无需桥侧干预。
pub fn acp_env(
    relay_url: &str,
    private_key: &str,
    agent_command: &str,
    agent_owner: &str,
) -> Vec<(String, String)> {
    vec![
        ("BUZZ_RELAY_URL".into(), relay_url.into()),
        ("BUZZ_PRIVATE_KEY".into(), private_key.into()),
        ("BUZZ_ACP_AGENT_COMMAND".into(), agent_command.into()),
        ("BUZZ_AGENT_PROVIDER".into(), "anthropic".into()),
        // 入站作者门（buzz-acp 默认 respond-to=owner-only，owner 未配置=fail-closed
        // 丢弃一切事件，lib.rs is_owner_or_sibling）。ABB 发布的 kind-9 用户消息由
        // 桥身份签名 → owner 必须设为**桥身份公钥**，author==owner 直接短路通过。
        // 曾误写 BUZZ_AGENT_OWNER（无此正字）且传空——静默全链路失效的根因。
        ("BUZZ_ACP_AGENT_OWNER".into(), agent_owner.into()),
    ]
}

impl BuzzAcpProcess {
    /// 拉起 buzz-acp。relay_url 指向 ABB 的 mini-relay；agent_owner = 桥身份公钥
    /// （见 [`acp_env`] 的门控说明）。unix 下自建进程组，Drop 杀整组（见类型注释）。
    pub fn spawn(
        exe: &str,
        relay_url: &str,
        private_key: &str,
        agent_command: &str,
        agent_owner: &str,
    ) -> std::io::Result<Self> {
        let mut cmd = tokio::process::Command::new(exe);
        for (k, v) in acp_env(relay_url, private_key, agent_command, agent_owner) {
            cmd.env(k, v);
        }
        // 自建进程组：让 Drop 的 killpg(-pid) 能连带收掉 acp 的 buzz-agent 池。
        // Windows 无此语义（杀树需 toolhelp 快照，本仓 winproc 那套是为 CLI spawn
        // 建的），暂由 kill_on_drop 兜直接子进程——孤儿池风险记入 #206。
        #[cfg(unix)]
        cmd.process_group(0);
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let pid = child.id();
        Ok(Self { child, pid })
    }

    /// 等退出（I5 判据的载体：巡检用它替代轮询——即时、无常驻定时器唤醒）。
    /// 返回的 ExitStatus：success()=exit 0=**主动停止**（owner `!shutdown` / auto-stop
    /// 到点，按 I5 不得自动重拉）；非零或 Err = 崩溃/系统错（可重拉）。
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }
}

/// 句柄消亡即收进程组：kill_on_drop 只保证直接子进程，组内 buzz-agent 池由这里
/// 兜底（崩溃重拉与关停两条路径都经 Drop；不做 SIGTERM→SIGKILL 两段等待——
/// Drop 里阻塞 2s 会拖住关停路径，池无可保存状态，直接收组）。
impl Drop for BuzzAcpProcess {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            // SAFETY: kill(-pgid, SIGKILL) 只作用于本类型 spawn 时用 process_group(0)
            // 自建的进程组；pid 取自 Child::id()，进程已退出时 pid 可能被复用——与
            // agent.rs::kill_agent_tree 同款权衡（杀错组的前提是 pid 已被复用为新组
            // 组长，实践中窗口极小）。调用只传合法整数、无内存解引用。
            unsafe {
                let _ = libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// env 装配回归（审查 #205 后追问 buzz 配置面）：变量名必须是 buzz-acp
    /// CliArgs 的正字——`BUZZ_ACP_AGENT_OWNER`（曾误写 BUZZ_AGENT_OWNER + 空值：
    /// respond-to=owner-only 门 fail-closed，全部事件被静默丢弃）。owner 值 =
    /// 传入的桥身份公钥；relay 地址/私钥/agent 命令逐一透传。
    #[test]
    fn acp_env_uses_canonical_buzz_acp_names() {
        let env: std::collections::HashMap<String, String> =
            std::collections::HashMap::from_iter(acp_env(
                "ws://127.0.0.1:3000",
                "agent-sec",
                "/opt/bin/buzz-agent",
                "br1dg3pubkey",
            ));
        assert_eq!(env["BUZZ_RELAY_URL"], "ws://127.0.0.1:3000");
        assert_eq!(env["BUZZ_PRIVATE_KEY"], "agent-sec");
        assert_eq!(env["BUZZ_ACP_AGENT_COMMAND"], "/opt/bin/buzz-agent");
        assert_eq!(env["BUZZ_ACP_AGENT_OWNER"], "br1dg3pubkey");
        assert_eq!(env["BUZZ_AGENT_PROVIDER"], "anthropic");
        // 错名绝迹：历史上写错的 BUZZ_AGENT_OWNER 不得回流
        assert!(!env.contains_key("BUZZ_AGENT_OWNER"));
    }

    /// I5 判据通道：干净退出（exit 0）与崩溃退出（非零）必须可区分——
    /// 巡检据此决定「终态不重拉」还是「崩溃重拉」。真进程回归：
    /// 以 true/false 两个最小 harness 替身验证 exit 码判定通道。
    #[cfg(unix)]
    #[tokio::test]
    async fn wait_distinguishes_clean_and_crash_exit() {
        // true/false 的落位随发行版不同（macOS 无 /bin/true；多数 Linux 两处都有
        // 软链），按存在性解析——找不到就跳过（环境性，不红）。
        fn pick(cands: &[&'static str; 2]) -> Option<&'static str> {
            cands
                .iter()
                .copied()
                .find(|p| std::path::Path::new(p).exists())
        }
        let (Some(true_exe), Some(false_exe)) = (
            pick(&["/usr/bin/true", "/bin/true"]),
            pick(&["/usr/bin/false", "/bin/false"]),
        ) else {
            eprintln!("skip: 本机无 true/false 替身");
            return;
        };
        let mut clean = BuzzAcpProcess::spawn(true_exe, "ws://x", "k", "cmd", "owner").unwrap();
        let mut crash = BuzzAcpProcess::spawn(false_exe, "ws://x", "k", "cmd", "owner").unwrap();
        // wait() 即时返回（服务期不等退出，只在终态判定处用；加 5s 兜底防测试挂）
        let clean_status = tokio::time::timeout(std::time::Duration::from_secs(5), clean.wait())
            .await
            .expect("true 替身应退出")
            .unwrap();
        let crash_status = tokio::time::timeout(std::time::Duration::from_secs(5), crash.wait())
            .await
            .expect("false 替身应退出")
            .unwrap();
        assert!(
            clean_status.success(),
            "exit 0 必须判为主动退出（I5 终态，不自动重拉）"
        );
        assert!(!crash_status.success(), "非零退出必须判为崩溃（可重拉）");
    }
}
