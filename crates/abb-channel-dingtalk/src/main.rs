//! 钉钉渠道的 MCP server 主程序。
//!
//! ## 与 ABB 的关系
//!
//! ABB 是总线，本进程是它拉起的**渠道服务**之一。契约见
//! `docs/mcp-bus-architecture.md` §3：
//!
//! | 方向 | 契约 | 本进程的实现 |
//! |---|---|---|
//! | 出站 | 工具 `send_text`（必需） | ✅ 走 `dingtalk::DingTalkClient::send_text`（超长自动分段） |
//! | 出站 | 工具 `send_thread_reply`（可选） | ❌ **不声明**：钉钉**没有话题概念** |
//! | 出站 | 工具 `send_attachment`（可选） | ✅ base64 内联 → `media/upload` → 图片/文件消息 |
//! | 入站 | `poll` 或 `channel://` 资源订阅 | ✅ **资源订阅**（`channel://inbox`），见下 |
//!
//! ## 与飞书版的唯一结构差异：没有 `send_thread_reply`
//!
//! 飞书有话题（`/im/v1/messages/:id/reply`），不声明 `send_thread_reply` 就会把所有回复
//! 拍平到主聊天；钉钉的 Stream 回调与发送接口里**根本没有话题维度**（群聊一律发到
//! `openConversationId`，单聊一律发到 staffId）。声明一个做不到的工具会让 ABB 走到
//! 必然失败的路径，所以这里**显式不声明**，ABB 按契约 §3.1 回落 `send_text`。
//!
//! 代价与补偿：原来 ABB 的 `DingTalkMessenger::send_thread_reply` 顺带做两件事——
//! ①按 `DINGTALK_MSG_LIMIT` 分段；②群聊里 @ 最近提问者。两件都在本进程补上：
//! 分段在 `call_tool` 里做，@ 靠入站时刷新的「最近提问者」表（`Channel::last_sender`，
//! 与 ABB `note_sender` 同口径）。不补的话表现是「群里回复不再 @ 提问者」——一种
//! 静默的能力退化。
//!
//! ## 为什么入站选「订阅」而不是 `poll`
//!
//! 钉钉的入站是**服务端推的长连接**（Stream 模式：`connections/open` 换 ticket → wss
//! 推 CALLBACK 帧），本地没有「按游标取一批」的原语。若强行做成 `poll`，每次调用要么
//! 空转，要么得在进程里再造一层缓冲——而缓冲本来就是要给订阅用的：
//!
//! ```text
//!   钉钉 Stream ──wss──► 本进程（缓冲 Vec<消息>）──notifications/resources/updated──► ABB
//!                                                    ABB 收到通知 ──resources/read──► 取走并清空
//! ```
//!
//! ## 三个必须做对的地方
//!
//! 1. **stdout 只能出 JSON-RPC**。任何一行杂输出都会让 ABB 侧的解析炸掉，所以日志
//!    一律 `eprintln!`（协议层那 7 处 `log!` 与 3 处 `botstatus::report` 都已在
//!    `src/wsin.rs` 里换掉/删掉）。
//! 2. **`resources/read` 必须清空缓冲**。不清的话 ABB 每次通知都会把历史消息重放一遍
//!    ——同一条消息被回答多次比丢消息更难查。
//! 3. **入站失败不许拖死契约层**。Stream 连不上（凭证错/网络不通）只打 stderr 警告，
//!    `tools/list` / `send_text` 照常工作——ABB 至少还能发消息、还能看到真实报错。
//!
//! ## 凭证
//!
//! 只从环境变量拿 `DT_APP_ID` / `DT_APP_SECRET` / `DT_ROBOT_CODE`（ABB 的
//! `mcp_channel.env` 下发）。本进程**不认识 ABB 的 config.json**——渠道配置由提供方决定。
//! `DT_ROBOT_CODE` 缺省回落到 `DT_APP_ID`（钉钉里两者通常同值，都是应用的 AppKey）；
//! 两者都空 ⇒ 出站工具**明确报错**，而不是发一条注定被平台拒的请求。

mod dingtalk;
mod wsin;

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use dingtalk::DingTalkClient;
use serde_json::{json, Value};

/// 入站资源的 URI（契约 §3.2 的 A 方案，整渠道一个 inbox）。
pub const INBOX_URI: &str = "channel://inbox";

/// 缓冲上限（条）。ABB 每轮 read 都会取走全部，正常不会积累到这个数；
/// 上限只为「ABB 崩了/没来读」时**不无限吃内存**——超了丢最旧的并打 stderr。
const INBOX_CAP: usize = 200;

/// 钉钉外发体积上限（与 ABB `attachments.rs` 的 `DINGTALK_{IMAGE,FILE}_MAX_BYTES` 同值）。
/// 内联 base64 后体积还要 ×4/3，所以本地先兜一道，别等上传被服务端拒。
/// 20 MB × 4/3 ≈ 26.7 MB < 契约 §3.5 的单帧上限 32 MB，仍在线内。
const DINGTALK_IMAGE_MAX_BYTES: usize = 20 * 1024 * 1024;
const DINGTALK_FILE_MAX_BYTES: usize = 20 * 1024 * 1024;

/// 渠道进程的共享状态。
struct Channel {
    /// 出站用的钉钉客户端（`access_token` 缓存在它里面）。
    client: DingTalkClient,
    /// 机器人码（出站每条消息都要带）。空 = 没配凭证，工具调用会明确报错。
    robot_code: String,
    /// **待 ABB 取走**的入站消息（契约 §3.3 形状）。`resources/read` 取走并清空。
    inbox: Mutex<Vec<Value>>,
    /// 已订阅的 URI。只有订阅过才推通知（MCP 的规矩：没订就别推）。
    subscribed: Mutex<Vec<String>>,
    /// 订阅成功后「缓冲里已经有东西」⇒ 主循环写完响应再补一条通知。
    ///
    /// 为什么不在 `resources/subscribe` 的处理里直接推：那条通知会在**响应之前**
    /// 出现在 stdout。JSON-RPC 允许乱序，但没必要给对端制造「先收到通知、后收到
    /// 订阅响应」的时序；写完响应再推，语义最干净。
    notify_pending: AtomicBool,
    /// 每个会话最近一条入站消息的发送者（群聊回复时 @ 他）。
    ///
    /// 原 ABB 的 `DingTalkMessenger` 用同一张表（`note_sender` + `send_thread_reply`
    /// 里查它）；本进程没有 `send_thread_reply` 工具了，所以由 `send_text` 消费它，
    /// 否则群聊里回复会静默失去 @ 提及（见文件头「代价与补偿」）。
    last_sender: Mutex<HashMap<String, String>>,
    /// stdout 的唯一出口。主循环回包与 Stream 线程推通知都走它 ⇒ **行不会交错**。
    out: Mutex<std::io::Stdout>,
}

impl Channel {
    /// 写一行 JSON 到 stdout 并 flush（NDJSON）。
    ///
    /// 锁中毒不 panic：这条路径的失败方式是「日志里少一行」，不该让整个渠道进程死。
    fn write_line(&self, v: &Value) {
        let mut g = self.out.lock().unwrap_or_else(|e| e.into_inner());
        if serde_json::to_writer(&mut *g, v).is_err() || g.write_all(b"\n").is_err() {
            eprintln!("[dingtalk-mcp] 写 stdout 失败，放弃这一行");
            return;
        }
        let _ = g.flush();
    }

    fn is_subscribed(&self, uri: &str) -> bool {
        self.subscribed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|u| u == uri)
    }

    fn subscribe(&self, uri: &str) {
        let mut g = self.subscribed.lock().unwrap_or_else(|e| e.into_inner());
        if !g.iter().any(|u| u == uri) {
            g.push(uri.to_string());
        }
    }

    /// 记下「这个会话最近是谁在说话」。
    fn note_sender(&self, chat_id: &str, sender_id: &str) {
        let mut g = self.last_sender.lock().unwrap_or_else(|e| e.into_inner());
        g.insert(chat_id.to_string(), sender_id.to_string());
    }

    /// 取「这个会话最近提问的人」（群聊回复时 @ 用）。没有记录就是 `None`。
    fn last_sender_of(&self, chat_id: &str) -> Option<String> {
        self.last_sender
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(chat_id)
            .cloned()
    }

    /// Stream 线程的入口：塞进缓冲，然后（若已订阅）推一条「有更新」。
    fn push_inbound(&self, m: Value) {
        {
            let mut g = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
            if g.len() >= INBOX_CAP {
                g.remove(0);
                eprintln!("[dingtalk-mcp] 入站缓冲满（{INBOX_CAP}），丢弃最旧一条");
            }
            g.push(m);
        }
        self.notify_if_subscribed();
    }

    /// 推 `notifications/resources/updated`。两个条件同时满足才推：
    /// 1. **订阅过**——MCP 里未订阅的 notify 是噪音；
    /// 2. **缓冲非空**——通知的语义是「有新东西可读」，让 ABB 为一次空 read 跑一趟
    ///    毫无意义（read 会滴空缓冲，所以「非空」与「有东西可读」等价）。
    fn notify_if_subscribed(&self) {
        if !self.is_subscribed(INBOX_URI) {
            return;
        }
        if self
            .inbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
        {
            return;
        }
        self.write_line(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/resources/updated",
            "params": {"uri": INBOX_URI},
        }));
    }

    /// 取走缓冲里的全部消息（**取走即清空**，见文件头第 2 条）。
    fn take_inbox(&self) -> Vec<Value> {
        let mut g = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *g)
    }
}

fn main() {
    // 凭证从环境变量拿（ABB 的 `mcp_channel.env` 下发）——渠道配置由提供方决定，
    // 本进程不认识 ABB 的 config.json。
    let app_id = std::env::var("DT_APP_ID").unwrap_or_default();
    let app_secret = std::env::var("DT_APP_SECRET").unwrap_or_default();
    let robot_code = std::env::var("DT_ROBOT_CODE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        // 没单独配 robotCode 时回落 AppKey：钉钉里二者通常同值（都是企业内部应用的 AppKey）。
        .unwrap_or_else(|| app_id.clone());
    eprintln!(
        "[dingtalk-mcp] 启动 app_id={} secret={} robot={}",
        if app_id.is_empty() {
            "(空)".to_string()
        } else {
            mask(&app_id)
        },
        if app_secret.is_empty() {
            "空"
        } else {
            "已设置"
        },
        if robot_code.is_empty() {
            "(空)".to_string()
        } else {
            mask(&robot_code)
        }
    );

    let ch = Arc::new(Channel {
        client: DingTalkClient::new(&app_id, &app_secret),
        robot_code,
        inbox: Mutex::new(Vec::new()),
        subscribed: Mutex::new(Vec::new()),
        notify_pending: AtomicBool::new(false),
        last_sender: Mutex::new(HashMap::new()),
        out: Mutex::new(std::io::stdout()),
    });

    if app_id.is_empty() || app_secret.is_empty() {
        eprintln!(
            "[dingtalk-mcp] 未提供 DT_APP_ID / DT_APP_SECRET —— Stream 长连接不启动，\
             出站调用也会被钉钉拒。把这两项写进 ABB 的 mcp_channel.env 后重启。"
        );
    } else {
        // 入站：Stream 长连接跑在独立线程（自己的 tokio runtime）。
        // **失败不致命**：连不上只打 stderr，契约层照常工作（见文件头第 3 条）。
        wsin::spawn(ch.clone());
    }

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(line) else {
            eprintln!("[dingtalk-mcp] 非法 JSON，忽略");
            continue;
        };
        if let Some(resp) = handle(&ch, &req) {
            // 一行一条：MCP 是 NDJSON。
            ch.write_line(&resp);
        }
        // 订阅成功时若缓冲里已有消息，**写完响应再补通知**（见 `notify_pending`）。
        if ch.notify_pending.swap(false, Ordering::SeqCst) {
            ch.notify_if_subscribed();
        }
    }
    eprintln!("[dingtalk-mcp] stdin 关闭，退出");
}

/// 只露字符串的头尾：它在日志里出现得频繁，但完整值仍属配置。
fn mask(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 8 {
        return "***".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

fn handle(ch: &Arc<Channel>, req: &Value) -> Option<Value> {
    let method = req.get("method")?.as_str()?;
    // 通知（无 id / id 为 null）不回包。注意 `initialize` 之类请求必须带 id。
    let rid = req.get("id").filter(|v| !v.is_null())?.clone();
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            // 声明 resources 是必须的：ABB 据 `resources/subscribe` 能力选入站路线（A 方案）。
            "capabilities": {"tools": {}, "resources": {"subscribe": true, "listChanged": false}},
            "serverInfo": {"name": "abb-channel-dingtalk", "version": env!("CARGO_PKG_VERSION")},
        })),
        "tools/list" => Ok(json!({"tools": tools()})),
        "resources/list" => Ok(json!({
            "resources": [{
                "uri": INBOX_URI,
                "name": "inbox",
                "mimeType": "application/json",
                "description": "钉钉入站消息缓冲（订阅后收到 updated 通知再 read）",
            }]
        })),
        "resources/subscribe" => subscribe(ch, &params),
        "resources/unsubscribe" => {
            let uri = params.get("uri").and_then(|v| v.as_str()).unwrap_or("");
            let mut g = ch.subscribed.lock().unwrap_or_else(|e| e.into_inner());
            g.retain(|u| u != uri);
            Ok(json!({}))
        }
        "resources/read" => read_inbox(ch, &params),
        "tools/call" => call_tool(ch, &params),
        // 其余通知类方法（`notifications/*`）也走这里：它们没有 id，上面已提前 return。
        other => Err(format!("未实现的方法 `{other}`")),
    };

    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": rid, "result": r}),
        Err(msg) => json!({
            "jsonrpc": "2.0", "id": rid,
            "error": {"code": -32603, "message": msg}
        }),
    })
}

/// `resources/subscribe`：只认 `channel://inbox`。
///
/// 别的 URI 一律报错（不静默成功）：ABB 会把「订阅成功」当成入站可用，
/// 若我们答应了一个不存在的资源，症状是「渠道显示在线但收不到消息」。
fn subscribe(ch: &Arc<Channel>, params: &Value) -> Result<Value, String> {
    let uri = params.get("uri").and_then(|v| v.as_str()).unwrap_or("");
    if uri != INBOX_URI {
        return Err(format!(
            "资源 `{uri}` 不存在；本渠道只有 `{INBOX_URI}`（钉钉整渠道一个 inbox）"
        ));
    }
    ch.subscribe(uri);
    eprintln!("[dingtalk-mcp] 已订阅 {uri}");
    // 缓冲里可能已经有消息（订阅之前就收到了）⇒ 让主循环写完响应补一条通知。
    // **空缓冲则不置标记**：没有东西可读时推通知只会换来一次空 read。
    let pending = !ch
        .inbox
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty();
    ch.notify_pending.store(pending, Ordering::SeqCst);
    Ok(json!({}))
}

/// `resources/read`：把缓冲区**整批**交给 ABB，并清空。
fn read_inbox(ch: &Arc<Channel>, params: &Value) -> Result<Value, String> {
    let uri = params.get("uri").and_then(|v| v.as_str()).unwrap_or("");
    if uri != INBOX_URI {
        // 读错资源 = 把别的内容当消息交出去，必须 fail-closed。
        return Err(format!(
            "资源 `{uri}` 不存在；本渠道只有 `{INBOX_URI}`（拒绝读未知资源）"
        ));
    }
    let msgs = ch.take_inbox();
    // `text` 里是**数组**（可能 0 条）：契约 §3.3 允许多条一个 inbox，
    // ABB 的 `messages_from_resource` 对空数组与对象都接受。
    let text = Value::Array(msgs).to_string();
    Ok(json!({
        "contents": [{"uri": INBOX_URI, "mimeType": "application/json", "text": text}]
    }))
}

fn tools() -> Value {
    json!([
        {
            "name": "send_text",
            "description": "发文本到钉钉会话（超长按 8000 字符分段；cid 开头按群聊发、否则按 staffId 单聊发）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat_id": {"type": "string", "description": "会话键：群聊 = openConversationId（cid…），单聊 = 对方 staffId"},
                    "text": {"type": "string"}
                },
                "required": ["chat_id", "text"]
            }
        },
        {
            "name": "send_attachment",
            "description": "发媒体到钉钉会话（图片走 sampleImageMsg，其它走 sampleFile；内容 base64 内联）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat_id": {"type": "string", "description": "会话键：群聊 = openConversationId（cid…），单聊 = 对方 staffId"},
                    "name": {"type": "string", "description": "文件名（按后缀判断图片/文件与上传类型）"},
                    "mime": {"type": "string"},
                    "kind": {"type": "string", "description": "image | file"},
                    "data_b64": {"type": "string", "description": "文件内容的 base64"}
                },
                "required": ["chat_id", "name", "data_b64"]
            }
        }
    ])
}

/// 文件名兜底：名字为空时按 mime 补一个。
///
/// 判定必须看**扩展名**而不只看 `kind`：钉钉 `media/upload` 按后缀校验媒体类型
/// （固定名 "image" 会被判 40004「不合法的媒体文件类型」，审查 #254），
/// `sampleFile` 模板也有自己的后缀白名单。与 ABB `attachment_upload_name` 同口径。
fn upload_name(name: &str, mime: &str, kind: &str) -> String {
    if !name.trim().is_empty() {
        return name.to_string();
    }
    let ext = match mime
        .rsplit_once('/')
        .map(|(_, sub)| sub.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "png",
        Some("jpeg") | Some("jpg") => "jpg",
        Some("gif") => "gif",
        Some("webp") => "webp",
        Some("bmp") => "bmp",
        Some("pdf") => "pdf",
        Some("plain") => "txt",
        _ => {
            if kind == "image" {
                "png"
            } else {
                "bin"
            }
        }
    };
    format!("attachment.{ext}")
}

/// 钉钉外发能力闸（与 ABB `messenger::dingtalk_send_plan` 同判据）：
/// **先判能不能发，再去读/解码内容**——反过来的历史教训是「不支持的组合把 100 MB
/// 整读进内存再丢」（审查 #254）。
#[derive(Debug)]
enum SendPlan {
    Image,
    File(&'static str),
}

fn send_plan(kind: &str, name: &str) -> Result<SendPlan, String> {
    match kind {
        "image" if dingtalk::dingtalk_image_uploadable(name) => Ok(SendPlan::Image),
        "image" => Err(format!(
            "钉钉机器人发送图片失败：仅支持 {}；当前图片={name}",
            dingtalk::DINGTALK_IMAGE_EXTS.join("/")
        )),
        "file" => {
            if let Some(ft) = dingtalk::dingtalk_file_type(name) {
                return Ok(SendPlan::File(ft));
            }
            // kind 说文件、后缀却是图片：`sampleFile` 的 fileType 白名单里没有 png/jpg，
            // 硬发必被拒；按图片发能成（ABB 的 kind 由后缀推导，正常不会走到这里）。
            if dingtalk::dingtalk_image_uploadable(name) {
                return Ok(SendPlan::Image);
            }
            Err(format!(
                "钉钉机器人发送文件失败：仅支持 {}；当前文件={name}",
                dingtalk::DINGTALK_FILE_EXTS.join("/")
            ))
        }
        _ => Err(format!(
            "钉钉机器人暂不支持该附件类型：kind={kind} 文件={name}；当前支持图片与 {}",
            dingtalk::DINGTALK_FILE_EXTS.join("/")
        )),
    }
}

fn call_tool(ch: &Arc<Channel>, params: &Value) -> Result<Value, String> {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let a = params.get("arguments").cloned().unwrap_or(Value::Null);
    match name {
        "send_text" => {
            let chat_id = a.get("chat_id").and_then(|v| v.as_str()).unwrap_or("");
            let text = a.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if chat_id.is_empty() {
                return Err("send_text 缺 chat_id".into());
            }
            // 没配机器人码就明确失败：钉钉会以 400 拒掉，报「缺 robotCode」比报平台的
            // 「invalid parameter」可诊断得多。
            if ch.robot_code.is_empty() {
                return Err(
                    "send_text 失败：未配置机器人码（DT_ROBOT_CODE / DT_APP_ID 都为空）".into(),
                );
            }
            let at = at_user_for(ch, chat_id);
            let rt = rt();
            rt.block_on(async {
                // 超长分段（与 ABB 的 `send_thread_reply` 同一套按字符贪心逻辑）：
                // 不分段时钉钉会整条拒（文本上限约 2 万字节）。
                for chunk in split_text(text, dingtalk::DINGTALK_MSG_LIMIT) {
                    ch.client
                        .send_text(chat_id, &ch.robot_code, &chunk, at.as_deref())
                        .await?;
                }
                Ok::<(), anyhow::Error>(())
            })
            .map_err(|e| format!("send_text 失败：{e:#}"))?;
            Ok(json!({"content": [{"type": "text", "text": "ok"}]}))
        }
        "send_attachment" => {
            let chat_id = a.get("chat_id").and_then(|v| v.as_str()).unwrap_or("");
            let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let mime = a.get("mime").and_then(|v| v.as_str()).unwrap_or("");
            let kind = a.get("kind").and_then(|v| v.as_str()).unwrap_or("file");
            let b64 = a.get("data_b64").and_then(|v| v.as_str()).unwrap_or("");
            if chat_id.is_empty() {
                return Err("send_attachment 缺 chat_id".into());
            }
            if b64.is_empty() {
                return Err("send_attachment 缺 data_b64".into());
            }
            if ch.robot_code.is_empty() {
                return Err(
                    "send_attachment 失败：未配置机器人码（DT_ROBOT_CODE / DT_APP_ID 都为空）"
                        .into(),
                );
            }
            let fname = upload_name(name, mime, kind);
            // 能力闸先于解码：不支持的组合当场报，不必先 base64 解出几十 MB。
            let plan = send_plan(kind, &fname)?;
            let cap = match plan {
                SendPlan::Image => DINGTALK_IMAGE_MAX_BYTES,
                SendPlan::File(_) => DINGTALK_FILE_MAX_BYTES,
            };
            let data = base64_decode(b64).ok_or_else(|| "data_b64 不是合法 base64".to_string())?;
            if data.len() > cap {
                return Err(format!(
                    "附件过大（{} MB > 平台上限 {} MB）：{fname}",
                    data.len() / (1024 * 1024),
                    cap / (1024 * 1024)
                ));
            }
            let rt = rt();
            rt.block_on(async {
                match plan {
                    SendPlan::Image => {
                        let media_id = ch.client.upload_image(data, &fname).await?;
                        ch.client
                            .send_image_message(chat_id, &ch.robot_code, &media_id)
                            .await
                    }
                    SendPlan::File(file_type) => {
                        let media_id = ch.client.upload_media("file", data, &fname).await?;
                        ch.client
                            .send_file_message(
                                chat_id,
                                &ch.robot_code,
                                &media_id,
                                &fname,
                                file_type,
                            )
                            .await
                    }
                }
            })
            .map_err(|e| format!("send_attachment 失败：{e:#}"))?;
            Ok(json!({"content": [{"type": "text", "text": "ok"}]}))
        }
        other => Err(format!("未知工具 `{other}`")),
    }
}

/// 群聊回复要 @ 谁：只有群聊（`cid` 开头）且知道最近提问者时才 @。
///
/// 单聊 chat_id 就是对方 staffId，不需要 @；群聊不 @ 的话回复会被淹没在刷屏里
/// ——这是 ABB 原 `DingTalkMessenger::send_thread_reply` 的行为，本进程没有那个工具，
/// 语义落在 `send_text` 上。
fn at_user_for(ch: &Arc<Channel>, chat_id: &str) -> Option<String> {
    if !dingtalk::is_group_chat(chat_id) {
        return None;
    }
    ch.last_sender_of(chat_id)
}

/// 标准 base64 解码（含 padding）。返回 `None` 表示输入非法。
///
/// 手写不引 crate：与 ABB 侧 `mcp_channel::base64_encode` 对称，且飞书/微信两个
/// 渠道 crate 里也是同一份实现（那边是私有函数，跨 crate 用不了）。
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }
    let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().filter(|&&c| c == b'=').count();
        let mut n: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            let v = if c == b'=' { 0 } else { val(c)? };
            n |= (v as u32) << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// 按字符数贪心分段，且**尽量不在 ``` 代码块中间切断**。
///
/// 抄自 ABB `src/feishu.rs::split_text`（那边是 `pub fn`，但不在本 crate 里）：
/// 钉钉的文本上限约 2 万字节，超长整条会被平台拒；原实现由 ABB
/// `DingTalkMessenger::send_thread_reply` 调用它分段，搬到本进程后只能自己在
/// `send_text` 里补上（否则超长回复从「分几条发」退化成「一条都发不出」）。
///
/// **与 ABB 版的唯一差别**：单行本身就超过 `limit` 时（长段落 / minified JSON /
/// 一整个大代码块）硬切。原实现只按行切，这种输入会产出一个**超额 chunk** —— 钉钉
/// 那一条仍然被整条拒，等于分段白做。硬切只是把「必然失败」换成「多分几条发」。
fn split_text(text: &str, limit: usize) -> Vec<String> {
    let limit = limit.max(1);
    if text.chars().count() <= limit {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    let mut in_fence = false; // 是否在 ``` 代码块内（跨行跟踪）
    for line in text.split_inclusive('\n') {
        let l = line.chars().count();
        if l > limit {
            // 单行超限：先收尾当前块，再把这一行按 limit 字符硬切。
            // 不切的话超额 chunk 会被平台整条拒（见函数注释）。
            if !cur.is_empty() {
                chunks.push(std::mem::take(&mut cur));
                cur_len = 0;
            }
            let chars: Vec<char> = line.chars().collect();
            for piece in chars.chunks(limit) {
                chunks.push(piece.iter().collect());
            }
            continue;
        }
        if cur_len + l > limit && !cur.is_empty() && !in_fence {
            chunks.push(std::mem::take(&mut cur));
            cur_len = 0;
        }
        cur.push_str(line);
        cur_len += l;
        // 本行 ``` 出现奇数次 → 切换 fence 状态（fence 行归当前块后再翻转）
        if line.matches("```").count() % 2 == 1 {
            in_fence = !in_fence;
        }
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    if chunks.is_empty() {
        // 兜底：硬切前 limit 字符
        return vec![text.chars().take(limit).collect()];
    }
    chunks
}

/// 只在需要时建 runtime（工具调用是同步入口，内部用 `block_on`）。
///
/// 单线程足够：每次调用都是「一到几次 HTTP 往返」，没有并发需求。
fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("建 tokio runtime 失败")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_channel() -> Arc<Channel> {
        Arc::new(Channel {
            client: DingTalkClient::new("app", "secret"),
            robot_code: "robot".to_string(),
            inbox: Mutex::new(Vec::new()),
            subscribed: Mutex::new(Vec::new()),
            notify_pending: AtomicBool::new(false),
            last_sender: Mutex::new(HashMap::new()),
            // 单测的 stdout 会被 `cargo test` 捕获，写进去不会污染真实协议通道。
            out: Mutex::new(std::io::stdout()),
        })
    }

    fn call(ch: &Arc<Channel>, req: Value) -> Value {
        handle(ch, &req).expect("要回包")
    }

    /// 出站工具**只有两条**：钉钉无话题 ⇒ 不声明 `send_thread_reply`。
    ///
    /// 这条与飞书版正好相反（那边必须声明），所以两边各钉一条：判据不能只靠人记。
    #[test]
    fn tools_list_declares_two_tools_and_no_thread_reply() {
        let names: Vec<String> = tools()
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["send_text", "send_attachment"]);
        assert!(
            !names.iter().any(|n| n == "send_thread_reply"),
            "钉钉没有话题概念，声明它会让 ABB 走到必然失败的路径"
        );
    }

    /// 入站资源必须列出来（不是空数组——那会让 ABB 判「无入站」）。
    #[test]
    fn resources_list_declares_exactly_the_inbox() {
        let r = call(
            &fake_channel(),
            json!({"jsonrpc":"2.0","id":1,"method":"resources/list"}),
        );
        let uris: Vec<&str> = r["result"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x["uri"].as_str())
            .collect();
        assert_eq!(uris, vec![INBOX_URI]);
    }

    /// 契约核心：**订阅后 read 必须给出合法 JSON 数组**（空也要是 `[]`，
    /// 不是 `null`、不是空串——ABB 那边解析失败会整批报错）。
    #[test]
    fn subscribe_then_read_returns_json_array() {
        let ch = fake_channel();
        let sub = call(
            &ch,
            json!({"jsonrpc":"2.0","id":1,"method":"resources/subscribe",
                   "params":{"uri": INBOX_URI}}),
        );
        assert!(sub.get("error").is_none(), "订阅不该失败：{sub}");
        assert_eq!(sub["result"], json!({}));
        assert!(ch.is_subscribed(INBOX_URI), "订阅没记下来");

        let r = call(
            &ch,
            json!({"jsonrpc":"2.0","id":2,"method":"resources/read",
                   "params":{"uri": INBOX_URI}}),
        );
        let text = r["result"]["contents"][0]["text"]
            .as_str()
            .expect("contents[0].text 必须是字符串");
        let parsed: Value = serde_json::from_str(text).expect("text 必须是合法 JSON");
        assert_eq!(parsed, json!([]), "空缓冲要序列化成 []，不是 null");
    }

    /// **read 必须清空缓冲**：不清就会让同一条消息被反复处理。
    #[test]
    fn read_drains_the_inbox() {
        let ch = fake_channel();
        ch.subscribe(INBOX_URI);
        ch.inbox
            .lock()
            .unwrap()
            .push(json!({"chat_id":"c","message_id":"m"}));
        let first = call(
            &ch,
            json!({"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":INBOX_URI}}),
        );
        let text = first["result"]["contents"][0]["text"].as_str().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(text)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let second = call(
            &ch,
            json!({"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":INBOX_URI}}),
        );
        let text = second["result"]["contents"][0]["text"].as_str().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(text).unwrap(),
            json!([]),
            "第二次 read 必须是空的——否则同一条消息会被回答两次"
        );
    }

    /// 读/订阅未知资源必须报错（fail-closed）：读错资源会把别的内容当消息交出去。
    #[test]
    fn unknown_resource_is_rejected() {
        let ch = fake_channel();
        for method in ["resources/read", "resources/subscribe"] {
            let r = call(
                &ch,
                json!({"jsonrpc":"2.0","id":1,"method": method,
                       "params":{"uri":"channel://other"}}),
            );
            assert!(
                r.get("error").is_some(),
                "{method} 未知资源必须回 error：{r}"
            );
        }
    }

    /// 未知方法要回 error（不是静默成功）。
    #[test]
    fn unknown_method_returns_error() {
        let r = call(
            &fake_channel(),
            json!({"jsonrpc":"2.0","id":9,"method":"nope"}),
        );
        assert!(r.get("error").is_some(), "未知方法必须回 error");
    }

    /// 通知（无 id）不回包：回了会让对端的 id→响应匹配错位。
    #[test]
    fn notification_gets_no_response() {
        let ch = fake_channel();
        assert!(handle(
            &ch,
            &json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{}})
        )
        .is_none());
    }

    /// **先收到消息、后订阅**时不能丢：靠 `notify_pending` 让主循环补一条通知。
    #[test]
    fn late_subscribe_still_notifies_about_buffered_messages() {
        let ch = fake_channel();
        ch.push_inbound(json!({"chat_id":"c","message_id":"m"}));
        let r = call(
            &ch,
            json!({"jsonrpc":"2.0","id":1,"method":"resources/subscribe",
                   "params":{"uri": INBOX_URI}}),
        );
        assert!(r.get("error").is_none(), "订阅不该失败：{r}");
        assert!(
            ch.notify_pending.load(Ordering::SeqCst),
            "缓冲里已有消息 ⇒ 必须置待补通知标记（否则这批消息永远等不到 read）"
        );
    }

    /// 缓冲满时丢**最旧**的，不是丢最新的。
    #[test]
    fn inbox_cap_drops_oldest() {
        let ch = fake_channel();
        for i in 0..(INBOX_CAP + 1) {
            ch.push_inbound(json!({"chat_id":"c","message_id": i}));
        }
        let got = ch.take_inbox();
        assert_eq!(got.len(), INBOX_CAP);
        assert_eq!(got[0]["message_id"], json!(1), "丢的应该是最旧那条");
    }

    /// 群聊回复 @ 最近提问者；单聊不 @；没有记录也不 @。
    #[test]
    fn at_user_only_for_group_with_known_sender() {
        let ch = fake_channel();
        ch.note_sender("cid_abc", "staff_1");
        assert_eq!(at_user_for(&ch, "cid_abc").as_deref(), Some("staff_1"));
        assert_eq!(at_user_for(&ch, "staff_1"), None, "单聊不需要 @");
        assert_eq!(at_user_for(&ch, "cid_unknown"), None, "没记录就别猜");
    }

    /// 外部传进来的附件按 kind + 后缀分派到图片/文件模板；不支持的组合明确报错。
    #[test]
    fn send_plan_matches_dingtalk_capability_gate() {
        assert!(matches!(
            send_plan("image", "a.PNG").unwrap(),
            SendPlan::Image
        ));
        assert!(matches!(
            send_plan("file", "报表.XLSX").unwrap(),
            SendPlan::File("xlsx")
        ));
        // kind 说文件但后缀是图片 ⇒ 按图片发（sampleFile 白名单里没有 png）
        assert!(matches!(
            send_plan("file", "a.png").unwrap(),
            SendPlan::Image
        ));
        let e = send_plan("image", "a.heic").unwrap_err();
        assert!(e.contains("仅支持"), "图片白名单要报清楚：{e}");
        let e = send_plan("file", "a.csv").unwrap_err();
        assert!(e.contains("仅支持"), "文件白名单要报清楚：{e}");
        let e = send_plan("video", "a.mp4").unwrap_err();
        assert!(e.contains("暂不支持"), "不支持的类型要报清楚：{e}");
    }

    /// 非法 base64 与缺字段都要报错，不能发空文件。
    #[test]
    fn attachment_argument_errors() {
        let ch = fake_channel();
        let e = call_tool(
            &ch,
            &json!({"name":"send_attachment","arguments":{
                "chat_id":"c","name":"x.png","data_b64":"!!!!"}}),
        )
        .unwrap_err();
        assert!(e.contains("合法 base64"), "报因要说清是编码问题：{e}");
        let e = call_tool(
            &ch,
            &json!({"name":"send_attachment","arguments":{"chat_id":"c","name":"x"}}),
        )
        .unwrap_err();
        assert!(e.contains("data_b64"), "报因要点名缺哪个字段：{e}");
    }

    /// 没有机器人码时**明确报错**（而不是发一条注定被平台拒的请求）。
    #[test]
    fn missing_robot_code_fails_loudly() {
        let ch = Arc::new(Channel {
            client: DingTalkClient::new("app", "secret"),
            robot_code: String::new(),
            inbox: Mutex::new(Vec::new()),
            subscribed: Mutex::new(Vec::new()),
            notify_pending: AtomicBool::new(false),
            last_sender: Mutex::new(HashMap::new()),
            out: Mutex::new(std::io::stdout()),
        });
        let e = call_tool(
            &ch,
            &json!({"name":"send_text","arguments":{"chat_id":"c","text":"hi"}}),
        )
        .unwrap_err();
        assert!(e.contains("机器人码"), "报因要说清缺什么：{e}");
    }

    /// 空 filenames 按 mime 补后缀：上传按后缀校验格式，没名字就必然被拒。
    #[test]
    fn upload_name_falls_back_to_mime() {
        assert_eq!(upload_name("pic.png", "", "image"), "pic.png");
        assert_eq!(upload_name("", "application/pdf", "file"), "attachment.pdf");
        assert_eq!(upload_name("", "", "image"), "attachment.png");
        assert_eq!(upload_name("", "", "file"), "attachment.bin");
    }

    /// 分段：短文本原样一条；多行按行切**尽量不切断代码块**；单行超限硬切。
    #[test]
    fn split_text_keeps_short_text_whole_and_respects_fences() {
        assert_eq!(split_text("hello", 8000), vec!["hello".to_string()]);
        // 单行超限 ⇒ 硬切（否则超额 chunk 会被平台整条拒）
        let long = "a".repeat(20);
        let chunks = split_text(&long, 5);
        assert!(chunks.len() > 1, "超长必须分段：{chunks:?}");
        assert_eq!(chunks.concat(), long, "分段不许丢字符");
        // 小代码块整体超 limit 但**行都不超** ⇒ 整块保留（in_fence 拦住切点）
        let fenced = "```\nbb\ncc\n```\n";
        assert_eq!(fenced.chars().count(), 14);
        let chunks = split_text(fenced, 10);
        assert_eq!(chunks, vec![fenced.to_string()], "小代码块应整块保留");
        // 单行本身超过 limit 时**连 fence 也硬切**：超额 chunk 会被平台整条拒，
        // 两害相权宁可拆块（见 split_text 函数注释）。
        let big = format!("```\n{}\n```\n", "b".repeat(30));
        let chunks = split_text(&big, 10);
        assert!(chunks.len() > 1, "单行超限必须硬切：{chunks:?}");
        assert_eq!(chunks.concat(), big, "硬切同样不许丢字符");
        // 多行文本按行切，不切碎单行
        let lines = format!("{}\n{}\n", "x".repeat(8), "y".repeat(8));
        let chunks = split_text(&lines, 10);
        assert_eq!(
            chunks,
            vec![
                format!("{}\n", "x".repeat(8)),
                format!("{}\n", "y".repeat(8))
            ]
        );
    }

    /// base64 解码与 ABB 的编码对称（含 padding 的三种情形）。
    #[test]
    fn base64_roundtrip() {
        for s in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
            let enc = {
                let data = s.as_bytes();
                let mut e = String::new();
                const T: &[u8; 64] =
                    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
                for ch in data.chunks(3) {
                    let b = [ch[0], *ch.get(1).unwrap_or(&0), *ch.get(2).unwrap_or(&0)];
                    let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
                    e.push(T[(n >> 18) as usize & 63] as char);
                    e.push(T[(n >> 12) as usize & 63] as char);
                    e.push(if ch.len() > 1 {
                        T[(n >> 6) as usize & 63] as char
                    } else {
                        '='
                    });
                    e.push(if ch.len() > 2 {
                        T[n as usize & 63] as char
                    } else {
                        '='
                    });
                }
                e
            };
            assert_eq!(
                base64_decode(&enc).as_deref(),
                Some(s.as_bytes()),
                "往返失败：{s:?} → {enc:?}"
            );
        }
        assert_eq!(base64_decode("!!!!"), None);
        assert_eq!(base64_decode("abc"), None, "长度不是 4 的倍数");
    }
}
