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

use crate::mcp::truncate_middle;
use crate::types::{AgentError, ToolDef, ToolResult, ToolResultContent};

/// 会话工作区（session/new 的 cwd）——shell 的运行目录与 write 的限定根。
/// shell 之外的只读工具（read/ls/glob）允许绝对路径（与 pi/claude 对齐），
/// write 是本模块唯一有写能力的工具，强制工作区限定。
const SHELL_OUT_CAP: usize = 16 * 1024; // 单流（stdout/stderr）字节上限，超出中段省略
const SHELL_TIMEOUT_DEFAULT: u64 = 120;
const SHELL_TIMEOUT_MAX: u64 = 600;
const READ_MAX_BYTES: usize = 256 * 1024;
const READ_MAX_LINES: usize = 10_000;
const WRITE_MAX_BYTES: usize = 512 * 1024;
const LIST_MAX_ENTRIES: usize = 500;
const GLOB_MAX_HITS: usize = 500;
const GLOB_MAX_DIRS: usize = 10_000;

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
pub async fn run(
    tool: Tool,
    arguments: &Value,
    cwd: &str,
    provider_id: &str,
    cancel: &mut watch::Receiver<bool>,
) -> Result<ToolResult, AgentError> {
    match tool {
        Tool::Shell => run_shell(arguments, cwd, provider_id, cancel).await,
        Tool::Read => run_read(arguments, cwd, provider_id).await,
        Tool::Write => run_write(arguments, cwd, provider_id).await,
        Tool::Ls => run_ls(arguments, cwd, provider_id).await,
        Tool::Glob => run_glob(arguments, cwd, provider_id).await,
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

fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

// ── shell ───────────────────────────────────────────────────────────────────

async fn run_shell(
    arguments: &Value,
    cwd: &str,
    provider_id: &str,
    cancel: &mut watch::Receiver<bool>,
) -> Result<ToolResult, AgentError> {
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
    // spawn_one 同源；代理/SSH/Git 等工具链所需变量保留）。
    cmd.env_clear();
    for k in crate::mcp::PASSTHROUGH_ENV {
        if let Ok(v) = std::env::var(k) {
            cmd.env(k, v);
        }
    }
    #[cfg(windows)]
    for k in crate::mcp::windows_child_passthrough_env() {
        if let Ok(v) = std::env::var(k) {
            cmd.env(k, v);
        }
    }
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return error_result(provider_id, format!("shell: failed to start command: {e}")),
    };

    // 输出管道从 spawn 起就并发抽干（防管道缓冲写满把子进程卡死）；超时/取消
    // 时杀进程树后管道 EOF，正常路径等进程退出后 5s 内收尾。
    let out_task = tokio::spawn(read_pipe(child.stdout.take()));
    let err_task = tokio::spawn(read_pipe(child.stderr.take()));

    let wait_status = tokio::select! {
        biased;
        _ = cancel.changed() => {
            kill_child_tree(&mut child).await;
            return error_result(provider_id, "shell: cancelled");
        }
        _ = tokio::time::sleep(timeout) => {
            kill_child_tree(&mut child).await;
            return error_result(
                provider_id,
                format!("shell: timeout after {}s (raise timeout_secs for long tasks)", timeout.as_secs()),
            );
        }
        r = child.wait() => r,
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
    let stdout = match tokio::time::timeout(drain, out_task).await {
        Ok(Ok(b)) => b,
        _ => Vec::new(), // 孙进程仍持有管道：拿已读到的部分（超时丢弃）
    };
    let stderr = match tokio::time::timeout(drain, err_task).await {
        Ok(Ok(b)) => b,
        _ => Vec::new(),
    };

    let mut text = format!(
        "exit: {}",
        status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "killed".into())
    );
    if !stdout.is_empty() {
        let s = String::from_utf8_lossy(&stdout).into_owned();
        text.push_str(&format!(
            "\nstdout:\n{}",
            truncate_middle(&s, SHELL_OUT_CAP)
        ));
    }
    if !stderr.is_empty() {
        let s = String::from_utf8_lossy(&stderr).into_owned();
        text.push_str(&format!(
            "\nstderr:\n{}",
            truncate_middle(&s, SHELL_OUT_CAP)
        ));
    }
    ok_result(provider_id, text)
}

/// 抽干一条子进程输出管道（EOF 为止）。stdout/stderr 管道类型不同，泛型收。
async fn read_pipe<R>(pipe: Option<R>) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;
    let Some(mut pipe) = pipe else {
        return Vec::new();
    };
    let mut buf = Vec::new();
    let _ = pipe.read_to_end(&mut buf).await;
    buf
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

/// 只读工具的路径解析：相对路径基于会话工作区，绝对路径原样放行（与
/// pi/claude 对齐——只读无写风险）。返回规范化（canonicalize 后）的路径。
async fn resolve_read_path(cwd: &str, path: &str) -> Result<PathBuf, String> {
    let raw = Path::new(path);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        Path::new(cwd).join(raw)
    };
    tokio::fs::canonicalize(&joined)
        .await
        .map_err(|e| format!("path not accessible: {path} ({e})"))
}

/// write 专用：拒绝绝对路径，`..` 逃逸经父目录 canonicalize 后前缀校验拦截
///（父目录允许自动创建）。返回值保证在 `cwd` 子树内。
async fn confined_write_path(cwd: &str, path: &str) -> Result<PathBuf, String> {
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
    if !target.starts_with(&base) {
        return Err(format!(
            "write: path escapes the workspace (.. traversal rejected): {path}"
        ));
    }
    Ok(target)
}

// ── read ────────────────────────────────────────────────────────────────────

async fn run_read(
    arguments: &Value,
    cwd: &str,
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
    let resolved = match resolve_read_path(cwd, path).await {
        Ok(p) => p,
        Err(e) => return error_result(provider_id, format!("read: {e}")),
    };
    if !resolved.is_file() {
        return error_result(provider_id, format!("read: not a file: {path}"));
    }
    let bytes = match tokio::fs::read(&resolved).await {
        Ok(b) => b,
        Err(e) => return error_result(provider_id, format!("read: {e}")),
    };
    if bytes.len() > READ_MAX_BYTES {
        return error_result(
            provider_id,
            format!(
                "read: file too large ({} bytes > {READ_MAX_BYTES}); use offset/limit or shell tools (head/sed)",
                bytes.len()
            ),
        );
    }
    if bytes.contains(&0) {
        return error_result(provider_id, format!("read: binary file: {path}"));
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
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

async fn run_write(
    arguments: &Value,
    cwd: &str,
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
    let target = match confined_write_path(cwd, path).await {
        Ok(t) => t,
        Err(e) => return error_result(provider_id, e),
    };
    match tokio::fs::write(&target, content).await {
        Ok(()) => ok_result(
            provider_id,
            format!("wrote {} bytes to {}", content.len(), path),
        ),
        Err(e) => error_result(provider_id, format!("write: {e}")),
    }
}

// ── ls ──────────────────────────────────────────────────────────────────────

async fn run_ls(arguments: &Value, cwd: &str, provider_id: &str) -> Result<ToolResult, AgentError> {
    let path = arg_str(arguments, "path").unwrap_or(cwd);
    let resolved = match resolve_read_path(cwd, path).await {
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
    while let Ok(Some(entry)) = rd.next_entry().await {
        if entries.len() >= LIST_MAX_ENTRIES {
            entries.push(("[... more entries elided ...]".to_string(), 0, false));
            break;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let (size, is_dir) = match entry.metadata().await {
            Ok(m) => (m.len(), m.is_dir()),
            Err(_) => (0, false),
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
    ok_result(provider_id, out)
}

// ── glob ────────────────────────────────────────────────────────────────────

/// 段级 `*`/`?` 匹配（不含 `/`；`**` 由 [`match_segments`] 处理）。
fn wild_match(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = s.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
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
    provider_id: &str,
) -> Result<ToolResult, AgentError> {
    let pattern = match arg_str(arguments, "pattern") {
        Some(p) if !p.is_empty() => p,
        _ => return error_result(provider_id, "glob: missing required argument \"pattern\""),
    };
    let base_str = arg_str(arguments, "path").unwrap_or(cwd);
    let base = match resolve_read_path(cwd, base_str).await {
        Ok(p) => p,
        Err(e) => return error_result(provider_id, format!("glob: {e}")),
    };
    if !base.is_dir() {
        return error_result(
            provider_id,
            format!("glob: base not a directory: {base_str}"),
        );
    }
    let pat_segs: Vec<&str> = pattern.split('/').collect();
    let mut hits: Vec<String> = Vec::new();
    let mut dirs_visited = 0usize;
    walk(
        &base,
        &mut Vec::new(),
        &pat_segs,
        &mut hits,
        &mut dirs_visited,
    )
    .await;
    hits.sort();
    if hits.is_empty() {
        return ok_result(provider_id, "no matches".to_string());
    }
    let mut out = String::new();
    for h in hits {
        out.push_str(&h);
        out.push('\n');
    }
    ok_result(provider_id, out)
}

/// 递归遍历（不跟随符号链接目录、跳过 `.git`），命中上限 [`GLOB_MAX_HITS`]。
/// 相对路径用 `/` 拼接，与平台无关。递归点 Box::pin（async 递归需要装箱）。
fn walk<'a>(
    dir: &'a Path,
    rel: &'a mut [String],
    pat: &'a [&str],
    hits: &'a mut Vec<String>,
    dirs_visited: &'a mut usize,
) -> impl std::future::Future<Output = ()> + 'a {
    async move {
        if hits.len() >= GLOB_MAX_HITS || *dirs_visited >= GLOB_MAX_DIRS {
            return;
        }
        *dirs_visited += 1;
        let mut rd = match tokio::fs::read_dir(dir).await {
            Ok(rd) => rd,
            Err(_) => return,
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            if hits.len() >= GLOB_MAX_HITS {
                return;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel_segs: Vec<&str> = rel.iter().map(String::as_str).collect();
            let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                if name == ".git" {
                    continue;
                }
                // 目录是否命中（`ls -d` 语义：glob 也报目录）
                let mut child = rel.to_vec();
                child.push(name.clone());
                let full: Vec<&str> = child.iter().map(String::as_str).collect();
                if match_segments(pat, &full) {
                    hits.push(full.join("/"));
                }
                let entry_path = entry.path();
                if tokio::fs::symlink_metadata(&entry_path)
                    .await
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(true)
                {
                    continue; // 符号链接目录不递归（防环）
                }
                Box::pin(walk(&entry_path, &mut child, pat, hits, dirs_visited)).await;
            } else {
                let mut full = rel_segs;
                full.push(&name);
                if match_segments(pat, &full) {
                    hits.push(full.join("/"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每次调用的独立临时目录（进程 id + getrandom 熵，无 fastrand 依赖）。
    fn test_dir() -> PathBuf {
        let mut buf = [0u8; 4];
        let _ = getrandom::fill(&mut buf);
        std::env::temp_dir().join(format!(
            "abb-devtools-{}-{}",
            std::process::id(),
            u32::from_le_bytes(buf)
        ))
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
        std::fs::create_dir_all(&tmp).unwrap();
        // 绝对路径拒绝
        let err = confined_write_path(tmp.to_str().unwrap(), "/etc/passwd")
            .await
            .unwrap_err();
        assert!(err.contains("absolute path"), "{err}");
        // `..` 逃逸拒绝
        let err = confined_write_path(tmp.to_str().unwrap(), "../escape.txt")
            .await
            .unwrap_err();
        assert!(err.contains("escapes"), "{err}");
        // 正常相对路径落在工作区内（tmp 先 canonicalize：macOS /var → /private/var）
        let ok = confined_write_path(tmp.to_str().unwrap(), "sub/dir/a.txt")
            .await
            .unwrap();
        let canon_tmp = tokio::fs::canonicalize(&tmp).await.unwrap();
        assert!(ok.starts_with(&canon_tmp), "{ok:?} not under {canon_tmp:?}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn write_read_roundtrip() {
        let tmp = test_dir();
        std::fs::create_dir_all(&tmp).unwrap();
        let cwd = tmp.to_str().unwrap();
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
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_runs_and_caps_output() {
        let tmp = test_dir();
        std::fs::create_dir_all(&tmp).unwrap();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let mut rx = cancel_rx;
        let r = run(
            Tool::Shell,
            &json!({ "command": "echo hello; echo err >&2; exit 3" }),
            tmp.to_str().unwrap(),
            "p",
            &mut rx,
        )
        .await
        .unwrap();
        assert!(r.text().contains("exit: 3"), "{}", r.text());
        assert!(r.text().contains("hello"));
        assert!(r.text().contains("err"));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn ls_lists_dir() {
        let tmp = test_dir();
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("b.txt"), "x").unwrap();
        std::fs::create_dir_all(tmp.join("a_dir")).unwrap();
        let r = run(
            Tool::Ls,
            &json!({ "path": tmp.to_str().unwrap() }),
            tmp.to_str().unwrap(),
            "p",
            &mut watch::channel(false).1,
        )
        .await
        .unwrap();
        assert!(r.text().contains("a_dir/"));
        assert!(r.text().contains("b.txt"));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn glob_finds_matches_skips_git() {
        let tmp = test_dir();
        std::fs::create_dir_all(tmp.join("src/deep")).unwrap();
        std::fs::create_dir_all(tmp.join(".git")).unwrap();
        std::fs::write(tmp.join("src/a.rs"), "x").unwrap();
        std::fs::write(tmp.join("src/deep/b.rs"), "x").unwrap();
        std::fs::write(tmp.join(".git/config"), "x").unwrap();
        let r = run(
            Tool::Glob,
            &json!({ "pattern": "src/**/*.rs" }),
            tmp.to_str().unwrap(),
            "p",
            &mut watch::channel(false).1,
        )
        .await
        .unwrap();
        assert!(r.text().contains("src/a.rs"), "{}", r.text());
        assert!(r.text().contains("src/deep/b.rs"), "{}", r.text());
        assert!(!r.text().contains(".git"), "{}", r.text());
        std::fs::remove_dir_all(&tmp).ok();
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
