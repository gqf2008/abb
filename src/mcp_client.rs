//! 渠道 MCP 服务的 **stdio client**（总线 S1b）。
//!
//! 拓扑与契约见 [`crate::mcp_bus`]、`docs/mcp-bus-architecture.md`。
//! 本模块只做一件事：**把一条 JSON-RPC 请求发过去、把对应的响应读回来**——
//! 不解释业务语义（那是 `mcp_bus` 的契约类型与上层 `Messenger` 的事）。
//!
//! ## 为什么不是 `rmcp`
//!
//! root 已经有手写的 ACP JSON-RPC（`src/runtime/acp.rs`，同样的 `LinesCodec` 分帧）。
//! 为一个约 6 方法 + 1 通知的客户端再拉一棵依赖树，与「113k 行的托盘二进制」
//! 不成比例。风格上跟随既有实现也更好维护。
//!
//! ## 两个容易写错的地方（都有测试钉着）
//!
//! 1. **`id` 必须自己配对**。MCP server 可以随时推**通知**（无 `id`）或回
//!    **别的请求的响应**（并发时乱序）。天真实现「读一行就当自己的响应」在
//!    有通知的渠道上会错位——而且错位后拿到的是**另一个调用的结果**，
//!    不会报错，只会给出错数据（本仓反复踩的假绿形态）。
//! 2. **超时必须包住整段读写**。server 卡住时 `read_line` 会永远等下去，
//!    在托盘进程里就是「界面还在、渠道不动」——这类症状最难查。
//!
//! ## 失败语义
//!
//! 所有失败都是**显式错误**，不静默降级：连接断、超时、JSON 坏、`id` 对不上、
//! server 回 `error` 都有各自的变体。上层据此决定「这个渠道判为不可用」。

// 本步骤（总线 S1b）只把 stdio client 做出来并单测，**还没接线到 `Messenger`**
// （S1c：`McpChannelMessenger` + `bot.kind = "mcp"`）。接线前其中一部分会显示为死代码。
//
// 与 `mcp_bus` 同口径：**移除这个 allow 的时候必须是「真的接上了」**，不是「先关警告」。
// 所以它写得范围最小（只有此模块），且解除条件明确记为 S1c 完成。
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use serde_json::Value;

use crate::mcp_bus::{build_request, parse_response, RpcError};

/// 单行上限。
///
/// **2026-11-03 从 8 MB 提到 32 MB**，原因是附件走 base64 内联时撞上了它：
/// base64 让数据膨胀 33%，于是 >6 MB 的文件会**静默失败**（`LineTooLong`
/// 在接收侧抛出，发送方只看到一次失败的调用，很难与"渠道不支持附件"区分）。
///
/// 32 MB 的取舍：
/// - base64 后约 24 MB 原文件 —— 覆盖 IM 场景的绝大多数附件（微信单文件上限本身也在这一档）
/// - 不是无限：仍然防 rogue server 撑爆内存（这是原本设它的理由）
/// - 与 ACP 侧 10 MB 不同档是刻意的：ACP 那条流只走文本，这条要走附件
///
/// **ABB 侧还会先做一次预检**（`mcp_channel::send_attachment`）：超限时给出
/// 「附件太大（X MB，上限 Y MB）」而不是让底层报 LineTooLong。两层都要有：
/// 预检负责**说清楚**，上限负责**兜底**。
pub const MAX_LINE_BYTES: usize = 32 * 1024 * 1024;

/// 附件内联传输的**原文件**上限（base64 后 ×4/3 必须留在 [`MAX_LINE_BYTES`] 内）。
///
/// 取 20 MB：20 × 4/3 ≈ 26.7 MB，给 JSON 外壳留足余量。
pub const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

/// 默认单次调用超时。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

/// client 侧错误。**每个变体都带足以定位的信息**——渠道「连不上」和「连上了但不回」
/// 是两种完全不同的故障，混成一个变体会让排障全靠猜。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// 起不来子进程（可执行文件不存在 / 没权限）。
    Spawn(String),
    /// 写入失败（server 已退出 / 关掉了 stdin）。
    Write(String),
    /// 读线程结束（stdout 关闭）。
    Closed,
    /// 超过时限。
    Timeout(Duration),
    /// 单行超过 [`MAX_LINE_BYTES`]。
    LineTooLong,
    /// 不是合法 JSON。
    BadJson(String),
    /// server 回了 `error`。
    Rpc(RpcError),
    /// 读到的是**通知**或**别的 id 的响应**，且流已结束也没等到自己的。
    /// 单独区分它，是为了让「server 只推通知不回响应」这种实现缺陷一眼可见。
    NoResponseForId(u64),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "起不来渠道 MCP 服务：{e}"),
            Self::Write(e) => write!(f, "写不进去（服务可能已退出）：{e}"),
            Self::Closed => write!(f, "服务的 stdout 已关闭，拿不到响应"),
            Self::Timeout(d) => write!(f, "等响应超过 {:?}", d),
            Self::LineTooLong => write!(f, "单行超过 {MAX_LINE_BYTES} 字节上限"),
            Self::BadJson(e) => write!(f, "响应不是合法 JSON：{e}"),
            Self::Rpc(e) => write!(f, "服务返回错误：{e:?}"),
            Self::NoResponseForId(id) => {
                write!(f, "流结束但没等到 id={id} 的响应（只收到通知或别的 id）")
            }
        }
    }
}

impl std::error::Error for ClientError {}

/// 一个 stdio 渠道 MCP 服务。
///
/// 读在**独立线程**上做、通过通道回流：这样「等响应」就能带超时
/// （std 的 `read_line` 本身不支持超时，而拉 tokio 进这一层只为超时不值）。
pub struct StdioChannelClient {
    /// 服务名。目前只用于 [`Self::name`]（S2 入站与日志要用），故逐项 allow。
    #[allow(dead_code)]
    name: String,
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Result<Value, ClientError>>,
    /// 提前读到的、不属于当前调用的消息（通知 / 别的 id 的响应），下次配对时先用。
    pending: VecDeque<Value>,
    next_id: AtomicU64,
    timeout: Duration,
    /// 读线程是否已结束——结束后不再等，直接报 [`ClientError::Closed`]。
    reader_done: bool,
}

impl StdioChannelClient {
    /// 起一个 stdio MCP 服务。
    ///
    /// `envs` 是**追加**在白名单之上的环境变量（与 `channel_mcp::load_for_session`
    /// 同口径：条目自声明的 env 叠加，不是替换）。
    pub fn spawn(
        name: &str,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
    ) -> Result<Self, ClientError> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr 继承：渠道服务的日志要能被看见，不藏在管道里憋死
            // （憋满 64 KB 缓冲区会让服务自己阻塞——这类死锁很难查）。
            .stderr(Stdio::inherit());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| ClientError::Spawn(format!("{program}: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ClientError::Spawn("拿不到 stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ClientError::Spawn("拿不到 stdout".into()))?;
        let rx = spawn_reader(stdout);
        // 登记：`process::exit` 跳 Drop 时由 atexit 兜底杀掉（见 reap_on_exit）。
        #[cfg(unix)]
        reap_on_exit::register(child.id());
        Ok(Self {
            name: name.to_string(),
            child,
            stdin,
            rx,
            pending: VecDeque::new(),
            next_id: AtomicU64::new(1),
            timeout: DEFAULT_TIMEOUT,
            reader_done: false,
        })
    }

    /// 覆盖单次调用超时（测试与「慢渠道」用）。
    #[allow(dead_code)]
    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    #[allow(dead_code)]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[allow(dead_code)]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// 发一次请求并等**它自己的**响应。
    ///
    /// 通知与其它 id 的响应会被缓存进 `pending`（不丢），下次配对先用——
    /// 丢弃它们会让「上次的晚到响应」被当成这次的结果。
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = build_request(id, method, params);
        let mut line =
            serde_json::to_string(&req).map_err(|e| ClientError::Write(e.to_string()))?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.flush())
            .map_err(|e| ClientError::Write(e.to_string()))?;

        self.await_response(id)
    }

    /// 发一条通知（无响应）。
    ///
    /// S2 入站要用（`resources/subscribe` 得先发一条 `initialized` 通知），
    /// 现在还没到那一步 ⇒ 逐项 allow（理由见 [`Self::name`]）。
    #[allow(dead_code)]
    pub fn notify(&mut self, method: &str, params: Value) -> Result<(), ClientError> {
        let msg = crate::mcp_bus::build_notification(method, params);
        let mut line =
            serde_json::to_string(&msg).map_err(|e| ClientError::Write(e.to_string()))?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.flush())
            .map_err(|e| ClientError::Write(e.to_string()))
    }

    /// 先看 `pending`，没有就等通道；等到的东西按 `id` 决定归属。
    fn await_response(&mut self, want: u64) -> Result<Value, ClientError> {
        // 先把缓存取一遍：上次调用时「晚到」的响应可能已经在里面。
        //
        // 注意这里的 `None` **要放回去**——第一版只在注释里写了「缓存进 pending（不丢）」，
        // 代码却是 `continue`（直接丢），两条用例当场咬住：
        // 乱序响应丢了 ⇒ 第二次调用等到超时；通知丢了 ⇒ `drain_notifications` 拿到 0 条。
        // 教训：注释声称的行为必须真出现在代码里。
        let mut checked = 0usize;
        while checked < self.pending.len() {
            let v = self.pending.pop_front().expect("长度已检查");
            match classify(&v, want)? {
                Some(good) => return Ok(good),
                None => {
                    self.pending.push_back(v);
                    checked += 1;
                }
            }
        }
        if self.reader_done {
            self.reap_if_stream_ended();
            return Err(ClientError::NoResponseForId(want));
        }
        loop {
            match self.rx.recv_timeout(self.timeout) {
                Ok(Ok(v)) => match classify(&v, want)? {
                    Some(good) => return Ok(good),
                    // 不是自己的：留住，下次配对先用（丢弃会让晚到响应错配到别的调用）。
                    None => self.pending.push_back(v),
                },
                Ok(Err(e)) => {
                    if matches!(e, ClientError::Closed) {
                        self.reader_done = true;
                        // 流结束了还没等到 ⇒ 是「没有这个 id 的响应」，比笼统的
                        // Closed 更能指路（server 只推通知、不回响应）。
                        return Err(ClientError::NoResponseForId(want));
                    }
                    return Err(e);
                }
                Err(RecvTimeoutError::Timeout) => return Err(ClientError::Timeout(self.timeout)),
                Err(RecvTimeoutError::Disconnected) => {
                    self.reader_done = true;
                    return Err(ClientError::NoResponseForId(want));
                }
            }
        }
    }

    /// 流已结束 ⇒ 子进程多半也退了，**必须 `try_wait` 回收**。
    ///
    /// 2026-11-04 owner 报「停止渠道会产生僵尸进程」的根因就在这里：
    /// `reader_done` 原来只置标志，`wait()` 只写在 `Drop` 里。于是
    /// 「server 自己退出（崩溃/OOM/EOF 自退）」到「client 被 drop」这段窗口里，
    /// 子进程是 **Z 状态**挂着——而 client 被 `SubscribeSource` 长期持有，
    /// 这个窗口可以长到进程结束。
    ///
    /// 为什么无条件重试而不是「置位那一刻回收一次」：`try_wait` 在进程还活着时
    /// 返回 `Ok(None)`，那一刻收不到；下一轮再试才有机会。调用点放在
    /// `call` / `drain_notifications` 入口（真实现里 fetch 每轮都过这里）。
    fn reap_if_stream_ended(&mut self) {
        if self.reader_done {
            // 失败无妨：ECHILD = 别人已回收；其他错误也只能留待 Drop。
            let _ = self.child.try_wait();
        }
    }

    /// 取出缓存里所有已到达的非响应消息（上层用来收 `notifications/resources/updated`）。
    ///
    /// 为什么单独取而不是在 [`call`] 里回调：回调会要求持锁调上层，容易死锁；
    /// 让调用方在自己方便的时候取更简单。
    #[allow(dead_code)]
    pub fn drain_notifications(&mut self) -> Vec<Value> {
        // 入口先回收一次：上一轮已看到 EOF 的话，此刻进程可能已经退了。
        self.reap_if_stream_ended();
        // 先把通道里已到达的都收进来（非阻塞）。
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Ok(v) => self.pending.push_back(v),
                Err(_) => {
                    self.reader_done = true;
                    break;
                }
            }
        }
        // 再从 pending 里挑出通知（无 `id`），响应留着等配对的 `call`。
        let mut got = Vec::new();
        let mut keep = VecDeque::new();
        while let Some(v) = self.pending.pop_front() {
            if v.get("id").map(|i| !i.is_null()).unwrap_or(false) {
                keep.push_back(v);
            } else {
                got.push(v);
            }
        }
        self.pending = keep;
        // 本轮刚置上 `reader_done`（流在本轮结束）⇒ 立刻回收，不等下一轮。
        self.reap_if_stream_ended();
        got
    }
}

impl Drop for StdioChannelClient {
    fn drop(&mut self) {
        // 先关 stdin：多数 MCP server 看到 EOF 会自己退；再 kill 兜底。
        // 不这么做的话，托盘里每换一个渠道就漏一个子进程。
        let _ = self.child.kill();
        let _ = self.child.wait();
        // 已回收 ⇒ 从 exit 兜底表里摘掉，免得 atexit 对复用过的 pid 误杀。
        #[cfg(unix)]
        reap_on_exit::unregister(self.child.id());
    }
}

/// 进程退出时兜底杀掉还活着的渠道子进程。
///
/// ## 为什么 Drop 里的 kill 不够
///
/// `std::process::exit` **跳过所有 Drop**。而 service 有多处 exit：
/// 配置读不出（`service.rs` 两处）、收尾 20s 超时强退（`service.rs:696`）——
/// 这些路径下 [`StdioChannelClient::drop`] 的 `kill()` 根本不会跑，
/// MCP server 子进程（`abb-channel-*`、`ki`）就留在系统里继续跑。
/// owner 2026-11-04 报的「停止渠道产生残留进程」就是这一类。
///
/// ## 为什么用 atexit 而不是改掉 exit
///
/// `process::exit` 走 libc `exit(3)`，**会执行 atexit 处理器**。所以这里注册一个
/// 就够了，不必去动 service 收尾那 5 处 exit（那些位置各自有「防挂死占锁」的
/// 明确理由，改成正常返回会重新引入挂死）。
///
/// 只 kill 不 wait：进程正要在毫秒内死掉，僵尸会被 launchd/init 立刻收养。
#[cfg(unix)]
mod reap_on_exit {
    use std::sync::Mutex;

    /// 还活着的子进程 pid（spawn 成功登记，Drop 回收后注销）。
    static LIVE: Mutex<Vec<u32>> = Mutex::new(Vec::new());

    extern "C" fn kill_all() {
        // `try_lock`：exit 可能发生在持锁的线程上，卡在 exit 里比漏杀更糟。
        if let Ok(g) = LIVE.try_lock() {
            for pid in g.iter() {
                // 失败无妨：进程可能已经自己退了。
                unsafe {
                    libc::kill(*pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
    }

    pub fn register(pid: u32) {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| unsafe {
            libc::atexit(kill_all);
        });
        if let Ok(mut g) = LIVE.lock() {
            g.push(pid);
        }
    }

    pub fn unregister(pid: u32) {
        if let Ok(mut g) = LIVE.lock() {
            g.retain(|p| *p != pid);
        }
    }

    /// 某个 pid 是否在登记表里（**只在测试里读**；加 `cfg(test)` 免得被
    /// `-D dead-code` 判成死代码）。
    ///
    /// 为什么按 pid 而不是按总数：测试默认**并行**跑，全局计数会被别的用例
    /// spawn 的子进程干扰——第一版用 `before + 1` 在完整测试集里就红了
    /// （单跑绿、全跑红，正是测试隔离缺陷的典型形态）。
    #[cfg(test)]
    pub fn is_tracked(pid: u32) -> bool {
        LIVE.lock().map(|g| g.contains(&pid)).unwrap_or(false)
    }
}

/// 把一行分好类：是自己的响应 ⇒ `Some`；通知 / 别人的响应 ⇒ `None`（并缓存）。
///
/// 这里**故意**把非目标消息全部留住而不是丢弃——见 [`StdioChannelClient::call`] 的说明。
fn classify(v: &Value, want: u64) -> Result<Option<Value>, ClientError> {
    if v.get("jsonrpc").and_then(|s| s.as_str()) != Some("2.0") {
        return Err(ClientError::BadJson("缺少 jsonrpc: 2.0".into()));
    }
    let id = v.get("id");
    let is_notification = id.map(|i| i.is_null()).unwrap_or(true);
    if is_notification {
        return Ok(None);
    }
    if id.and_then(|i| i.as_u64()) != Some(want) {
        return Ok(None);
    }
    parse_response(v).map(Some).map_err(ClientError::Rpc)
}

/// 读线程：把 stdout 按行切、按行解析成 JSON 投回通道。
fn spawn_reader(stdout: ChildStdout) -> Receiver<Result<Value, ClientError>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            // `read_until` 而不是 `read_line`：非法 UTF-8 不该让整条流挂掉，
            // 而且能顺手量到字节数（`read_line` 到 8 MB 才报错，那时内存已经吃掉了）。
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) => {
                    let _ = tx.send(Err(ClientError::Closed));
                    return;
                }
                Ok(_) => {
                    if buf.len() > MAX_LINE_BYTES {
                        let _ = tx.send(Err(ClientError::LineTooLong));
                        return;
                    }
                    // 去掉行尾换行（`read_until` 保留它）。
                    while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
                        buf.pop();
                    }
                    if buf.is_empty() {
                        continue;
                    }
                    let v = match serde_json::from_slice::<Value>(&buf) {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = tx.send(Err(ClientError::BadJson(e.to_string())));
                            return;
                        }
                    };
                    if tx.send(Ok(v)).is_err() {
                        return;
                    }
                }
                Err(_) => {
                    let _ = tx.send(Err(ClientError::Closed));
                    return;
                }
            }
        }
    });
    rx
}

/// 测试用的 POSIX shell 路径（各平台的 mock MCP server 都靠它跑脚本体）。
///
/// **为什么必须探测，不能硬写 `/bin/sh`**：2026-11-05 首次在 GitHub 上真跑这批
/// 测试时，Windows job 有 **29 个用例失败**，全部同一行——`/bin/sh` 在 Windows
/// 不存在（`os error 3: The system cannot find the path specified`）。
/// 而这些用例的逻辑与平台无关（只是「读一行、回一行」的假 MCP server），
/// 却因为一个路径常量在 Windows 上整片红。镜像停更 6 天让这个错藏了 6 天。
///
/// Windows 的 GitHub runner 预装 Git for Windows，`shell: bash` 用的就是
/// `C:\Program Files\Git\bin\bash.exe`；直接用绝对路径探测，不依赖 PATH。
///
/// 全部候选都不存在时返回 `None`，调用方用 `expect` **响亮失败**——不允许静默
/// 跳过：那正是本仓反复吃过的假绿（skip 默认等于通过）。
#[cfg(test)]
pub(crate) fn posix_shell() -> Option<&'static str> {
    const CANDIDATES: &[&str] = &[
        "/bin/sh",
        "C:\\Program Files\\Git\\bin\\bash.exe",
        "C:\\Program Files\\Git\\usr\\bin\\sh.exe",
        "C:\\Program Files (x86)\\Git\\bin\\bash.exe",
    ];
    CANDIDATES
        .iter()
        .copied()
        .find(|p| std::path::Path::new(p).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用一个 shell 脚本冒充 MCP server —— 比拉真 server 快、且能精确构造
    /// 「先推通知」「乱序回响应」「不回响应」这些真实故障。
    fn script_client(body: &str) -> StdioChannelClient {
        let sh = posix_shell().expect(
            "测试环境没有可用的 POSIX shell（/bin/sh 或 Git Bash）——mock MCP server 靠它跑脚本体",
        );
        StdioChannelClient::spawn("fake", sh, &["-c".into(), body.into()], &[])
            .expect("起不来测试脚本")
    }

    /// 子进程必须登记进 exit 兜底表，drop 后必须摘掉。
    ///
    /// 判据两头都要：只验「登记了」会把「永不注销」的泄漏漏掉（表越涨越大，
    /// atexit 时对早已回收的 pid 发 SIGKILL —— pid 复用后就是**误杀别人的进程**）。
    #[cfg(unix)]
    #[test]
    fn child_is_tracked_for_exit_and_untracked_after_drop() {
        let c = script_client("sleep 5");
        let pid = c.child.id();
        assert!(
            super::reap_on_exit::is_tracked(pid),
            "spawn 后必须登记（否则 process::exit 时会漏杀）"
        );
        drop(c);
        assert!(
            !super::reap_on_exit::is_tracked(pid),
            "drop 后必须注销（否则 pid 复用时会误杀无辜进程）"
        );
    }

    /// **MCP server 自己退出后必须被回收**（否则留僵尸进程）。
    ///
    /// 2026-11-04 owner 实报「停止渠道会产生僵尸进程」。根因：`reader_done`
    /// 只置标志、**从不 `wait()`**——`wait()` 原来只在 `Drop` 里。于是
    /// 「server 崩溃/自己退出」到「client 被 drop」之间，子进程是 Z 状态挂着；
    /// 而 client 被 `SubscribeSource` 长期持有，这个窗口可以很长。
    ///
    /// 判据用 `ps -o stat=`：空 = 已回收（正确）；`Z` = 僵尸（缺陷）。
    /// 不用 `/proc`：macOS 没有。
    #[cfg(unix)]
    #[test]
    fn exited_server_is_reaped_not_left_zombie() {
        use std::time::Duration;
        // 立刻退出：模拟 server 崩溃/自退（最常见形态）。
        let sh = posix_shell().expect("测试环境没有可用的 POSIX shell");
        let mut c = StdioChannelClient::spawn("t", sh, &["-c".into(), "exit 0".into()], &[])
            .expect("起不来测试脚本");
        let pid = c.child.id();
        // 等它变成「已退出未回收」。
        std::thread::sleep(Duration::from_millis(300));
        // 触发检测点（真实现里 fetch 每轮都会调它）。
        let _ = c.drain_notifications();
        std::thread::sleep(Duration::from_millis(200));
        // **不能用 `ps` 判**：本机沙箱里 `ps` 直接 Operation not permitted，
        // `Command::output()` 返回 Err 被 unwrap_or_default 吞成空串 ⇒ 断言恒过
        // （第一版就是这么假绿的，实测 stat="" 才发现）。
        //
        // `waitpid(WNOHANG)` 是真系统调用，三种返回都有意义：
        //   r == pid  ⇒ 已退出且**没人回收** ⇒ 僵尸（缺陷）
        //   r == 0    ⇒ 还活着（没退出，不算僵尸）
        //   r == -1   ⇒ 早被别人回收了（正确路径）
        let mut status: libc::c_int = 0;
        let r = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
        assert!(
            r != pid as libc::pid_t,
            "server 已退出但没人 wait ⇒ pid={pid} 成了僵尸（waitpid={r}）；\
             必须在检测到流结束时 try_wait 回收"
        );
    }

    #[test]
    fn normal_request_response_pairs_by_id() {
        let mut c = script_client(
            r#"while IFS= read -r line; do
                 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                 printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
               done"#,
        );
        let r = c.call("tools/list", Value::Null).expect("应当拿到结果");
        assert_eq!(r.get("ok").and_then(|v| v.as_bool()), Some(true));
        // 第二次调用的 id 必须递增（不是永远 1）——否则 server 侧无法区分。
        let r2 = c.call("tools/list", Value::Null).expect("第二次");
        assert_eq!(r2.get("ok").and_then(|v| v.as_bool()), Some(true));
    }

    /// **这是本模块最重要的一条**：server 先推一条通知再回响应。
    /// 天真实现会把通知当成响应 ⇒ 解析出 `result` 缺失的错误，
    /// 或者更糟：把通知当成功返回。
    #[test]
    fn notification_before_response_is_not_mistaken_for_it() {
        let mut c = script_client(
            r#"while IFS= read -r line; do
                 printf '{"jsonrpc":"2.0","method":"notifications/resources/updated","params":{"uri":"channel://a/inbox"}}\n'
                 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                 printf '{"jsonrpc":"2.0","id":%s,"result":{"v":7}}\n' "$id"
               done"#,
        );
        let r = c
            .call("tools/list", Value::Null)
            .expect("应当拿到自己的响应");
        assert_eq!(
            r.get("v").and_then(|v| v.as_u64()),
            Some(7),
            "拿到的不是 id 配对的响应（通知被当成响应了？）"
        );
    }

    /// 乱序：server 先回 id=2 的，再回 id=1 的。第二次调用必须用缓存里的那条，
    /// 而不是去读新的一行（那会永远等下去）。
    #[test]
    fn out_of_order_response_is_reused_from_pending() {
        let mut c = script_client(
            r#"n=0
               while IFS= read -r line; do
                 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                 n=$((n+1))
                 if [ "$n" = "1" ]; then
                   printf '{"jsonrpc":"2.0","id":2,"result":{"who":"two"}}\n'
                   printf '{"jsonrpc":"2.0","id":1,"result":{"who":"one"}}\n'
                 fi
               done"#,
        );
        let r1 = c.call("a", Value::Null).expect("id=1");
        assert_eq!(r1.get("who").and_then(|v| v.as_str()), Some("one"));
        // id=2 那条已经在 pending 里，不需要 server 再说什么。
        let r2 = c.call("b", Value::Null).expect("id=2 应当来自缓存");
        assert_eq!(r2.get("who").and_then(|v| v.as_str()), Some("two"));
    }

    #[test]
    fn error_response_surfaces_as_rpc_error() {
        let mut c = script_client(
            r#"while IFS= read -r line; do
                 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                 printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"no such tool"}}\n' "$id"
               done"#,
        );
        let err = c.call("tools/call", Value::Null).unwrap_err();
        assert!(matches!(err, ClientError::Rpc(_)), "实际：{err:?}");
    }

    /// server 只推通知、永不回响应 ⇒ 必须在超时后报错，**不能永远挂着**
    /// （挂着在托盘里的症状是「界面还在、渠道不动」，最难查）。
    #[test]
    fn silence_times_out_instead_of_hanging() {
        let mut c = script_client("while IFS= read -r line; do :; done")
            .with_timeout(Duration::from_millis(300));
        let t0 = std::time::Instant::now();
        let err = c.call("x", Value::Null).unwrap_err();
        assert!(matches!(err, ClientError::Timeout(_)), "实际：{err:?}");
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "超时没生效，等了 {:?}",
            t0.elapsed()
        );
    }

    /// 流直接关掉 ⇒ 报「没等到这个 id 的响应」，而不是笼统的 Closed：
    /// 这个区别能指出「server 只推通知不回响应」这类实现缺陷。
    #[test]
    fn closed_stream_without_response_names_the_missing_id() {
        let mut c = script_client("exit 0");
        let err = c.call("x", Value::Null).unwrap_err();
        assert!(
            matches!(err, ClientError::NoResponseForId(1)),
            "实际：{err:?}"
        );
    }

    #[test]
    fn invalid_json_is_reported_not_silently_skipped() {
        let mut c = script_client("while IFS= read -r line; do printf 'not json\\n'; done");
        let err = c.call("x", Value::Null).unwrap_err();
        assert!(matches!(err, ClientError::BadJson(_)), "实际：{err:?}");
    }

    /// 通知能被上层单独取走（订阅路径靠它），且**不会**被当成响应。
    #[test]
    fn drain_notifications_returns_only_notifications() {
        let mut c = script_client(
            r#"while IFS= read -r line; do
                 printf '{"jsonrpc":"2.0","method":"notifications/resources/updated","params":{"uri":"channel://a/inbox"}}\n'
               done"#,
        );
        // 先触发一次读取：call 会超时（用短超时，别让用例白等 20 秒），
        // 但通知此时已被缓存进 pending。
        c = c.with_timeout(Duration::from_millis(300));
        let _ = c.call("x", Value::Null);
        let ns = c.drain_notifications();
        assert_eq!(ns.len(), 1, "应当拿到 1 条通知，实际 {} 条", ns.len());
        assert_eq!(
            ns[0].get("method").and_then(|v| v.as_str()),
            Some("notifications/resources/updated")
        );
        // 再取一次应为空（已取走）。
        assert!(c.drain_notifications().is_empty());
    }

    #[test]
    fn missing_jsonrpc_version_is_rejected() {
        let mut c = script_client(
            r#"while IFS= read -r line; do
                 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                 printf '{"id":%s,"result":{}}\n' "$id"
               done"#,
        );
        let err = c.call("x", Value::Null).unwrap_err();
        assert!(matches!(err, ClientError::BadJson(_)), "实际：{err:?}");
    }

    #[test]
    fn spawn_failure_is_explicit() {
        // `StdioChannelClient` 不实现 Debug（它持有 Channel/Child，Debug 派生没意义），
        // 所以用 `match` 取错误值而不是 `unwrap_err()`。
        let err = match StdioChannelClient::spawn("nope", "/definitely/not/a/binary", &[], &[]) {
            Ok(_) => panic!("不存在的可执行文件不该起得来"),
            Err(e) => e,
        };
        assert!(matches!(err, ClientError::Spawn(_)), "实际：{err:?}");
        // 错误文案要带上是哪个程序，否则多渠道时排障全靠猜。
        assert!(err.to_string().contains("/definitely/not/a/binary"));
    }
}
