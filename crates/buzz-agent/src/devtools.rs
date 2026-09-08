//! Built-in dev tools（shell / read / write / ls / glob）——进程内执行，不经 MCP。
//!
//! 注册在 "dev" 伪服务器命名空间下（`dev__shell` 等，由 [`crate::mcp::McpRegistry`]
//! 装配时挂进工具表）。与上游 Buzz 的 dev MCP 服务器同款安全姿态：
//!
//! - shell 子进程用白名单 env（[`crate::mcp::PASSTHROUGH_ENV`]）——供应商 key
//!   （BUZZ_AGENT_PROVIDER / OPENAI_COMPAT_* / ANTHROPIC_*）绝不进入子进程 env；
//! - `write` 限定在会话工作区（拒绝绝对路径与 `..` 逃逸），`read`/`ls`/`glob` 只读；
//! - 全部输出有界（read 256KB / shell 每流 16KB / 目录列举 500 条 / glob 500 命中）；
//! - shell 在会话 cwd 运行，硬超时（默认 120s）或回合取消时按进程组杀掉整棵子进程树。
//!
//! 权限与 MCP 工具同链路：预检 → `session/request_permission`（ABB 桥 auto-approve）
//! → 执行 → 结果回填。`BUZZ_AGENT_DEV_TOOLS=0` 关闭整套工具。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::process::Command;
use tokio::sync::watch;

use crate::types::{AgentError, ToolDef, ToolResult, ToolResultContent};

/// 会话工作区（session/new 的 cwd）——shell 的运行目录与 write 的限定根。
/// shell 之外的只读工具（read/ls/glob）允许绝对路径（与 pi/claude 对齐），
/// write 是本模块唯一有写能力的工具，强制工作区限定。
const SHELL_OUT_CAP: usize = 16 * 1024; // 单流（stdout/stderr）展示上限：头+省略标记+尾，超出中段省略
const STREAM_TAIL_BUDGET: usize = 2048; // 单流保留的末尾字节（错误摘要通常在结尾）
const STREAM_HEAD_BUDGET: usize = SHELL_OUT_CAP - STREAM_TAIL_BUDGET - 128; // 头部预算（余量给省略标记）
const SHELL_TIMEOUT_DEFAULT: u64 = 120;
const SHELL_TIMEOUT_MAX: u64 = 600;
const READ_MAX_BYTES: usize = 256 * 1024;
const READ_MAX_LINES: usize = 10_000;
const WRITE_MAX_BYTES: usize = 512 * 1024;
const LIST_MAX_ENTRIES: usize = 500;
const GLOB_MAX_HITS: usize = 500;
const GLOB_MAX_DIRS: usize = 10_000;

/// 有界管道抽干结果：内存只保留头 [`STREAM_HEAD_BUDGET`] + 尾
/// [`STREAM_TAIL_BUDGET`] 字节，其余字节计数后丢弃——子进程输出多大都不会
/// 撑爆 agent 进程（抽干本身持续到 EOF，防管道写满卡死子进程）。
struct CappedStream {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    total: u64,
}

impl CappedStream {
    fn new() -> Self {
        Self {
            head: Vec::with_capacity(STREAM_HEAD_BUDGET.min(4096)),
            tail: std::collections::VecDeque::with_capacity(STREAM_TAIL_BUDGET),
            total: 0,
        }
    }

    fn push_bytes(&mut self, mut chunk: &[u8]) {
        self.total += chunk.len() as u64;
        if self.head.len() < STREAM_HEAD_BUDGET {
            let take = (STREAM_HEAD_BUDGET - self.head.len()).min(chunk.len());
            self.head.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
        }
        for &b in chunk {
            if self.tail.len() >= STREAM_TAIL_BUDGET {
                self.tail.pop_front();
            }
            self.tail.push_back(b);
        }
    }

    /// 渲染为展示文本：未超出预算时原样返回；超出时头+省略标记+尾
    /// （与 `mcp::truncate_middle` 同构，但截断发生在抽干期而非展示期）。
    fn render(&self) -> String {
        let head = String::from_utf8_lossy(&self.head);
        let elided = self
            .total
            .saturating_sub(self.head.len() as u64 + self.tail.len() as u64);
        if elided == 0 {
            let tail: Vec<u8> = self.tail.iter().copied().collect();
            return format!("{head}{}", String::from_utf8_lossy(&tail));
        }
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        format!(
            "{head}\n[... {elided} of {} bytes elided from tool output ...]\n{}",
            self.total,
            String::from_utf8_lossy(&tail)
        )
    }
}

/// 内置工具标识（registry `Entry::Builtin` 路由用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Shell,
    Read,
    Write,
    Ls,
    Glob,
}

impl Tool {
    fn bare(self) -> &'static str {
        match self {
            Tool::Shell => "shell",
            Tool::Read => "read",
            Tool::Write => "write",
            Tool::Ls => "ls",
            Tool::Glob => "glob",
        }
    }
}

/// 全部内置工具的 `(标识, ToolDef)`。registry 装配时挂到 `dev__<bare>` 名下。
pub fn defs() -> Vec<(Tool, ToolDef)> {
    vec![
        (
            Tool::Shell,
            ToolDef {
                name: Tool::Shell.bare().to_owned(),
                description: "Run a shell command in the session workspace directory. \
                    Returns exit code, stdout and stderr (each capped). \
                    Use for git, builds, installs, and any CLI task. \
                    Long-running commands: raise timeout_secs; the default is 120s. \
                    Working directory is the session workspace."
                    .to_owned(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The shell command to run." },
                        "timeout_secs": {
                            "type": "integer", "minimum": 1, "maximum": 600,
                            "description": "Hard timeout in seconds (default 120)."
                        }
                    },
                    "required": ["command"]
                }),
            },
        ),
        (
            Tool::Read,
            ToolDef {
                name: Tool::Read.bare().to_owned(),
                description: "Read a text file. Path is absolute or relative to the session \
                    workspace. Returns the file content; binary files are rejected. \
                    Use offset/limit to read a window of lines."
                    .to_owned(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path (absolute or workspace-relative)." },
                        "offset": { "type": "integer", "minimum": 0, "description": "0-based starting line (default 0)." },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 10000, "description": "Max lines to return (default 2000)." }
                    },
                    "required": ["path"]
                }),
            },
        ),
        (
            Tool::Write,
            ToolDef {
                name: Tool::Write.bare().to_owned(),
                description: "Create or overwrite a file inside the session workspace. \
                    Path must be workspace-relative — absolute paths and '..' escapes are rejected. \
                    Parent directories are created as needed."
                    .to_owned(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Workspace-relative file path." },
                        "content": { "type": "string", "description": "Full file content to write." }
                    },
                    "required": ["path", "content"]
                }),
            },
        ),
        (
            Tool::Ls,
            ToolDef {
                name: Tool::Ls.bare().to_owned(),
                description: "List a directory's entries with kind and size. \
                    Path is absolute or relative to the session workspace (default: workspace)."
                    .to_owned(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Directory path (absolute or workspace-relative)." }
                    }
                }),
            },
        ),
        (
            Tool::Glob,
            ToolDef {
                name: Tool::Glob.bare().to_owned(),
                description: "Find files matching a glob pattern under a base directory. \
                    Supports '*', '?' and '**' (any depth). Returns workspace-relative paths. \
                    Does not follow symlinks and skips .git directories."
                    .to_owned(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Glob pattern, e.g. '**/*.rs' or 'src/**/test_*'." },
                        "path": { "type": "string", "description": "Base directory (absolute or workspace-relative; default: workspace)." }
                    },
                    "required": ["pattern"]
                }),
            },
        ),
    ]
}

/// 内置工具分发入口（registry `call` 的 Builtin 臂）。
#[cfg(test)]
pub async fn run(
    tool: Tool,
    arguments: &Value,
    cwd: &str,
    provider_id: &str,
    cancel: &mut watch::Receiver<bool>,
) -> Result<ToolResult, AgentError> {
    let policy = crate::wire::ToolPolicy::build(
        crate::wire::Sandbox::FullAccess,
        cwd,
        None,
        crate::wire::ShellMode::Full,
        None,
    );
    run_with_policy(tool, arguments, cwd, &policy, provider_id, cancel).await
}

pub async fn run_with_policy(
    tool: Tool,
    arguments: &Value,
    cwd: &str,
    policy: &crate::wire::ToolPolicy,
    provider_id: &str,
    cancel: &mut watch::Receiver<bool>,
) -> Result<ToolResult, AgentError> {
    match tool {
        Tool::Shell => run_shell(arguments, cwd, policy, provider_id, cancel).await,
        Tool::Read => run_read(arguments, cwd, policy, provider_id).await,
        Tool::Write => run_write(arguments, cwd, policy, provider_id).await,
        Tool::Ls => run_ls(arguments, cwd, policy, provider_id).await,
        Tool::Glob => run_glob(arguments, cwd, policy, provider_id).await,
    }
}

fn ok_result(provider_id: &str, text: String) -> Result<ToolResult, AgentError> {
    Ok(ToolResult {
        provider_id: provider_id.to_owned(),
        content: vec![ToolResultContent::Text(text)],
        is_error: false,
    })
}

fn error_result(provider_id: &str, msg: impl Into<String>) -> Result<ToolResult, AgentError> {
    Ok(ToolResult {
        provider_id: provider_id.to_owned(),
        content: vec![ToolResultContent::Text(msg.into())],
        is_error: true,
    })
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// 取非负整型参数。schema 声明的是 integer，但 LLM 常常把数值发成字符串
/// （"300"）或浮点（300.0）——这里宽容收下，负数/非数值仍返回 None 走默认值，
/// 避免"模型以为设了 300s、实际按 120s 默认被掐"这类静默偏差。
fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    match args.get(key) {
        Some(Value::Number(n)) => n
            .as_u64()
            .or_else(|| n.as_f64().map(|f| f.max(0.0) as u64))
            .or_else(|| n.as_i64().map(|i| i.max(0) as u64)),
        Some(Value::String(s)) => s.parse::<u64>().ok(),
        _ => None,
    }
}

// ── shell ───────────────────────────────────────────────────────────────────

async fn run_shell(
    arguments: &Value,
    cwd: &str,
    policy: &crate::wire::ToolPolicy,
    provider_id: &str,
    cancel: &mut watch::Receiver<bool>,
) -> Result<ToolResult, AgentError> {
    // P1.3b：Restricted 模式过 argv 白名单（granted 承诺语义）。拒绝以
    // is_error 工具结果回流（模型可见、可改用白名单内命令），不挂回合。
    if policy.shell == crate::wire::ShellMode::Restricted {
        let command = arguments
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match crate::shell_policy::check_restricted(
            command,
            &policy.write_roots,
            policy.abb_bin.as_deref(),
        ) {
            crate::shell_policy::Decision::Allow => {}
            crate::shell_policy::Decision::Deny(why) => {
                return ok_result(provider_id, format!("shell: 拒绝受限命令：{why}")).map(
                    |mut r| {
                        r.is_error = true;
                        r
                    },
                );
            }
        }
    }
    let command = match arg_str(arguments, "command") {
        Some(c) if !c.trim().is_empty() => c,
        _ => return error_result(provider_id, "shell: missing required argument \"command\""),
    };
    let timeout = Duration::from_secs(
        arg_u64(arguments, "timeout_secs")
            .unwrap_or(SHELL_TIMEOUT_DEFAULT)
            .clamp(1, SHELL_TIMEOUT_MAX),
    );

    let mut cmd = Command::new(shell_binary());
    cmd.args(shell_args(command));
    cmd.current_dir(cwd);
    // 白名单 env：供应商 key 等桥侧注入的敏感变量绝不进子进程（与 MCP
    // spawn_one 同一份白名单，见 mcp::apply_passthrough_env；代理/SSH/Git
    // 等工具链所需变量保留）。内置 shell 的直接子进程没有 dev-mcp 那道
    // "取 key 后先剥壳再派生"的中间层，buzz/nostr 身份私钥若放行，模型一条
    // `echo $NOSTR_PRIVATE_KEY` 就能读走——这里显式剔除。
    crate::mcp::apply_passthrough_env(&mut cmd);
    cmd.env_remove("NOSTR_PRIVATE_KEY");
    cmd.env_remove("BUZZ_PRIVATE_KEY");
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return error_result(provider_id, format!("shell: failed to start command: {e}")),
    };

    // 输出管道从 spawn 起就并发抽干（防管道缓冲写满把子进程卡死）；抽干有界
    // （见 CappedStream），输出量不占内存。超时/取消时杀进程树后管道 EOF，
    // 正常路径等进程退出后 5s 内收尾。
    let out_task = tokio::spawn(read_pipe(child.stdout.take()));
    let err_task = tokio::spawn(read_pipe(child.stderr.take()));
    // RAII 进程树守护：run_shell 的任何退出路径——包括外层 tool_timeout 或
    // 任务 abort 直接 drop 本 future——都会 SIGKILL 整个进程组并后台回收，
    // 杜绝"模型被告知超时、命令却继续在后台跑"的孤儿进程。
    let mut guard = ShellGuard::new(child);

    let wait_status = tokio::select! {
        biased;
        _ = cancel.changed() => {
            // 用户取消与 MCP 路径同语义：Err(Cancelled) 走 failed 通道，而不是
            // 返回一个 is_error 结果（后者会让历史追加"反思重试"后缀，诱导
            // 模型重跑一条用户主动中止的命令）。
            guard.kill_tree().await;
            return Err(AgentError::Cancelled);
        }
        r = guard.wait() => r,
        _ = tokio::time::sleep(timeout) => {
            // 注意：wait 臂排在 sleep 之前——子进程恰在截止时刻退出时按成功
            // 处理；只有确实没退出才走超时杀树。
            guard.kill_tree().await;
            return error_result(
                provider_id,
                format!("shell: timeout after {}s (raise timeout_secs for long tasks)", timeout.as_secs()),
            );
        }
    };
    let status = match wait_status {
        Ok(s) => s,
        Err(e) => {
            return error_result(
                provider_id,
                format!("shell: failed to wait on command: {e}"),
            )
        }
    };
    let drain = Duration::from_secs(5);
    let mut lingering = false;
    let stdout = match tokio::time::timeout(drain, out_task).await {
        Ok(Ok(s)) => s,
        _ => {
            // 孙进程（脱离进程组的后台子进程）仍持有管道：拿不到已读部分
            // （读任务还在跑，且保留有界），如实告知模型输出不完整。
            lingering = true;
            CappedStream::new()
        }
    };
    let stderr = match tokio::time::timeout(drain, err_task).await {
        Ok(Ok(s)) => s,
        _ => {
            lingering = true;
            CappedStream::new()
        }
    };

    let mut text = format!(
        "exit: {}",
        status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "killed".into())
    );
    if stdout.total > 0 {
        text.push_str(&format!("\nstdout:\n{}", stdout.render()));
    }
    if stderr.total > 0 {
        text.push_str(&format!("\nstderr:\n{}", stderr.render()));
    }
    if lingering {
        text.push_str(
            "\n[output still streaming from a lingering background child process; result truncated]\n",
        );
    }
    ok_result(provider_id, text)
}

/// 抽干一条子进程输出管道（EOF 为止），内存有界（只保留头/尾预算字节，
/// 其余计数后丢弃）。stdout/stderr 管道类型不同，泛型收。
async fn read_pipe<R>(pipe: Option<R>) -> CappedStream
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;
    let Some(mut pipe) = pipe else {
        return CappedStream::new();
    };
    let mut out = CappedStream::new();
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => out.push_bytes(&chunk[..n]),
        }
    }
    out
}

/// 子进程进程树守护：`child` 被显式杀/回收后置 `None`；Drop 时若仍持有
/// 子进程（取消/超时分支之外的异常退出路径），同步 SIGKILL 进程组（unix）
/// 并派后台任务 wait 回收，杜绝孤儿进程与僵尸。
struct ShellGuard {
    child: Option<tokio::process::Child>,
}

impl ShellGuard {
    fn new(child: tokio::process::Child) -> Self {
        Self { child: Some(child) }
    }

    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let result = self
            .child
            .as_mut()
            .expect("ShellGuard child present")
            .wait()
            .await;
        if result.is_ok() {
            self.child = None; // 已回收，Drop 不再干预
        }
        result
    }

    /// 取消/超时分支的显式杀树（杀后 wait 回收，Drop 不再重复）。
    async fn kill_tree(&mut self) {
        if let Some(mut child) = self.child.take() {
            kill_child_tree(&mut child).await;
        }
    }
}

impl Drop for ShellGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            #[cfg(unix)]
            if let Some(id) = child.id() {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(id as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            let _ = child.start_kill(); // 非 unix 兜底（直接子进程）
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
    }
}

#[cfg(unix)]
fn shell_binary() -> &'static str {
    "sh"
}
#[cfg(unix)]
fn shell_args(command: &str) -> [&str; 2] {
    ["-c", command]
}
#[cfg(not(unix))]
fn shell_binary() -> &'static str {
    "cmd"
}
#[cfg(not(unix))]
fn shell_args(command: &str) -> [&str; 2] {
    ["/C", command]
}

/// 超时/取消时杀整棵子进程树：unix 走进程组 SIGKILL（同 MCP 子进程语义），
/// 其余平台回退单进程 kill。
async fn kill_child_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(id) = child.id() {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(id as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    #[cfg(not(unix))]
    let _ = child.kill().await;
    let _ = child.wait().await;
}

// ── 路径解析 ────────────────────────────────────────────────────────────────

/// 只读工具的路径解析。FullAccess 档绝对路径原样放行（与 pi/claude 对齐——
/// 今天的行为）；受限档（read_only/workspace_write，P1.3a）canonicalize 后必须
/// 落在 read_roots 内——否则 granted/受限会话可以直接 `dev__read
/// ~/.agent-bridge/config.json` 读走明文供应商 key / `~/.ssh`（实机审计结论）。
async fn resolve_read_path(
    cwd: &str,
    path: &str,
    policy: &crate::wire::ToolPolicy,
) -> Result<PathBuf, String> {
    let raw = Path::new(path);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        Path::new(cwd).join(raw)
    };
    let canon = tokio::fs::canonicalize(&joined)
        .await
        .map_err(|e| format!("path not accessible: {path} ({e})"))?;
    if let Some(roots) = &policy.read_roots {
        if !crate::wire::ToolPolicy::within(roots, &canon) {
            return Err(format!(
                "read denied outside the session's allowed roots (sandbox {:?}): {path}",
                policy.sandbox
            ));
        }
    }
    Ok(canon)
}

/// write 专用：拒绝绝对路径，`..` 逃逸经父目录 canonicalize 后前缀校验拦截
///（父目录允许自动创建）。P1.3a：限定根从「仅 cwd」扩为 policy.write_roots
/// 任一命中（FullAccess 时 write_roots=[cwd]，与今天一致）。
async fn confined_write_path(
    cwd: &str,
    path: &str,
    policy: &crate::wire::ToolPolicy,
) -> Result<PathBuf, String> {
    let raw = Path::new(path);
    if raw.is_absolute() {
        return Err(format!(
            "write: absolute path not allowed (workspace-relative only): {path}"
        ));
    }
    let base = tokio::fs::canonicalize(cwd)
        .await
        .map_err(|e| format!("workspace not accessible: {cwd} ({e})"))?;
    let parent = raw
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| base.join(p));
    if let Some(p) = &parent {
        tokio::fs::create_dir_all(p)
            .await
            .map_err(|e| format!("write: cannot create parent dirs: {e}"))?;
    }
    let parent = tokio::fs::canonicalize(parent.unwrap_or_else(|| base.clone()))
        .await
        .map_err(|e| format!("write: parent dir not accessible: {e}"))?;
    let file_name = raw
        .file_name()
        .ok_or_else(|| format!("write: invalid path: {path}"))?;
    let target = parent.join(file_name);
    let roots = if policy.write_roots.is_empty() {
        vec![base.clone()]
    } else {
        policy.write_roots.clone()
    };
    if !crate::wire::ToolPolicy::within(&roots, &target) {
        return Err(format!(
            "write: path outside the session's writable roots (sandbox {:?}): {path}",
            policy.sandbox
        ));
    }
    Ok(target)
}

// ── read ────────────────────────────────────────────────────────────────────

async fn run_read(
    arguments: &Value,
    cwd: &str,
    policy: &crate::wire::ToolPolicy,
    provider_id: &str,
) -> Result<ToolResult, AgentError> {
    let path = match arg_str(arguments, "path") {
        Some(p) if !p.is_empty() => p,
        _ => return error_result(provider_id, "read: missing required argument \"path\""),
    };
    let offset = arg_u64(arguments, "offset").unwrap_or(0) as usize;
    let limit = arg_u64(arguments, "limit")
        .unwrap_or(2000)
        .clamp(1, READ_MAX_LINES as u64) as usize;
    let resolved = match resolve_read_path(cwd, path, policy).await {
        Ok(p) => p,
        Err(e) => return error_result(provider_id, format!("read: {e}")),
    };
    if !resolved.is_file() {
        return error_result(provider_id, format!("read: not a file: {path}"));
    }
    // 先按元数据尺寸预检：超大文件在读取前就拒绝，避免整文件读入内存
    // （tokio::fs::read 无上限，几个 GB 的文件会直接把 agent 进程打 OOM）。
    // 窗口参数（offset/limit）只对 ≤ READ_MAX_BYTES 的文件生效；大文件请用
    // shell 工具（head/sed）。
    let meta = match tokio::fs::metadata(&resolved).await {
        Ok(m) => m,
        Err(e) => return error_result(provider_id, format!("read: {e}")),
    };
    if meta.len() > READ_MAX_BYTES as u64 {
        return error_result(
            provider_id,
            format!(
                "read: file too large ({} bytes > {READ_MAX_BYTES}); use the shell tool (head/sed) to read large files",
                meta.len()
            ),
        );
    }
    // 预检与实际读取之间文件可能被并发撑大，读取仍加 take 上限兜底。
    let bytes = match read_at_most(&resolved, READ_MAX_BYTES as u64 + 1).await {
        Ok(b) => b,
        Err(e) => return error_result(provider_id, format!("read: {e}")),
    };
    if bytes.len() > READ_MAX_BYTES {
        return error_result(
            provider_id,
            format!(
                "read: file too large ({} bytes > {READ_MAX_BYTES}); use the shell tool (head/sed) to read large files",
                bytes.len()
            ),
        );
    }
    // "二进制拒绝"契约：NUL 之外还要过 UTF-8 校验——纯 CJK 的 UTF-16 文本
    // 可以完全不含 NUL（"你好" = 60 4F 7D 59），但绝不是 UTF-8 文本；放任
    // 它走 from_utf8_lossy 会把乱码当正文返回，模型据此 dev__write 回写就会
    // 把原编码毁掉。误拒（如 Latin-1 老文本）可用 shell 工具（iconv/file）兜底。
    if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
        return error_result(
            provider_id,
            format!("read: not UTF-8 text (binary or other encoding): {path}"),
        );
    }
    let text = String::from_utf8(bytes).expect("validated above");
    let mut lines = text.lines();
    let window: Vec<&str> = lines.by_ref().skip(offset).take(limit).collect();
    let total = text.lines().count();
    let mut out = String::new();
    for l in &window {
        out.push_str(l);
        out.push('\n');
    }
    if offset + window.len() < total {
        out.push_str(&format!(
            "[... {} more lines ...]\n",
            total - offset - window.len()
        ));
    }
    ok_result(provider_id, out)
}

// ── write ───────────────────────────────────────────────────────────────────

/// 有上限的文件读取：最多读 `limit` 字节。防止"预检后文件被并发写大"的
/// TOCTOU，也防 procfs 之类元数据尺寸失真导致的无界整读。
async fn read_at_most(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let f = tokio::fs::File::open(path).await?;
    let mut buf = Vec::with_capacity(limit.min(64 * 1024) as usize);
    let mut f = f.take(limit);
    f.read_to_end(&mut buf).await?;
    Ok(buf)
}

async fn run_write(
    arguments: &Value,
    cwd: &str,
    policy: &crate::wire::ToolPolicy,
    provider_id: &str,
) -> Result<ToolResult, AgentError> {
    let path = match arg_str(arguments, "path") {
        Some(p) if !p.is_empty() => p,
        _ => return error_result(provider_id, "write: missing required argument \"path\""),
    };
    let content = match arg_str(arguments, "content") {
        Some(c) => c,
        None => return error_result(provider_id, "write: missing required argument \"content\""),
    };
    if content.len() > WRITE_MAX_BYTES {
        return error_result(
            provider_id,
            format!(
                "write: content too large ({} bytes > {WRITE_MAX_BYTES})",
                content.len()
            ),
        );
    }
    let target = match confined_write_path(cwd, path, policy).await {
        Ok(t) => t,
        Err(e) => return error_result(provider_id, e),
    };
    // 最终组件若是符号链接，tokio::fs::write 会跟随链接写到工作区外
    // （confined_write_path 只 canonicalize 了父目录）。这里显式拒绝。
    match tokio::fs::symlink_metadata(&target).await {
        Ok(m) if m.file_type().is_symlink() => {
            return error_result(
                provider_id,
                format!("write: refusing to follow symlink outside the workspace: {path}"),
            );
        }
        _ => {} // 不存在（正常新建）或非链接（正常覆盖）
    }
    match tokio::fs::write(&target, content).await {
        Ok(()) => ok_result(
            provider_id,
            format!("wrote {} bytes to {}", content.len(), path),
        ),
        Err(e) => error_result(provider_id, format!("write: {e}")),
    }
}

// ── ls ──────────────────────────────────────────────────────────────────────

async fn run_ls(
    arguments: &Value,
    cwd: &str,
    policy: &crate::wire::ToolPolicy,
    provider_id: &str,
) -> Result<ToolResult, AgentError> {
    let path = arg_str(arguments, "path").unwrap_or(cwd);
    let resolved = match resolve_read_path(cwd, path, policy).await {
        Ok(p) => p,
        Err(e) => return error_result(provider_id, format!("ls: {e}")),
    };
    if !resolved.is_dir() {
        return error_result(provider_id, format!("ls: not a directory: {path}"));
    }
    let mut rd = match tokio::fs::read_dir(&resolved).await {
        Ok(rd) => rd,
        Err(e) => return error_result(provider_id, format!("ls: {e}")),
    };
    let mut entries: Vec<(String, u64, bool)> = Vec::new();
    let mut truncated = false;
    let mut read_failed = false;
    loop {
        let entry = match rd.next_entry().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(_) => {
                // 中途读取出错（权限/挂载消失）：列表不完整，必须告知模型，
                // 否则残缺列表会被当成完整目录。
                read_failed = true;
                break;
            }
        };
        if entries.len() >= LIST_MAX_ENTRIES {
            truncated = true;
            break;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let (size, is_dir) = match entry.metadata().await {
            Ok(m) => (m.len(), m.is_dir()),
            // 悬空符号链接等条目元数据不可读：显式标注，避免伪装成 0 字节文件
            Err(_) => {
                entries.push((format!("{name} [unreadable]"), 0, false));
                continue;
            }
        };
        entries.push((name, size, is_dir));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::new();
    for (name, size, is_dir) in entries {
        if is_dir {
            out.push_str(&format!("{name}/\n"));
        } else {
            out.push_str(&format!("{name} ({size} bytes)\n"));
        }
    }
    // 省略标记排在真实条目之后渲染，不参与排序（否则 '[' 开头会按字母序
    // 混进列表中间，看起来像个真实文件）。
    if truncated {
        out.push_str("[... more entries elided ...]\n");
    }
    if read_failed {
        out.push_str("[... directory read error mid-listing: entries may be missing ...]\n");
    }
    ok_result(provider_id, out)
}

// ── glob ────────────────────────────────────────────────────────────────────

/// 段级 `*`/`?` 匹配（不含 `/`；`**` 由 [`match_segments`] 处理）。
/// 复用 config 里同一套全串匹配器（crate 内唯一实现），此处只包一层语义名。
fn wild_match(pat: &str, s: &str) -> bool {
    crate::config::glob_matches(pat, s)
}

/// 路径段级匹配：`**` 匹配零个或多个段。
fn match_segments(pat: &[&str], name: &[&str]) -> bool {
    if pat.is_empty() {
        return name.is_empty();
    }
    if pat[0] == "**" {
        if match_segments(&pat[1..], name) {
            return true;
        }
        if name.is_empty() {
            return false;
        }
        return match_segments(pat, &name[1..]);
    }
    if name.is_empty() {
        return false;
    }
    wild_match(pat[0], name[0]) && match_segments(&pat[1..], &name[1..])
}

async fn run_glob(
    arguments: &Value,
    cwd: &str,
    policy: &crate::wire::ToolPolicy,
    provider_id: &str,
) -> Result<ToolResult, AgentError> {
    let pattern = match arg_str(arguments, "pattern") {
        Some(p) if !p.is_empty() => p,
        _ => return error_result(provider_id, "glob: missing required argument \"pattern\""),
    };
    let base_str = arg_str(arguments, "path").unwrap_or(cwd);
    let base = match resolve_read_path(cwd, base_str, policy).await {
        Ok(p) => p,
        Err(e) => return error_result(provider_id, format!("glob: {e}")),
    };
    if !base.is_dir() {
        return error_result(
            provider_id,
            format!("glob: base not a directory: {base_str}"),
        );
    }
    // 归一化：剥掉空段与 "."（"./**/*.rs"、"a//b"、尾部 "/" 都是模型的常见
    // 写法，按通用 glob 语义与去掉这些段的写法等价）。否则这些段会与真实
    // 路径分量逐字比较而永远无法命中，却返回看似可信的 "no matches"。
    let pat_segs: Vec<&str> = pattern
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let mut hits: Vec<String> = Vec::new();
    let mut dirs_visited = 0usize;
    let (truncated, dirs_unreadable) = walk(
        &base,
        &mut Vec::new(),
        &pat_segs,
        &mut hits,
        &mut dirs_visited,
    )
    .await;
    hits.sort();
    let mut out = String::new();
    for h in hits {
        out.push_str(&h);
        out.push('\n');
    }
    // 命中数或遍历目录数触及上限时如实标注，避免模型把截断结果当成完整
    // 枚举（与 run_ls 的省略标记同语义）。
    if truncated {
        out.push_str("[... more matches elided (glob cap reached) ...]\n");
    }
    if dirs_unreadable > 0 {
        out.push_str(&format!(
            "[... {dirs_unreadable} director(ies) could not be read: results may be incomplete ...]\n"
        ));
    }
    if out.is_empty() {
        return ok_result(provider_id, "no matches".to_string());
    }
    ok_result(provider_id, out)
}

/// 递归遍历（不跟随符号链接目录、跳过 `.git`），命中上限 [`GLOB_MAX_HITS`]、
/// 遍历目录上限 [`GLOB_MAX_DIRS`]。命中超上限后继续遍历但不入列（目录上限
/// 保证遍历成本有界）。返回 `(是否截断, 读不出来的目录数)`——两者都需要
/// 调用方在结果尾部标注，残缺枚举与完整枚举必须可区分。相对路径用 `/`
/// 拼接，与平台无关。递归点 Box::pin。
fn walk<'a>(
    dir: &'a Path,
    rel: &'a mut [String],
    pat: &'a [&str],
    hits: &'a mut Vec<String>,
    dirs_visited: &'a mut usize,
) -> impl std::future::Future<Output = (bool, usize)> + 'a {
    async move {
        if *dirs_visited >= GLOB_MAX_DIRS {
            return (true, 0); // 目录上限：未遍历的子树里可能还有命中
        }
        *dirs_visited += 1;
        let mut rd = match tokio::fs::read_dir(dir).await {
            Ok(rd) => rd,
            // 权限拒绝/挂载消失：此子树整个缺失，必须上报让模型知道结果不完整
            Err(_) => return (false, 1),
        };
        let mut truncated = false;
        let mut errored = 0usize;
        loop {
            let entry = match rd.next_entry().await {
                Ok(Some(e)) => e,
                Ok(None) => break,
                Err(_) => {
                    errored += 1;
                    break;
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                if name == ".git" {
                    continue;
                }
                // 目录是否命中（`ls -d` 语义：glob 也报目录）
                let mut child = rel.to_vec();
                child.push(name.clone());
                let full: Vec<&str> = child.iter().map(String::as_str).collect();
                if match_segments(pat, &full) && !push_hit(hits, full.join("/")) {
                    truncated = true;
                }
                let entry_path = entry.path();
                if tokio::fs::symlink_metadata(&entry_path)
                    .await
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(true)
                {
                    continue; // 符号链接目录不递归（防环）
                }
                let (t, e) = Box::pin(walk(&entry_path, &mut child, pat, hits, dirs_visited)).await;
                truncated |= t;
                errored += e;
            } else {
                let mut full: Vec<&str> = rel.iter().map(String::as_str).collect();
                full.push(&name);
                if match_segments(pat, &full) && !push_hit(hits, full.join("/")) {
                    truncated = true;
                }
            }
        }
        (truncated, errored)
    }
}

/// 命中入列：达到 [`GLOB_MAX_HITS`] 后不再入列并返回 false（调用方记截断）。
fn push_hit(hits: &mut Vec<String>, hit: String) -> bool {
    if hits.len() < GLOB_MAX_HITS {
        hits.push(hit);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 独立临时目录（tempfile 已在本 crate dev-dependencies：自动唯一命名，
    /// Drop 时自动清理——断言 panic 也不会在 /tmp 留下 abb-devtools-* 残留）。
    #[cfg(unix)]
    #[tokio::test]
    async fn restricted_policy_denies_reads_outside_roots() {
        // P1.3a：workspace_write/read_only 档读域必须挡在 roots 外——granted 经
        // dev__read 读 ~/.ssh、config.json（明文 key）的洞就此封死。
        let dir = test_dir();
        let cwd = dir.path().to_str().unwrap();
        let policy = crate::wire::ToolPolicy::build(
            crate::wire::Sandbox::WorkspaceWrite,
            cwd,
            None,
            crate::wire::ShellMode::Full,
            None,
        );
        let mut rx = watch::channel(false).1;
        let r = run_with_policy(
            Tool::Read,
            &json!({ "path": "/etc/hosts" }),
            cwd,
            &policy,
            "p",
            &mut rx,
        )
        .await
        .unwrap();
        assert!(r.is_error, "{:?}", r.text());
        assert!(r.text().contains("allowed roots"), "{}", r.text());
        // FullAccess 档保持今天：绝对路径可读（回归锁）
        let full = crate::wire::ToolPolicy::build(
            crate::wire::Sandbox::FullAccess,
            cwd,
            None,
            crate::wire::ShellMode::Full,
            None,
        );
        let ok = run_with_policy(
            Tool::Read,
            &json!({ "path": "/etc/hosts" }),
            cwd,
            &full,
            "p",
            &mut rx,
        )
        .await
        .unwrap();
        assert!(!ok.is_error);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escape_still_denied_after_canonicalize() {
        // canonicalize 之后才比对 → 工作区内软链指向域外文件同样拒绝（读侧）
        let dir = test_dir();
        std::fs::write(dir.path().join("secret.txt"), "top").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret.txt"), dir.path().join("link.txt"))
            .unwrap();
        let sub = dir.path().join("ws");
        std::fs::create_dir_all(&sub).unwrap();
        let subp = sub.to_str().unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret.txt"), sub.join("link.txt")).unwrap();
        let policy = crate::wire::ToolPolicy::build(
            crate::wire::Sandbox::ReadOnly,
            subp,
            None,
            crate::wire::ShellMode::Full,
            None,
        );
        let mut rx = watch::channel(false).1;
        let r = run_with_policy(
            Tool::Read,
            &json!({ "path": "link.txt" }),
            subp,
            &policy,
            "p",
            &mut rx,
        )
        .await
        .unwrap();
        assert!(r.is_error, "symlink 指向域外必须拒绝: {}", r.text());
        // 正向对照：ReadOnly 档读 root 内普通文件必须成功——否则上面那条"拒绝"
        // 只是"什么都拒"，测不出越域判断本身。
        std::fs::write(sub.join("plain.txt"), "inside").unwrap();
        let inside = run_with_policy(
            Tool::Read,
            &json!({ "path": "plain.txt" }),
            subp,
            &policy,
            "p",
            &mut rx,
        )
        .await
        .unwrap();
        assert!(!inside.is_error, "root 内文件被误拒: {}", inside.text());
    }

    #[tokio::test]
    async fn restricted_shell_whitelist_allows_and_denies() {
        // P1.3b：Restricted 模式——白名单内放行、外拒绝（is_error 回流可自纠）
        let dir = test_dir();
        let cwd = dir.path().to_str().unwrap();
        let policy = crate::wire::ToolPolicy::build(
            crate::wire::Sandbox::WorkspaceWrite,
            cwd,
            None,
            crate::wire::ShellMode::Restricted,
            Some("/usr/bin/agent-bridge".into()),
        );
        // sender 必须存活：drop 即广播「取消」，shell 回合会 Cancelled
        let (_tx, mut rx) = watch::channel(false);
        let ok = run_with_policy(
            Tool::Shell,
            &json!({ "command": "echo whitelist-ok" }),
            cwd,
            &policy,
            "p",
            &mut rx,
        )
        .await
        .unwrap();
        assert!(!ok.is_error, "{}", ok.text());
        assert!(ok.text().contains("whitelist-ok"), "{}", ok.text());
        let denied = run_with_policy(
            Tool::Shell,
            &json!({ "command": "rm -rf /tmp/x" }),
            cwd,
            &policy,
            "p",
            &mut rx,
        )
        .await
        .unwrap();
        assert!(denied.is_error, "rm 必须拒绝: {}", denied.text());
        assert!(denied.text().contains("受限白名单"), "{}", denied.text());
        // Full 模式回归锁：任意命令照跑（今天行为）
        let full = crate::wire::ToolPolicy::build(
            crate::wire::Sandbox::FullAccess,
            cwd,
            None,
            crate::wire::ShellMode::Full,
            None,
        );
        let free = run_with_policy(
            Tool::Shell,
            &json!({ "command": "echo free-ok" }),
            cwd,
            &full,
            "p",
            &mut rx,
        )
        .await
        .unwrap();
        assert!(!free.is_error);
    }

    /// FullAccess 测试策略（与今天字节级一致：读不限、写限 cwd）。
    fn test_policy(cwd: &str) -> crate::wire::ToolPolicy {
        crate::wire::ToolPolicy::build(
            crate::wire::Sandbox::FullAccess,
            cwd,
            None,
            crate::wire::ShellMode::Full,
            None,
        )
    }

    fn test_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn wild_match_basics() {
        assert!(wild_match("*", "foo.rs"));
        assert!(wild_match("*.rs", "foo.rs"));
        assert!(!wild_match("*.rs", "foo.txt"));
        assert!(wild_match("test_*", "test_a"));
        assert!(wild_match("a?c", "abc"));
        assert!(!wild_match("a?c", "ac"));
        // 裸 matcher 的 `*` 可跨 `/`（段切分是 match_segments 的职责）
        assert!(wild_match("*.rs", "dir/foo.rs"));
    }

    #[test]
    fn segment_match_double_star() {
        let p = ["**", "*.rs"];
        assert!(match_segments(&p, &["foo.rs"]));
        assert!(match_segments(&p, &["a", "b", "foo.rs"]));
        assert!(!match_segments(&p, &["a", "foo.txt"]));
        let q = ["src", "**", "test_*"];
        assert!(match_segments(&q, &["src", "test_x"]));
        assert!(match_segments(&q, &["src", "a", "b", "test_y"]));
        assert!(!match_segments(&q, &["lib", "test_x"]));
    }

    #[tokio::test]
    async fn write_confinement_rejects_escape() {
        let tmp = test_dir();
        let cwd = tmp.path().to_str().unwrap();
        // 绝对路径拒绝
        let err = confined_write_path(cwd, "/etc/passwd", &test_policy(cwd))
            .await
            .unwrap_err();
        assert!(err.contains("absolute path"), "{err}");
        // `..` 逃逸拒绝
        let err = confined_write_path(cwd, "../escape.txt", &test_policy(cwd))
            .await
            .unwrap_err();
        // P1.3a 措辞：逃逸被「roots 前缀校验」拦截（同一语义换文案）
        assert!(err.contains("writable roots"), "{err}");
        // 正常相对路径落在工作区内（tmp 先 canonicalize：macOS /var → /private/var）
        let ok = confined_write_path(cwd, "sub/dir/a.txt", &test_policy(cwd))
            .await
            .unwrap();
        let canon_tmp = tokio::fs::canonicalize(tmp.path()).await.unwrap();
        assert!(ok.starts_with(&canon_tmp), "{ok:?} not under {canon_tmp:?}");
    }

    #[tokio::test]
    async fn write_read_roundtrip() {
        let tmp = test_dir();
        let cwd = tmp.path().to_str().unwrap();
        let (_, wdef) = defs().into_iter().find(|(t, _)| *t == Tool::Write).unwrap();
        assert_eq!(wdef.name, "write");
        let w = run(
            Tool::Write,
            &json!({ "path": "notes/hello.md", "content": "第一行\n第二行\n" }),
            cwd,
            "p",
            &mut watch::channel(false).1,
        )
        .await
        .unwrap();
        assert!(!w.is_error);
        let r = run(
            Tool::Read,
            &json!({ "path": "notes/hello.md", "offset": 1 }),
            cwd,
            "p",
            &mut watch::channel(false).1,
        )
        .await
        .unwrap();
        assert!(r.text().contains("第二行"));
        assert!(!r.text().contains("第一行"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_runs_and_caps_output() {
        let tmp = test_dir();
        let cwd = tmp.path().to_str().unwrap();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let mut rx = cancel_rx;
        let r = run(
            Tool::Shell,
            &json!({ "command": "echo hello; echo err >&2; exit 3" }),
            cwd,
            "p",
            &mut rx,
        )
        .await
        .unwrap();
        assert!(r.text().contains("exit: 3"), "{}", r.text());
        assert!(r.text().contains("hello"));
        assert!(r.text().contains("err"));
    }

    #[tokio::test]
    async fn ls_lists_dir() {
        let tmp = test_dir();
        std::fs::write(tmp.path().join("b.txt"), "x").unwrap();
        std::fs::create_dir_all(tmp.path().join("a_dir")).unwrap();
        let r = run(
            Tool::Ls,
            &json!({ "path": tmp.path().to_str().unwrap() }),
            tmp.path().to_str().unwrap(),
            "p",
            &mut watch::channel(false).1,
        )
        .await
        .unwrap();
        assert!(r.text().contains("a_dir/"));
        assert!(r.text().contains("b.txt"));
    }

    #[tokio::test]
    async fn glob_finds_matches_skips_git() {
        let tmp = test_dir();
        std::fs::create_dir_all(tmp.path().join("src/deep")).unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join("src/a.rs"), "x").unwrap();
        std::fs::write(tmp.path().join("src/deep/b.rs"), "x").unwrap();
        std::fs::write(tmp.path().join(".git/config"), "x").unwrap();
        let r = run(
            Tool::Glob,
            &json!({ "pattern": "src/**/*.rs" }),
            tmp.path().to_str().unwrap(),
            "p",
            &mut watch::channel(false).1,
        )
        .await
        .unwrap();
        assert!(r.text().contains("src/a.rs"), "{}", r.text());
        assert!(r.text().contains("src/deep/b.rs"), "{}", r.text());
        assert!(!r.text().contains(".git"), "{}", r.text());
    }

    #[tokio::test]
    async fn glob_marks_truncated_hits() {
        let tmp = test_dir();
        for i in 0..(GLOB_MAX_HITS + 5) {
            std::fs::write(tmp.path().join(format!("f{i:04}.txt")), "x").unwrap();
        }
        let r = run(
            Tool::Glob,
            &json!({ "pattern": "*.txt" }),
            tmp.path().to_str().unwrap(),
            "p",
            &mut watch::channel(false).1,
        )
        .await
        .unwrap();
        let text = r.text();
        assert!(text.contains("[... more matches elided"), "{}", text);
        assert_eq!(text.lines().count(), GLOB_MAX_HITS + 1, "500 hits + marker");
    }

    #[test]
    fn capped_stream_bounds_memory_and_marks_elision() {
        // 输出远超预算：内存只保留头/尾预算，渲染带省略标记
        let mut s = CappedStream::new();
        let big = vec![b'x'; STREAM_HEAD_BUDGET + STREAM_TAIL_BUDGET + 5000];
        for chunk in big.chunks(4096) {
            s.push_bytes(chunk);
        }
        assert_eq!(s.total as usize, big.len());
        assert!(s.head.len() <= STREAM_HEAD_BUDGET, "head bounded");
        assert!(s.tail.len() <= STREAM_TAIL_BUDGET, "tail bounded");
        let rendered = s.render();
        assert!(rendered.contains("bytes elided"), "{rendered}");
        // 小输出不出现省略标记
        let mut small = CappedStream::new();
        small.push_bytes(b"hello world");
        let small_text = small.render();
        assert!(!small_text.contains("elided"));
        assert!(small_text.contains("hello world"));
    }

    #[tokio::test]
    async fn read_rejects_oversized_file_without_slurp() {
        let tmp = test_dir();
        let cwd = tmp.path().to_str().unwrap();
        std::fs::write(tmp.path().join("big.log"), vec![b'a'; READ_MAX_BYTES + 1]).unwrap();
        let r = run(
            Tool::Read,
            &json!({ "path": "big.log" }),
            cwd,
            "p",
            &mut watch::channel(false).1,
        )
        .await
        .unwrap();
        assert!(r.is_error);
        assert!(
            r.text().contains("use the shell tool"),
            "honest guidance expected, got: {}",
            r.text()
        );
        assert!(
            !r.text().contains("use offset/limit"),
            "impossible advice gone"
        );
    }

    #[tokio::test]
    async fn glob_normalizes_dot_and_empty_segments() {
        let tmp = test_dir();
        std::fs::write(tmp.path().join("a.rs"), "x").unwrap();
        std::fs::create_dir_all(tmp.path().join("src/deep")).unwrap();
        std::fs::write(tmp.path().join("src/deep/b.rs"), "x").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        // "./" 前缀、双斜杠、尾部斜杠都是模型常见写法：归一化后必须命中
        for pattern in ["./**/*.rs", "src//deep/*.rs/", "./*.rs"] {
            let r = run(
                Tool::Glob,
                &json!({ "pattern": pattern }),
                cwd,
                "p",
                &mut watch::channel(false).1,
            )
            .await
            .unwrap();
            assert!(!r.is_error, "{pattern}: {}", r.text());
            assert!(
                !r.text().contains("no matches"),
                "{pattern}: expected matches, got {}",
                r.text()
            );
        }
    }

    #[tokio::test]
    async fn read_rejects_non_utf8_text() {
        let tmp = test_dir();
        let cwd = tmp.path().to_str().unwrap();
        // UTF-16LE（BOM + "你好" = FF FE 60 4F 7D 59）不含 NUL 字节，NUL 嗅探
        // 拦不住；整体 UTF-8 校验才能按"非 UTF-8 文本"拒绝，而不是 lossy 乱码回传。
        std::fs::write(
            tmp.path().join("utf16.txt"),
            [0xFF, 0xFE, 0x60, 0x4F, 0x7D, 0x59],
        )
        .unwrap();
        let r = run(
            Tool::Read,
            &json!({ "path": "utf16.txt" }),
            cwd,
            "p",
            &mut watch::channel(false).1,
        )
        .await
        .unwrap();
        assert!(r.is_error, "expected rejection, got {}", r.text());
        assert!(r.text().contains("not UTF-8 text"), "{}", r.text());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_cancel_returns_cancelled_error() {
        let tmp = test_dir();
        let cwd = tmp.path().to_str().unwrap().to_owned();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let mut rx = cancel_rx;
        let spawned = tokio::spawn(async move {
            run(
                Tool::Shell,
                &json!({ "command": "sleep 30", "timeout_secs": 60 }),
                &cwd,
                "p",
                &mut rx,
            )
            .await
        });
        // 等 sleep 真正起来再取消
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = cancel_tx.send(true);
        let result = tokio::time::timeout(Duration::from_secs(10), spawned)
            .await
            .expect("cancel must return promptly");
        match result {
            Ok(Err(AgentError::Cancelled)) => {}
            other => panic!("expected Err(Cancelled), got {other:?}"),
        }
    }

    #[test]
    fn defs_names_unique_and_required() {
        let mut seen = std::collections::HashSet::new();
        for (t, d) in defs() {
            assert!(seen.insert(d.name.clone()), "duplicate {}", d.name);
            assert_eq!(d.name, t.bare());
            let schema = &d.input_schema;
            assert_eq!(schema["type"], "object");
        }
    }
}
