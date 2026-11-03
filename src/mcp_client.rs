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

    /// 取出缓存里所有已到达的非响应消息（上层用来收 `notifications/resources/updated`）。
    ///
    /// 为什么单独取而不是在 [`call`] 里回调：回调会要求持锁调上层，容易死锁；
    /// 让调用方在自己方便的时候取更简单。
    #[allow(dead_code)]
    pub fn drain_notifications(&mut self) -> Vec<Value> {
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
        got
    }
}

impl Drop for StdioChannelClient {
    fn drop(&mut self) {
        // 先关 stdin：多数 MCP server 看到 EOF 会自己退；再 kill 兜底。
        // 不这么做的话，托盘里每换一个渠道就漏一个子进程。
        let _ = self.child.kill();
        let _ = self.child.wait();
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 用一个 shell 脚本冒充 MCP server —— 比拉真 server 快、且能精确构造
    /// 「先推通知」「乱序回响应」「不回响应」这些真实故障。
    fn script_client(body: &str) -> StdioChannelClient {
        StdioChannelClient::spawn("fake", "/bin/sh", &["-c".into(), body.into()], &[])
            .expect("起不来测试脚本")
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
