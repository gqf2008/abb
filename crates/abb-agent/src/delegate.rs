//! `dev__delegate`：把自包含的编码任务委派给**本机安装的** `claude` / `codex` CLI。
//!
//! 语义与上限**逐条对齐**被替代的 `crates/buzz-agent/src/devtools.rs::run_delegate`：
//! 同一件「多步编码活交给别人的 agent 跑」的事，换执行层不该换行为。
//!
//! - 后端命令行：claude `-p --output-format text --dangerously-skip-permissions -- <task>`；
//!   codex `exec --skip-git-repo-check --sandbox workspace-write -C <cwd> -- <task>`
//!   （`--` 分隔：task 以 `-` 开头或单词形（如 `review`）时不会被当成选项/子命令）；
//! - 二进制解析：`BUZZ_AGENT_DELEGATE_{CLAUDE,CODEX}_BIN` 覆盖 → PATH；
//! - 超时：默认 1200s、clamp 1..=1200；到点杀进程（unix 连同进程组）；
//! - 取消：`CancellationToken` 一取消就杀子进程；
//! - env：**白名单**（供应商 key 绝不进子进程）+ claude 的
//!   `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`；cwd = 会话工作区；
//! - 结果：`delegate(<cli>) exit: <code>` + stdout/stderr 尾部。
//!
//! **授权差异（如实登记）**：参照物在受限档（read-only / granted）**拒绝**这个工具
//! （它由 `policy.sandbox.allow_shell()` 与 shell 模式判定）；本包不实现档位，而受限会话
//! 根本不会跑到 abb-agent 上（abb 的 P2.3 硬闸拒建）⇒ 结构上满足，但没有独立闸门。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, ToolResultPartial};
use rpi_ai::types::{Schema, Tool};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::provider::EnvSource;

/// 默认超时（与参照物同值）。
pub const DELEGATE_TIMEOUT_DEFAULT: u64 = 1200;
/// 超时上界（与参照物同值）。
pub const DELEGATE_TIMEOUT_MAX: u64 = 1200;
/// 工具名（`dev__` 由 [`crate::builtin`] 的暴露层加）。
pub const DELEGATE_BARE_NAME: &str = "delegate";

/// 委派后端（只认这两个，与参照物同）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DelegateBackend {
    Claude,
    Codex,
}

impl DelegateBackend {
    pub fn from_arg(raw: &str) -> Option<Self> {
        match raw.trim() {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    pub fn cli(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// 二进制路径覆盖的环境变量名（测试缝 + 运维逃生阀，与参照物同名）。
    pub fn bin_override_env(self) -> &'static str {
        match self {
            Self::Claude => "BUZZ_AGENT_DELEGATE_CLAUDE_BIN",
            Self::Codex => "BUZZ_AGENT_DELEGATE_CODEX_BIN",
        }
    }
}

/// 参数（只在后端与 task 之间做最小的形状校验，其余交给 CLI）。
pub fn parse_timeout(raw: Option<u64>) -> u64 {
    raw.unwrap_or(DELEGATE_TIMEOUT_DEFAULT)
        .clamp(1, DELEGATE_TIMEOUT_MAX)
}

/// 后端的命令行参数（纯函数，便于单测钉住 `--` 分隔这类细节）。
pub fn args_for(backend: DelegateBackend, task: &str, cwd: &str) -> Vec<String> {
    match backend {
        DelegateBackend::Claude => vec![
            "-p".into(),
            "--output-format".into(),
            "text".into(),
            "--dangerously-skip-permissions".into(),
            "--".into(),
            task.into(),
        ],
        DelegateBackend::Codex => vec![
            "exec".into(),
            "--skip-git-repo-check".into(),
            "--sandbox".into(),
            "workspace-write".into(),
            "-C".into(),
            cwd.into(),
            "--".into(),
            task.into(),
        ],
    }
}

/// 解析委派 CLI：覆盖（必须是可执行文件）→ PATH。
pub fn delegate_cli_path(backend: DelegateBackend, env: &dyn EnvSource) -> Option<PathBuf> {
    if let Some(raw) = env.get(backend.bin_override_env()) {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            // **空串 = 显式禁用这个后端**（与参照物同：`override = Some` 即唯一来源，
            // 空串解析不出可执行文件 ⇒ 该后端不可用），**不**回落 PATH。
            return None;
        }
        let path = PathBuf::from(trimmed);
        if is_executable_file(&path) {
            return Some(path);
        }
        // **覆盖即唯一来源，绝不回落 PATH**（与参照物 `resolve_delegate_cli` 同取向：
        // 「运维要的就是确定性」——配了就该用它，找不到就如实报错，而不是偷偷换一个）。
        tracing::error!(
            "{} 指向的 {} 不可执行（按覆盖语义不再回落 PATH）",
            backend.bin_override_env(),
            trimmed
        );
        return None;
    }
    find_in_path(backend.cli(), env)
}

/// 可用的后端清单（错误信息用：让模型知道能改用哪个）。
pub fn available_backends(env: &dyn EnvSource) -> Vec<&'static str> {
    [DelegateBackend::Claude, DelegateBackend::Codex]
        .into_iter()
        .filter(|backend| delegate_cli_path(*backend, env).is_some())
        .map(|backend| backend.cli())
        .collect()
}

/// 有任意后端可用吗（用于**决定要不要暴露这个工具**，与参照物的 `cli_available` 腿同取向：
/// 两个 CLI 都没装时，工具表里不该出现一个只会报错的工具）。
pub fn any_backend_available(env: &dyn EnvSource) -> bool {
    !available_backends(env).is_empty()
}

/// 错误信息里的可用后端描述（与参照物逐字同：`claude, codex` / `none (…)`）。
pub fn available_backends_desc(env: &dyn EnvSource) -> String {
    let available = available_backends(env);
    if available.is_empty() {
        "none (neither claude nor codex found in PATH)".to_string()
    } else {
        available.join(", ")
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// PATH 里找可执行文件（Windows 上补 `.exe`）。
///
/// PATH 从**注入的环境源**读（而不是直接读进程 env）：这样「找不到 CLI」的形状可以确定性
/// 地测出来，生产侧 `ProcessEnv` 读到的仍是真 PATH。
fn find_in_path(name: &str, env: &dyn EnvSource) -> Option<PathBuf> {
    let path = env.get("PATH")?;
    for dir in std::env::split_paths(std::ffi::OsStr::new(&path)) {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

/// 每个流的**头/尾**各留多少字节（与参照物 `CappedStream` 同量级：16 KiB）。
const STREAM_HEAD_BYTES: usize = 16 * 1024;
const STREAM_TAIL_BYTES: usize = 16 * 1024;
/// 抽干管道的宽限：**每条流**各 5s（stdout/stderr 串行等 ⇒ 两条都被攥时最坏 ~10s）。
/// 超过就判定「有孙进程还攥着管道」，如实告知模型输出不完整（参照物同款 5s drain；
/// 早先这里无限等 ⇒ 一条攥着管道的孙进程能把整个回合挂死，连 `session/cancel` 都拉不回来）。
const DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

fn truncate_at_boundary(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut cut = max;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

/// 有界捕获：头 16 KiB + 尾 16 KiB，中间省略并**显式标注**省略了多少字节。
///
/// 为什么不能简单 `read_to_end` 后截断：①内存无上界（评审 F7）；②一刀切会**整段丢掉 stderr**
/// 且不留痕迹（评审 F2）。参照物对每个流都是「头+尾+省略标记」。
#[derive(Default)]
struct Capped {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    total: usize,
}

impl Capped {
    fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len();
        let head_room = STREAM_HEAD_BYTES.saturating_sub(self.head.len());
        let take = head_room.min(chunk.len());
        self.head.extend_from_slice(&chunk[..take]);
        for byte in &chunk[take..] {
            if self.tail.len() == STREAM_TAIL_BYTES {
                self.tail.pop_front();
            }
            self.tail.push_back(*byte);
        }
    }

    /// 渲染成文本（超限时带省略标记；按字符边界切）。
    fn render(&self) -> String {
        let head = String::from_utf8_lossy(&self.head).into_owned();
        let tail: String =
            String::from_utf8_lossy(&self.tail.iter().copied().collect::<Vec<u8>>()).into_owned();
        let kept = self.head.len() + self.tail.len();
        if self.total <= kept {
            return head;
        }
        let elided = self.total - kept;
        format!(
            "{}\n[... {} of {} bytes elided from delegate output ...]\n{}",
            truncate_at_boundary(&head, STREAM_HEAD_BYTES),
            elided,
            self.total,
            truncate_at_boundary(&tail, STREAM_TAIL_BYTES)
        )
    }

    fn is_empty(&self) -> bool {
        self.total == 0
    }
}

/// 读尽一个管道（**有界**），期间也认取消。
async fn read_pipe(
    pipe: Option<impl tokio::io::AsyncRead + Unpin>,
    signal: CancellationToken,
) -> Option<Capped> {
    use tokio::io::AsyncReadExt;
    let mut pipe = pipe?;
    let mut capped = Capped::default();
    let mut buf = [0u8; 8192];
    loop {
        let read = tokio::select! {
            biased;
            _ = signal.cancelled() => return None,
            result = pipe.read(&mut buf) => result,
        };
        match read {
            Ok(0) => return Some(capped),
            Ok(n) => capped.push(&buf[..n]),
            Err(_) => return Some(capped),
        }
    }
}

/// 跑一次委派（`bin` 显式传入，便于单测用假 CLI）。
pub async fn run_with_bin(
    bin: &Path,
    backend: DelegateBackend,
    task: &str,
    timeout_secs: u64,
    cwd: &Path,
    signal: CancellationToken,
) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args_for(backend, task, &cwd.to_string_lossy()));
    cmd.current_dir(cwd);
    // 白名单 env（**供应商 key 不进子进程**）——与 MCP 子进程、内置 shell 同一份表。
    crate::child_env::apply_passthrough_env(&mut cmd);
    if backend == DelegateBackend::Claude {
        // 实测必需（fork #7）：关掉 claude headless 的非必要遥测/更新流量。
        cmd.env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
    }
    // 独立进程组（unix）：取消/超时能整组收掉；Windows 抑制控制台窗。
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW（参照物 delegate 显式调 `configure_no_window`）。**只与内置 shell 腿同款**：
        // rpi-tools 的 `OsExecutionEnv` 真设这个 flag，而 abb-agent 的 MCP 腿没设（rmcp 1.8 的
        // 子进程传输不暴露 `creation_flags`）——别把两处说成一样。
        // 注：`tokio::process::Command` 在 Windows 上直接有 `creation_flags`（无需 import trait）。
        cmd.creation_flags(0x0800_0000);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = cmd
        .spawn()
        .map_err(|error| format!("delegate: 无法启动 `{}`：{error}", backend.cli()))?;
    let mut child = ChildGuard { child };

    let stdout = child.child.stdout.take();
    let stderr = child.child.stderr.take();
    let out_task = tokio::spawn(read_pipe(stdout, signal.clone()));
    let err_task = tokio::spawn(read_pipe(stderr, signal.clone()));

    let status = tokio::select! {
        biased;
        _ = signal.cancelled() => {
            child.kill().await;
            return Err("delegate: 已取消".to_string());
        }
        result = child.child.wait() => result.map_err(|error| format!("delegate: 等待 `{}` 失败：{error}", backend.cli()))?,
        _ = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)) => {
            child.kill().await;
            return Err(format!("delegate: `{}` 超时（{timeout_secs}s）", backend.cli()));
        }
    };

    // 抽干**有上界**：孙进程（脱离进程组的后台子进程）可能仍攥着管道，无限等会把回合挂死
    // 且 cancel 也无效（评审 F1：cancel 后 20s 无 stopReason）。超时就如实标注输出不完整。
    let mut lingering = false;
    let stdout = match tokio::time::timeout(DRAIN_GRACE, out_task).await {
        Ok(Ok(capped)) => capped.unwrap_or_default(),
        _ => {
            lingering = true;
            Capped::default()
        }
    };
    let stderr = match tokio::time::timeout(DRAIN_GRACE, err_task).await {
        Ok(Ok(capped)) => capped.unwrap_or_default(),
        _ => {
            lingering = true;
            Capped::default()
        }
    };

    let code = status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "killed".to_string());
    let mut text = format!("delegate({}) exit: {code}", backend.cli());
    if lingering {
        text.push_str("\n(output incomplete: a grandchild process still holds the pipe)");
    }
    if !stdout.is_empty() {
        text.push_str("\noutput:\n");
        text.push_str(&stdout.render());
    }
    if !stderr.is_empty() {
        text.push_str("\nstderr:\n");
        text.push_str(&stderr.render());
    }
    // F3：非零退出必须是**错误结果**（rpi 按 Ok/Err 推 `is_error`），否则委派失败会被当成
    // 成功工具结果回灌给模型。文本仍然完整带回（模型能看到 exit 码与输出）。
    if !status.success() {
        return Err(text);
    }
    Ok(text)
}

/// 子进程守卫：Drop 时兜底 kill（取消/超时路径已显式 kill 过）。
struct ChildGuard {
    child: tokio::process::Child,
}

impl ChildGuard {
    /// 杀进程：unix 上先按进程组整组杀（孙进程一起收），再补 `child.kill()`；
    /// Windows 上只有 `child.kill()`（与被替代组件的 non-unix 分支一致——它同样没有
    /// `taskkill /T`；进程组语义在 Windows 上由 Job Object 承担，本包未接）。
    async fn kill(&mut self) {
        #[cfg(unix)]
        {
            if let Some(pid) = self.child.id() {
                let _ = kill_group(pid as i32);
            }
        }
        let _ = self.child.kill().await;
    }
}

#[cfg(unix)]
fn kill_group(pid: i32) -> std::io::Result<()> {
    // 直接声明 libc 的 kill，避免为一个调用引依赖。
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    // SIGKILL = 9；负 pid = 整个进程组（子进程在 process_group(0) 里自成一组的组长）。
    let rc = unsafe { kill(-pid, 9) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // 尽力而为：真需要确定性收尾的路径都显式 kill 过。
        let _ = self.child.start_kill();
    }
}

/// `dev__delegate` 的 rpi 工具（schema 与参照物同形）。
pub struct DelegateTool {
    schema: Tool,
    workspace: PathBuf,
}

impl DelegateTool {
    pub fn new(workspace: &Path) -> Self {
        Self {
            schema: Tool {
                name: DELEGATE_BARE_NAME.to_string(),
                description: "Delegate a self-contained coding task to a locally installed AI \
                    coding CLI (claude or codex), run as a subprocess in the session workspace. \
                    The CLI acts as an autonomous coding agent: it can read/edit files and run \
                    commands to complete the task, then its final report is returned as this \
                    tool's output. It uses the user's own login state and subscription quota and \
                    never reads or writes their CLI config files. Cold start takes seconds and a \
                    task runs for minutes — set timeout_secs accordingly. Prefer this for \
                    multi-step coding/editing work; use dev__shell for single quick commands."
                    .to_string(),
                parameters: Schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "backend": {
                            "type": "string", "enum": ["claude", "codex"],
                            "description": "Which local coding CLI to run."
                        },
                        "task": { "type": "string", "description": "Self-contained coding task / instructions for the delegated agent." },
                        "timeout_secs": {
                            "type": "integer", "minimum": 1, "maximum": DELEGATE_TIMEOUT_MAX,
                            "description": "Hard timeout in seconds (default 1200)."
                        }
                    },
                    "required": ["backend", "task"]
                })),
                constrained_sampling: None,
            },
            workspace: workspace.to_path_buf(),
        }
    }
}

#[async_trait]
impl AgentTool for DelegateTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        DELEGATE_BARE_NAME
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let Some(backend) = params
            .get("backend")
            .and_then(Value::as_str)
            .and_then(DelegateBackend::from_arg)
        else {
            return Err(AgentError::Tool(
                "delegate: \"backend\" must be \"claude\" or \"codex\"".to_string(),
            ));
        };
        let Some(task) = params
            .get("task")
            .and_then(Value::as_str)
            .filter(|task| !task.trim().is_empty())
        else {
            return Err(AgentError::Tool(
                "delegate: missing required argument \"task\"".to_string(),
            ));
        };
        let Some(bin) = delegate_cli_path(backend, &crate::provider::ProcessEnv) else {
            return Err(AgentError::Tool(format!(
                "delegate: `{}` CLI not available; available: {}",
                backend.cli(),
                available_backends_desc(&crate::provider::ProcessEnv)
            )));
        };
        let timeout = parse_timeout(params.get("timeout_secs").and_then(Value::as_u64));
        let workspace = self.workspace.clone();
        let task = task.to_string();
        // 子进程 + 管道读：放独立任务里跑，让 agent 的取消能立刻生效（select 在函数内部）。
        let text = run_with_bin(&bin, backend, &task, timeout, &workspace, signal)
            .await
            .map_err(AgentError::Tool)?;
        Ok(AgentToolResult::text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeEnv(HashMap<String, String>);
    impl EnvSource for FakeEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    fn fake_env(pairs: &[(&str, &str)]) -> FakeEnv {
        FakeEnv(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
    }

    #[test]
    fn backend_and_timeout_parsing_matches_the_replaced_component() {
        assert_eq!(
            DelegateBackend::from_arg("claude"),
            Some(DelegateBackend::Claude)
        );
        assert_eq!(
            DelegateBackend::from_arg(" codex "),
            Some(DelegateBackend::Codex)
        );
        assert_eq!(DelegateBackend::from_arg("gemini"), None);

        assert_eq!(parse_timeout(None), DELEGATE_TIMEOUT_DEFAULT);
        assert_eq!(parse_timeout(Some(0)), 1, "0 要 clamp 到下界");
        assert_eq!(parse_timeout(Some(30)), 30);
        assert_eq!(parse_timeout(Some(99_999)), DELEGATE_TIMEOUT_MAX);
    }

    /// 命令行必须带 `--` 分隔（task 以 `-` 开头/是子命令名时不被当选项）。
    #[test]
    fn args_separate_the_task_with_double_dash() {
        let claude = args_for(DelegateBackend::Claude, "review", "/ws");
        assert_eq!(
            claude,
            vec![
                "-p",
                "--output-format",
                "text",
                "--dangerously-skip-permissions",
                "--",
                "review"
            ]
        );
        let codex = args_for(DelegateBackend::Codex, "-x", "/ws");
        assert_eq!(
            codex,
            vec![
                "exec",
                "--skip-git-repo-check",
                "--sandbox",
                "workspace-write",
                "-C",
                "/ws",
                "--",
                "-x"
            ]
        );
    }

    /// 覆盖优先、且必须是可执行文件；找不到时错误信息列出可用后端。
    #[test]
    fn cli_resolution_prefers_a_valid_override_and_reports_available_backends() {
        let dir = std::env::temp_dir().join(format!("abb-delegate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建目录");
        let fake = dir.join("fake-claude");
        std::fs::write(&fake, b"#!/bin/sh\necho ok\n").expect("写假 CLI");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake, perms).expect("chmod");
        }

        let env = fake_env(&[(
            "BUZZ_AGENT_DELEGATE_CLAUDE_BIN",
            fake.to_str().expect("path"),
        )]);
        assert_eq!(
            delegate_cli_path(DelegateBackend::Claude, &env),
            Some(fake.clone())
        );
        // 覆盖让 claude 一定可用；**不**断言「只有 claude」——PATH 里可能真装着 codex
        // （本机就有），那是环境事实而非本模块行为。可用清单的用途是错误信息给模型纠偏。
        assert!(
            available_backends(&env).contains(&"claude"),
            "有覆盖时 claude 必须在可用清单里"
        );

        // 覆盖指向不可执行路径 ⇒ **不回落 PATH**（与参照物 `resolve_delegate_cli` 同取向：
        // 「覆盖 = 唯一来源，运维要的就是确定性」）。这条曾被写成回落，评审 F4 证伪。
        let bad = fake_env(&[(
            "BUZZ_AGENT_DELEGATE_CLAUDE_BIN",
            "/definitely/not/here-claude",
        )]);
        assert_eq!(
            delegate_cli_path(DelegateBackend::Claude, &bad),
            None,
            "覆盖即唯一来源，不得偷偷回落 PATH"
        );
        // **空串覆盖 = 显式禁用**（不回落 PATH）——评审 g1：这条洞曾让「禁用」变成「回落」。
        let disabled = fake_env(&[
            ("PATH", fake.to_str().expect("path")),
            ("BUZZ_AGENT_DELEGATE_CLAUDE_BIN", "   "),
        ]);
        assert_eq!(
            delegate_cli_path(DelegateBackend::Claude, &disabled),
            None,
            "空串/纯空白覆盖 = 禁用该后端，不得回落 PATH"
        );

        // 「都没有」的形状要能确定性造出来（PATH 空/不指向任何 CLI）。
        let none = fake_env(&[
            ("PATH", "/nonexistent-dir-for-probe"),
            (
                "BUZZ_AGENT_DELEGATE_CLAUDE_BIN",
                "/definitely/not/here-claude",
            ),
            (
                "BUZZ_AGENT_DELEGATE_CODEX_BIN",
                "/definitely/not/here-codex",
            ),
        ]);
        assert_eq!(
            available_backends_desc(&none),
            "none (neither claude nor codex found in PATH)"
        );
        assert!(!any_backend_available(&none), "两个都不可用时不暴露工具");

        // PATH 里有假 CLI ⇒ 描述是逗号列表（参照物同款）。
        let dir2 = std::env::temp_dir().join(format!("abb-delegate-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir2).expect("建目录");
        let fake_codex = dir2.join("codex");
        std::fs::write(&fake_codex, b"#!/bin/sh\n").expect("写假 CLI");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake_codex).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_codex, perms).expect("chmod");
        }
        let on_path = fake_env(&[("PATH", dir2.to_str().expect("path"))]);
        assert_eq!(available_backends_desc(&on_path), "codex");
        assert!(any_backend_available(&on_path));
        std::fs::remove_dir_all(&dir2).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 用假 CLI 真跑一次：报告与退出码都要回到工具结果里。
    #[tokio::test]
    async fn runs_a_fake_cli_and_returns_its_report() {
        let dir = std::env::temp_dir().join(format!("abb-delegate-run-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建目录");
        let fake = dir.join("fake-cli");
        std::fs::write(
            &fake,
            b"#!/bin/sh\necho \"DELEGATE-REPORT-MARKER\"\necho \"args: $*\" 1>&2\nexit 0\n",
        )
        .expect("写假 CLI");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake, perms).expect("chmod");
        }

        let text = run_with_bin(
            &fake,
            DelegateBackend::Claude,
            "改这个仓库",
            30,
            &dir,
            CancellationToken::new(),
        )
        .await
        .expect("假 CLI 应能跑通");
        assert!(text.contains("delegate(claude) exit: 0"), "{text}");
        assert!(text.contains("DELEGATE-REPORT-MARKER"), "{text}");
        assert!(text.contains("stderr:"), "stderr 尾部也要带上：{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 非零退出必须是**错误结果**（rpi 按 Ok/Err 推 `is_error`；否则委派失败被当成功回灌）。
    #[tokio::test]
    async fn non_zero_exit_is_an_error_result() {
        let dir = std::env::temp_dir().join(format!("abb-delegate-fail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建目录");
        let fake = dir.join("failing-cli");
        std::fs::write(
            &fake,
            b"#!/bin/sh
echo \"BOOM-REPORT\"\nexit 3\n",
        )
        .expect("写假 CLI");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake, perms).expect("chmod");
        }
        let error = run_with_bin(
            &fake,
            DelegateBackend::Claude,
            "会失败的任务",
            30,
            &dir,
            CancellationToken::new(),
        )
        .await
        .expect_err("非零退出要报错（fail 位不能丢）");
        assert!(error.contains("exit: 3"), "{error}");
        assert!(
            error.contains("BOOM-REPORT"),
            "报告正文要随错误一起带回：{error}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **抽干有上界**：孙进程攥着管道时，工具必须在宽限（5s）后返回并**如实标注输出不完整**，
    /// 而不是把整个回合挂死（评审 F1 实测：cancel 后 20s 都没有 stopReason）。
    #[tokio::test]
    async fn grandchild_holding_the_pipe_does_not_hang_the_call() {
        let dir = std::env::temp_dir().join(format!("abb-delegate-linger-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建目录");
        let fake = dir.join("linger-cli");
        // 后台 sleep 继承 stdout ⇒ 父进程退出后管道仍被攥着。**把孙进程 PID 写下来**，
        // 收尾按 PID 杀（`pkill -f 'sleep 30'` 会连坐别人的进程——评审 g6）。
        let pid_file = dir.join("grandchild.pid");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\nsleep 30 &\necho $! > {}\necho \"parent done\"\nexit 0\n",
                pid_file.display()
            ),
        )
        .expect("写假 CLI");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake, perms).expect("chmod");
        }
        let started = std::time::Instant::now();
        let text = run_with_bin(
            &fake,
            DelegateBackend::Claude,
            "留下孙进程",
            30,
            &dir,
            CancellationToken::new(),
        )
        .await
        .expect("父进程成功退出");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "抽干必须有上界（实测 {:?}）",
            started.elapsed()
        );
        assert!(
            text.contains("output incomplete"),
            "攥着管道时要如实标注输出不完整：{text}"
        );
        // 按记录的 PID 收掉孙进程（收尾纪律：别留挂死进程，也别连坐别人的）。
        if let Ok(raw) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = raw.trim().parse::<i32>() {
                let _ = std::process::Command::new("kill")
                    .args(["-9", &pid.to_string()])
                    .status();
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 超时必须真的杀掉子进程（用一个 sleep 的假 CLI）。
    #[tokio::test]
    async fn timeout_kills_the_child() {
        let dir = std::env::temp_dir().join(format!("abb-delegate-timeout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建目录");
        let fake = dir.join("slow-cli");
        std::fs::write(&fake, b"#!/bin/sh\nsleep 30\n").expect("写假 CLI");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake, perms).expect("chmod");
        }
        let started = std::time::Instant::now();
        let error = run_with_bin(
            &fake,
            DelegateBackend::Claude,
            "慢任务",
            1,
            &dir,
            CancellationToken::new(),
        )
        .await
        .expect_err("应超时");
        assert!(error.contains("超时"), "{error}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "要真的及时退出"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
