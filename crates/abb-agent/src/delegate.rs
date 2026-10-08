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
        if !trimmed.is_empty() {
            let path = PathBuf::from(trimmed);
            if is_executable_file(&path) {
                return Some(path);
            }
            // 覆盖指错不静默回落：记 ERROR 后仍按 PATH 找（与参照物同取向）。
            tracing::error!(
                "{} 指向的 {} 不可执行，回落 PATH 查找",
                backend.bin_override_env(),
                trimmed
            );
        }
    }
    find_in_path(backend.cli())
}

/// 可用的后端清单（错误信息用：让模型知道能改用哪个）。
pub fn available_backends(env: &dyn EnvSource) -> Vec<&'static str> {
    [DelegateBackend::Claude, DelegateBackend::Codex]
        .into_iter()
        .filter(|backend| delegate_cli_path(*backend, env).is_some())
        .map(|backend| backend.cli())
        .collect()
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
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
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

/// 输出上限（与工具结果预算同量级：报告是文本，不需要无限大）。
const MAX_OUTPUT_BYTES: usize = 128 * 1024;

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
    // 孙进程控制台窗抑制（Windows）+ 独立进程组（unix）：取消时能整组收掉。
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = cmd
        .spawn()
        .map_err(|error| format!("delegate: 无法启动 `{}`：{error}", backend.cli()))?;
    let mut child = ChildGuard { child };

    let stdout = child.child.stdout.take();
    let stderr = child.child.stderr.take();
    let out_task = tokio::spawn(read_pipe(stdout));
    let err_task = tokio::spawn(read_pipe(stderr));

    let status = tokio::select! {
        biased;
        _ = signal.cancelled() => {
            child.kill().await;
            return Err("delegate: 已取消".to_string());
        }
        result = child.child.wait() => result.map_err(|error| format!("delegate: 等待 `{}` 失败：{error}", backend.cli()))?,
        _ = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)) => {
            child.kill().await;
            return Err(format!(
                "delegate: `{}` 超时（{timeout_secs}s）",
                backend.cli()
            ));
        }
    };

    let stdout = out_task.await.unwrap_or_default();
    let stderr = err_task.await.unwrap_or_default();
    let code = status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "killed".to_string());
    let mut text = format!("delegate({}) exit: {code}", backend.cli());
    if !stdout.is_empty() {
        text.push_str("\noutput:\n");
        text.push_str(&stdout);
    }
    if !stderr.is_empty() {
        text.push_str("\nstderr:\n");
        text.push_str(&stderr);
    }
    Ok(truncate_at_boundary(&text, MAX_OUTPUT_BYTES).to_string())
}

/// 子进程守卫：Drop 时兜底 kill（取消/超时路径已显式 kill 过）。
struct ChildGuard {
    child: tokio::process::Child,
}

impl ChildGuard {
    /// 杀进程（unix 上连同进程组；Windows 上 taskkill /T）。
    async fn kill(&mut self) {
        #[cfg(unix)]
        {
            if let Some(pid) = self.child.id() {
                // 与 MCP/shell 同款：负 pid = 整个进程组。
                let _ = nix_kill_group(pid as i32);
            }
        }
        let _ = self.child.kill().await;
    }
}

#[cfg(unix)]
fn nix_kill_group(pid: i32) -> std::io::Result<()> {
    // 直接用 libc 的 kill，避免为一个调用引依赖。
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    // SIGKILL = 9
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

/// 读尽一个管道（有界），超限就截断并标注。
async fn read_pipe(pipe: Option<impl tokio::io::AsyncRead + Unpin>) -> String {
    use tokio::io::AsyncReadExt;
    let Some(mut pipe) = pipe else {
        return String::new();
    };
    let mut buf = Vec::new();
    let _ = pipe.read_to_end(&mut buf).await;
    let text = String::from_utf8_lossy(&buf).into_owned();
    truncate_at_boundary(&text, MAX_OUTPUT_BYTES).to_string()
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
                "delegate: `{}` CLI not available; available: {:?}",
                backend.cli(),
                available_backends(&crate::provider::ProcessEnv)
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

        // 覆盖指向不可执行路径 ⇒ 回落 PATH（未必找得到，但不得 panic）。
        let bad = fake_env(&[(
            "BUZZ_AGENT_DELEGATE_CLAUDE_BIN",
            "/definitely/not/here-claude",
        )]);
        let resolved = delegate_cli_path(DelegateBackend::Claude, &bad);
        assert!(resolved.is_none() || resolved.expect("可选").is_file());
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
