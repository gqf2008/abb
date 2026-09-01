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
//! 按 I5 是终态不复活**（buzz docs/remote-agents.md 五不变量）；**关停必须调
//! graceful_stop()**（drop 只硬杀，会让 acp 来不及收自己的 agent 池与 flush relay）。

use std::process::Stdio;

/// buzz-acp 子进程管理（service spawn，bridge 只调用 publish/subscribe）。
///
/// **进程树与回收边界**（审查 #205r3 更正上一轮的失实描述）：buzz-acp 给它自己的
/// 每个 agent 池子进程也置了 `process_group(0)`（buzz `acp.rs:523`，注释明言「让
/// SIGKILL 不传到 harness 自己的组」）——所以 `kill(-acp_pid)` **收不到池**，池是
/// acp 自己在**协作式关停**里逐组收的（`AcpClient::shutdown` → `kill_process_group`
/// 每个池 pid，acp.rs:422-431），而那条路径只在它收到 **SIGTERM** 时才跑。结论：
/// - 关停必须走 [`BuzzAcpProcess::graceful_stop`]（SIGTERM → 等它自己排水收池 →
///   超时才硬杀）。**不能只靠 drop**：drop 序里 `kill_on_drop` 会立刻 SIGKILL，
///   同拍发出的 SIGTERM 根本来不及被它的 tokio 信号处理器处理。
/// - acp 崩溃/被硬杀时：池收不到 acp 的清理，只能靠 stdin EOF 自退——我们的
///   `reap` 无法代杀（不掌握池 pid），残留风险记 #206。
pub struct BuzzAcpProcess {
    child: tokio::process::Child,
    /// 仅 unix 的 graceful_stop 发信号用；Windows 下不读但保留字段（跨平台构造点
    /// 单一，不为 Windows 拆两套）。不加 cfg_attr 会挂 Windows CI：
    /// `field pid is never read` 被 -D warnings 拦（本机 macOS 门禁看不到，0a3acd9）。
    #[cfg_attr(not(unix), allow(dead_code))]
    pid: Option<u32>,
    /// stderr 有界尾巴。acp 的 clap 解析错、致命配置错、panic 全文**只**在这里——
    /// 三路 stdio 全 null 时它们统统塌成一行「退出异常」，与仓库在 CLI 路径上付过
    /// 学费的结论相反（#123：stderr 改管道透传，失败原因才可定位）。
    stderr_tail: std::sync::Arc<std::sync::Mutex<String>>,
}

/// stderr 尾巴上限（**字符**）：够装一条 panic + 若干行 clap 报错，又不无界增长。
const STDERR_TAIL_CHARS: usize = 2000;

/// 追加一段 stderr 并只保留末尾 [`STDERR_TAIL_CHARS`]。按 char 计数截断——按字节切
/// 会在多字节边界产出 U+FFFD（仓库规则：字符串处理必须 char-aware）。
fn push_tail(tail: &std::sync::Mutex<String>, chunk: &str) {
    let mut t = tail.lock().unwrap();
    t.push_str(chunk);
    t.push('\n');
    let total = t.chars().count();
    if total > STDERR_TAIL_CHARS {
        let byte_at = t
            .char_indices()
            .nth(total - STDERR_TAIL_CHARS)
            .map(|(i, _)| i)
            .unwrap_or(t.len());
        *t = t[byte_at..].to_string();
    }
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
    /// （见 [`acp_env`] 的门控说明）。
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
        // 自成进程组的实际作用（更正上一版注释）：**不是**「让我们 killpg 能连带收
        // 池」——池各在自己的组里（acp 自己置的），只能由 acp 协作式关停来收。这里
        // 置组是让 acp 成为组长，从而按 pid 精确发 SIGTERM 可达，且 ABB 收到的组信号
        // （开发期 Ctrl+C / 对 ABB 的 killpg）不会顺带误杀它。
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // 唯一保留的管道：失败诊断（见 stderr_tail 注释）。必须持续排空，
            // 否则管道写满会把 acp 卡死——由下面的读取任务负责。
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let pid = child.id();
        let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        if let Some(err) = child.stderr.take() {
            let sink = std::sync::Arc::clone(&stderr_tail);
            // 短命任务（随子进程 stderr EOF 收尾）：登记进治理，panic/指标可见（#69）
            crate::tasks::tasks().spawn("buzz-acp-stderr", async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(err).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    push_tail(&sink, &line);
                }
            });
        }
        Ok(Self {
            child,
            pid,
            stderr_tail,
        })
    }

    /// stderr 尾巴（诊断用；无输出返回空串）。崩溃日志带它，否则 clap 报错/panic
    /// 与「子进程自己退了」在日志里长得一模一样。
    pub fn stderr_tail(&self) -> String {
        self.stderr_tail.lock().unwrap().clone()
    }

    /// 等退出（I5 判据的载体：巡检用它替代轮询——即时、无常驻定时器唤醒）。
    /// 返回的 ExitStatus：success()=exit 0=**主动停止**（owner `!shutdown` / auto-stop
    /// 到点，按 I5 不得自动重拉）；非零或 Err = 崩溃/系统错（可重拉）。
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    /// 优雅关停（关停路径专用，审查 #205r3）：SIGTERM 给 acp → 给它 `grace` 走完
    /// 「flush relay 尾部事件 → 逐组收自己的 agent 池 → 退出」→ 超时才 SIGKILL 兜底。
    /// 只 drop 句柄拿不到这个效果（kill_on_drop 的同拍 SIGKILL 会让它的 SIGTERM
    /// 处理器根本没机会跑），所以关停必须显式调本方法。
    pub async fn graceful_stop(mut self, grace: std::time::Duration) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            // SAFETY: 只对本类型 spawn 出的子进程 pid 发信号（process_group(0) 使
            // acp 自成组长，池在其自己的组里、收不到也伤不到）；无内存解引用。
            unsafe {
                let _ = libc::kill(pid as i32, libc::SIGTERM);
            }
        }
        // 等它自己收干净；超时才硬杀（服务期常态等待不设总期限，这里是关停预算，
        // 有界是正确形态——见 RULE：关闭路径才允许期限）
        if tokio::time::timeout(grace, self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
        }
    }

    /// 收割已退出的句柄（崩溃重拉前）：acp 已经死了，只剩 reap 防僵尸。
    /// 注意**不要**在这里等 2s 或 killpg——上一版那么写的理由是「清整组防留池」，
    /// 实为不成立（见类型注释：池各在自己的组里，acp 崩溃时也无法代杀）。
    pub async fn reap(mut self) {
        let _ = self.child.wait().await;
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

    /// stderr 尾巴：按 char 保留末尾（多字节不得切坏），超限截断而非无界增长。
    #[test]
    fn push_tail_is_char_bounded_and_multibyte_safe() {
        let tail = std::sync::Mutex::new(String::new());
        for i in 0..400 {
            push_tail(&tail, &format!("第 {i} 行——中文与 emoji 🐛 混排"));
        }
        let t = tail.lock().unwrap().clone();
        assert!(t.chars().count() <= STDERR_TAIL_CHARS, "按 char 计不得超限");
        assert!(
            !t.contains('\u{FFFD}'),
            "多字节边界不得被切坏（char 计数而非字节）"
        );
        assert!(t.contains("399"), "保留的必须是末尾内容");
        // 空行只追加换行（按 char 计，且不得因截断丢内容字符）
        let before = tail.lock().unwrap().chars().count();
        push_tail(&tail, "");
        let after = tail.lock().unwrap().chars().count();
        assert!(after >= before, "只可能追加，不得吞内容");
        assert!(after - before <= 1, "一行空输入最多一个换行");
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
