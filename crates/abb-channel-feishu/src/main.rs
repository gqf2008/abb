//! 飞书渠道的 MCP server 主程序。
//!
//! ## 与 ABB 的关系
//!
//! ABB 是总线，本进程是它拉起的**渠道服务**之一。契约见
//! `docs/mcp-bus-architecture.md` §3：
//!
//! | 方向 | 契约 | 本进程的实现 |
//! |---|---|---|
//! | 出站 | 工具 `send_text`（必需） | ✅ 走 `feishu::FeishuClient::send_text` |
//! | 出站 | 工具 `send_thread_reply`（可选） | ✅ 飞书**有话题** ⇒ 声明并走 `reply_text` |
//! | 出站 | 工具 `send_attachment`（可选） | ✅ base64 内联 → 上传 → 媒体消息 |
//! | 出站 | 工具 `get_quoted_message` / `create_chat` / `typing` / `del_typing` / `done` / `chat_brief` / `get_chat_info` / `user_display_name` / `download_attachment`（均为可选） | ✅ 见「可选能力」一节 |
//! | 入站 | `poll` 或 `channel://` 资源订阅 | ✅ **资源订阅**（`channel://inbox`），见下 |
//!
//! ## 可选能力（契约 §3.4）
//!
//! ABB 侧全部走 `call_optional`：**缺失 / 调用失败 / 返回非 JSON ⇒ 一律 `None`**，
//! 由 `Messenger` trait 默认值兜底，不阻断主流程。所以这些工具出错**不必假装成功**——
//! 如实回 JSON-RPC `error`，日志里能看见真因，ABB 那边只是回落默认行为。
//!
//! | 工具 | 底层 | 备注 |
//! |---|---|---|
//! | `get_quoted_message` | `get_quoted_message` | 回 `{text, msg_type}` |
//! | `download_attachment` | `download_resource` | 参数只有 `{message_id, seq}`，file_key 由渠道反查（见下）|
//! | `create_chat` | `create_chat` | 群主=bot、owner 拉进群并设管理员 |
//! | `user_display_name` | `user_name` | 通讯录 scope 缺失时报错（ABB 用 id 兜底）|
//! | `typing` / `done` | `add_reaction` | emoji 与内置渠道一致：`Typing` / `DONE` |
//! | `del_typing` | `del_reaction` | `reaction_id` 可为 null（无表情可撤）|
//! | `chat_brief` | `chat_brief` | 回 `{chat_type, chat_name}` |
//! | `get_chat_info` | `get_chat_info` | 回 `{name, description}` |
//!
//! **为什么本渠道没有 `note_context` / `note_sender`**：那两项服务于微信（回复要回显
//! `context_token`）与钉钉（群聊 @ 要记住最近发送者）。飞书的消息事件自带
//! `message_id` / `chat_id` / `sender_id`，发送不需要回显凭证，也没有这两个状态。
//! 声明一个内部空转的工具只会让 ABB 误以为它有语义。
//!
//! ## `download_attachment` 怎么在只有 `{message_id, seq}` 的条件下拿到 file_key
//!
//! 契约刻意不给 `file_key`（§4「ABB 不负责协议细节」），飞书这边也不需要本地缓存：
//! 用 `GET /im/v1/messages/:id` 反查那条消息的 `body.content`，`parse_content` 抽出的
//! `resources` 顺序**与入站时 `attachments` 的顺序完全一致**（同一份解析函数），
//! 所以 `seq` 直接就是下标。
//!
//! `seq >= 100` 是 ABB 的**引用消息约定**（`bridge/mod.rs` 的 `100 + i`，与内置渠道同口径）：
//! 那批附件属于「被回复的那条」，而传进来的 `message_id` 是**回复消息**的 id，
//! 所以先读它的 `parent_id` 再拉原消息，取 `seq - 100`。
//! 下载落盘目录默认 `<tmp>/abb-channel-feishu/attachments`，可用
//! `ABB_FEISHU_DOWNLOAD_DIR` 覆盖（ABB 的 `mcp_channel.env` 下发）。
//!
//! ## 为什么入站选「订阅」而不是 `poll`
//!
//! 微信那条路能用 `poll`，是因为它的 `get_updates(cursor)` 本来就是个**游标长轮询**——
//! 契约的 `poll` 是照它长的。飞书不是：它的入站是**服务端推的长连接**（WS），
//! 本地没有「按游标取一批」的原语。若强行做成 `poll`，每次调用要么空转（无消息），
//! 要么得在进程里再造一层缓冲——**而缓冲本来就是要给订阅用的**，那就直接按订阅做：
//!
//! ```text
//!   飞书云 ──WS──► 本进程（缓冲 Vec<消息>）──notifications/resources/updated──► ABB
//!                                              ABB 收到通知 ──resources/read──► 取走并清空
//! ```
//!
//! 裁决记录：Jev `subscribe_push` p=0.81 / confidence=0.62
//! （rid `gen-dec-1793692596-P6YzJvS4Qo1baj0dfbY8`）。注意 p=0.81 是概率不是断言，
//! conf=0.62 表示它自己也只算「偏向」——真正的依据是上面那条「飞书本地没有游标原语」。
//!
//! ## 为什么飞书比钉钉先搬
//!
//! 依赖面小：飞书的出站只需 `tenant_access_token` + 三条 IM 接口（`messages` /
//! `messages/:id/reply` / `images` / `files`），全部是普通 HTTPS + JSON/multipart，
//! 没有钉钉那套「加签（HMAC）+ 回调加解密（AES）+ Stream 模式 SDK」的额外机制，
//! 搬到独立进程时**协议层可以整份搬、只换三处依赖**（见 `src/feishu.rs` 文件头）。
//! 依赖少 ⇒ 搬迁的失败面小 ⇒ 先搬它，钉钉按同一路径随后。
//!
//! ## 三个必须做对的地方
//!
//! 1. **stdout 只能出 JSON-RPC**。任何一行杂输出都会让 ABB 侧的解析炸掉，
//!    所以日志一律 `eprintln!`（`feishu.rs` 里的那四处 `log!` 搬迁时都换成了 `eprintln!`）。
//! 2. **`resources/read` 必须清空缓冲**。不清的话 ABB 每次通知都会把历史消息重放一遍
//!    ——同一条消息被回答多次比丢消息更难查。
//! 3. **入站失败不许拖死契约层**。WS 连不上（凭证错/网络不通）只打 stderr 警告，
//!    `tools/list` / `send_text` 照常工作——ABB 至少还能发消息、还能看到真实报错。
//!
//! ## 凭证
//!
//! 只从环境变量拿 `FS_APP_ID` / `FS_APP_SECRET`（ABB 的 `mcp_channel.env` 下发）。
//! 本进程**不认识 ABB 的 config.json**——渠道配置由提供方决定。

mod feishu;
mod proto;
mod wsin;

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use feishu::FeishuClient;
use serde_json::{json, Value};

/// 「处理中」表情。与内置渠道同一个 emoji（`src/messenger.rs` 的 `typing`），
/// 换名字会让用户看到两种回执，而对端无从区分。
const EMOJI_TYPING: &str = "Typing";
/// 「完成」表情（同上）。
const EMOJI_DONE: &str = "DONE";

/// 引用消息附件的 `seq` 偏移：ABB 用 `100 + i` 表示「被引用那条消息的第 i 个附件」。
/// 这是**跨渠道约定**（`bridge/mod.rs`），渠道侧不能自己改。
const QUOTED_SEQ_BASE: usize = 100;
/// 下载落盘目录的环境变量名（ABB 的 `mcp_channel.env` 下发；缺省走临时目录）。
const DOWNLOAD_DIR_ENV: &str = "ABB_FEISHU_DOWNLOAD_DIR";

/// 入站资源的 URI（契约 §3.2 的 A 方案，整渠道一个 inbox）。
pub const INBOX_URI: &str = "channel://inbox";

/// 缓冲上限（条）。ABB 每轮 read 都会取走全部，正常不会积累到这个数；
/// 上限只为「ABB 崩了/没来读」时**不无限吃内存**——超了丢最旧的并打 stderr。
const INBOX_CAP: usize = 200;

/// 渠道进程的共享状态。
struct Channel {
    /// 出站用的飞书客户端（`tenant_access_token` 缓存在它里面）。
    client: FeishuClient,
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
    /// stdout 的唯一出口。主循环回包与 WS 线程推通知都走它 ⇒ **行不会交错**。
    out: Mutex<std::io::Stdout>,
}

impl Channel {
    /// 写一行 JSON 到 stdout 并 flush（NDJSON）。
    ///
    /// 锁中毒不 panic：这条路径的失败方式是「日志里少一行」，不该让整个渠道进程死。
    fn write_line(&self, v: &Value) {
        let mut g = self.out.lock().unwrap_or_else(|e| e.into_inner());
        if serde_json::to_writer(&mut *g, v).is_err() || g.write_all(b"\n").is_err() {
            eprintln!("[feishu-mcp] 写 stdout 失败，放弃这一行");
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

    /// WS 线程的入口：塞进缓冲，然后（若已订阅）推一条「有更新」。
    fn push_inbound(&self, m: Value) {
        {
            let mut g = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
            if g.len() >= INBOX_CAP {
                g.remove(0);
                eprintln!("[feishu-mcp] 入站缓冲满（{INBOX_CAP}），丢弃最旧一条");
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
    let app_id = std::env::var("FS_APP_ID").unwrap_or_default();
    let app_secret = std::env::var("FS_APP_SECRET").unwrap_or_default();
    eprintln!(
        "[feishu-mcp] 启动 app_id={} secret={}",
        if app_id.is_empty() {
            "(空)".to_string()
        } else {
            mask(&app_id)
        },
        if app_secret.is_empty() {
            "空"
        } else {
            "已设置"
        }
    );

    let ch = Arc::new(Channel {
        client: FeishuClient::new(&app_id, &app_secret),
        inbox: Mutex::new(Vec::new()),
        subscribed: Mutex::new(Vec::new()),
        notify_pending: AtomicBool::new(false),
        out: Mutex::new(std::io::stdout()),
    });

    if app_id.is_empty() || app_secret.is_empty() {
        eprintln!(
            "[feishu-mcp] 未提供 FS_APP_ID / FS_APP_SECRET —— 入站长连接不启动，\
             出站调用也会被飞书拒。把这两项写进 ABB 的 mcp_channel.env 后重启。"
        );
    } else {
        // 入站：WS 长连接跑在独立线程（自己的 tokio runtime）。
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
            eprintln!("[feishu-mcp] 非法 JSON，忽略");
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
    eprintln!("[feishu-mcp] stdin 关闭，退出");
}

/// 只露 app_id 的头尾：它在日志里出现得频繁，但完整值仍属配置。
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
            "serverInfo": {"name": "abb-channel-feishu", "version": env!("CARGO_PKG_VERSION")},
        })),
        "tools/list" => Ok(json!({"tools": tools()})),
        "resources/list" => Ok(json!({
            "resources": [{
                "uri": INBOX_URI,
                "name": "inbox",
                "mimeType": "application/json",
                "description": "飞书入站消息缓冲（订阅后收到 updated 通知再 read）",
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
            "资源 `{uri}` 不存在；本渠道只有 `{INBOX_URI}`（飞书整渠道一个 inbox）"
        ));
    }
    ch.subscribe(uri);
    eprintln!("[feishu-mcp] 已订阅 {uri}");
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
            "description": "发文本到飞书会话（超长自动分段，卡片失败自动降级纯文本）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat_id": {"type": "string", "description": "飞书 chat_id（oc_…）"},
                    "text": {"type": "string"}
                },
                "required": ["chat_id", "text"]
            }
        },
        {
            "name": "send_thread_reply",
            "description": "在指定消息的话题里回复（飞书有话题概念；走 /im/v1/messages/:id/reply）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat_id": {"type": "string", "description": "会话键（仅作上下文，回复按 message_id 定位）"},
                    "message_id": {"type": "string", "description": "要回复的消息 id（om_…）"},
                    "text": {"type": "string"}
                },
                "required": ["chat_id", "message_id", "text"]
            }
        },
        {
            "name": "send_attachment",
            "description": "发媒体到飞书会话（图片内联 / 其它走文件卡片；内容 base64 内联）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat_id": {"type": "string", "description": "飞书 chat_id（oc_…）"},
                    "name": {"type": "string", "description": "文件名（用于判断图片/文件与上传类型）"},
                    "mime": {"type": "string"},
                    "kind": {"type": "string", "description": "image | file"},
                    "data_b64": {"type": "string", "description": "文件内容的 base64"}
                },
                "required": ["chat_id", "name", "data_b64"]
            }
        },
        {
            "name": "get_quoted_message",
            "description": "按 message_id 拉一条历史消息的全文与类型（引用/回复场景读上文）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message_id": {"type": "string", "description": "被引用的消息 id（om_…）"}
                },
                "required": ["message_id"]
            }
        },
        {
            "name": "download_attachment",
            "description": "下载入站消息的第 seq 个附件到本进程的下载目录，返回本地路径",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message_id": {"type": "string", "description": "消息 id（om_…）"},
                    "seq": {
                        "type": "integer",
                        "description": "第几个附件（从 0 起）；>= 100 表示被引用消息的第 seq-100 个"
                    }
                },
                "required": ["message_id", "seq"]
            }
        },
        {
            "name": "create_chat",
            "description": "建群（群主=机器人，owner 拉进群并设为管理员），返回 chat_id",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "群名"},
                    "description": {"type": "string", "description": "群介绍（可为空）"},
                    "owner_user_id": {
                        "type": "string",
                        "description": "owner 的 open_id（ou_…）：会被拉进群并设为管理员"
                    }
                },
                "required": ["name", "owner_user_id"]
            }
        },
        {
            "name": "update_chat",
            "description": "改群资料（群名 / 群介绍）；群主转让需走平台客户端",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat_id": {"type": "string"},
                    "name": {"type": "string"},
                    "description": {"type": "string"}
                },
                "required": ["chat_id", "name"]
            }
        },
        {
            "name": "bot_info",
            "description": "查本渠道 bot 自己的名称与 open_id（GUI「拉取」按钮用）",
            "inputSchema": {"type": "object", "properties": {}, "required": []}
        },
        {
            "name": "delete_chat",
            "description": "解散群（**不可恢复**；调用方须先做强确认）",
            "inputSchema": {
                "type": "object",
                "properties": {"chat_id": {"type": "string"}},
                "required": ["chat_id"]
            }
        },
        {
            "name": "user_display_name",
            "description": "按 open_id 反查用户显示名（需通讯录 scope）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "user_id": {"type": "string", "description": "用户 open_id（ou_…）"}
                },
                "required": ["user_id"]
            }
        },
        {
            "name": "typing",
            "description": "给消息加「处理中」表情，返回 reaction_id（供 del_typing 撤销）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message_id": {"type": "string"}
                },
                "required": ["message_id"]
            }
        },
        {
            "name": "del_typing",
            "description": "撤掉 typing 加上的「处理中」表情（reaction_id 为 null 时无事可做，算成功）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message_id": {"type": "string"},
                    "reaction_id": {"type": ["string", "null"]}
                },
                "required": ["message_id"]
            }
        },
        {
            "name": "done",
            "description": "给消息加「完成」表情（处理结束的可见回执）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message_id": {"type": "string"}
                },
                "required": ["message_id"]
            }
        },
        {
            "name": "chat_brief",
            "description": "查会话类型与名字（历史回填用）；chat_type = p2p | group",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat_id": {"type": "string", "description": "飞书 chat_id（oc_…）"}
                },
                "required": ["chat_id"]
            }
        },
        {
            "name": "get_chat_info",
            "description": "查群资料（群名 + 群介绍）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat_id": {"type": "string", "description": "飞书 chat_id（oc_…）"}
                },
                "required": ["chat_id"]
            }
        }
    ])
}

/// 图片走内联（`msg_type=image`）还是文件卡片（`msg_type=file`）。
///
/// 判定必须看**扩展名**而不只是 `kind`：飞书 images 端点对格式有硬校验，
/// 被拒时整个附件都会失败（ABB 侧 `feishu_send_plan` 同口径，审查 #254）。
/// 名字为空时按 mime 补一个——上传带真实文件名，服务端按后缀校验格式。
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

/// 飞书平台的外发体积上限（与 ABB `attachments.rs` 同值）。
/// 内联 base64 后体积还要 ×4/3，所以本地先兜一道，别等上传被服务端拒。
const FEISHU_IMAGE_MAX_BYTES: usize = 10 * 1024 * 1024;
const FEISHU_FILE_MAX_BYTES: usize = 30 * 1024 * 1024;

/// 工具结果的文本段：把结果 JSON 序列化进 `content[0].text`。
///
/// ABB 侧 `call_optional` 拿到文本后就是 `serde_json::from_str` —— 所以这里必须是
/// **JSON 文本**（不是自然语言描述），否则对端只能当作「不支持」。
fn json_text(v: Value) -> Value {
    json!({"content": [{"type": "text", "text": v.to_string()}]})
}

/// 取必填字符串参数。空串与纯空白都算缺：空 `chat_id` / `message_id` 只会换来一个
/// 费解的飞书 API 错误码，不如在这里就把名字说清楚。
fn need<'a>(a: &'a Value, tool: &str, key: &str) -> Result<&'a str, String> {
    a.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{tool} 缺 `{key}`"))
}

/// 取可选字符串参数（缺失与 `null` 都当空串）。
fn opt(a: &Value, key: &str) -> String {
    a.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// 把 `{message_id, seq}` 解析成「哪个 file_key、属于哪条消息」。
///
/// 飞书契约刻意不给 `file_key`（怎么下载是渠道的事），但也不需要本地缓存消息：
/// 用 `GET /im/v1/messages/:id` 反查 `body.content`，`parse_content` 抽出的 `resources`
/// 顺序与入站 `attachments` 完全一致（同一份解析函数），`seq` 直接就是下标。
///
/// `seq >= 100` 是 ABB 的引用消息约定（比回复消息自己多 100，`bridge/mod.rs` 的 `100 + i`）：
/// 那批附件属于**被回复的那条**，而传进来的 `message_id` 是回复消息的 id。
async fn resolve_resource(
    ch: &Arc<Channel>,
    message_id: &str,
    seq: usize,
) -> Result<(feishu::FeishuResource, String), String> {
    let (owner_mid, parsed, idx) = if seq < QUOTED_SEQ_BASE {
        let m = ch
            .client
            .get_message(message_id)
            .await
            .map_err(|e| format!("拉消息 {message_id} 失败：{e:#}"))?;
        (message_id.to_string(), m.parsed, seq)
    } else {
        let m = ch
            .client
            .get_message(message_id)
            .await
            .map_err(|e| format!("拉消息 {message_id} 失败：{e:#}"))?;
        if m.parent_id.is_empty() {
            return Err(format!(
                "seq={seq} 表示**被引用消息**的附件，但消息 {message_id} 不是回复消息\
                 （事件/接口都没给 parent_id）——是不是把索引算错了？"
            ));
        }
        let parent = ch
            .client
            .get_message(&m.parent_id)
            .await
            .map_err(|e| format!("拉被引用消息 {} 失败：{e:#}", m.parent_id))?;
        (m.parent_id, parent.parsed, seq - QUOTED_SEQ_BASE)
    };
    let res = parsed.resources.get(idx).cloned().ok_or_else(|| {
        format!(
            "消息 {owner_mid} 只有 {} 个附件，没有第 {idx} 个（seq 从 0 起）",
            parsed.resources.len()
        )
    })?;
    Ok((res, owner_mid))
}

/// 下载落盘目录：`ABB_FEISHU_DOWNLOAD_DIR` 优先，否则临时目录下的固定子目录。
fn download_dir() -> std::path::PathBuf {
    match std::env::var(DOWNLOAD_DIR_ENV) {
        Ok(d) if !d.trim().is_empty() => std::path::PathBuf::from(d),
        _ => std::env::temp_dir().join("abb-channel-feishu"),
    }
}

/// 消息 id 消毒（只用于拼文件名）：只留字母数字与 `-_.`。
fn sanitize_mid(mid: &str) -> String {
    let s: String = mid
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        "unknown".to_string()
    } else {
        s
    }
}

/// 文件名消毒：**发送方能完全控制这个字符串** ⇒ 不消毒就能用 `../../x` 写到目录外。
/// 只取末段（`/` 与 `\\` 都算分隔符）、去控制字符、去前导点（`.` / `..` / 隐藏文件）。
/// 中文等非 ASCII 字符保留——它们是文件名的一部分，不是风险。
fn sanitize_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    base.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .trim_start_matches('.')
        .to_string()
}

/// 按 mime 补一个扩展名（只在发送方没给文件名时用）。
fn ext_from_mime(mime: &str) -> &'static str {
    let base = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match base.as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/ogg" => "ogg",
        "video/mp4" => "mp4",
        _ => "bin",
    }
}

/// 附件的落盘文件名：优先用发送方给的名字（消毒过），没有就按 mime / `seq` 补。
fn download_file_name(res: &feishu::FeishuResource, seq: usize, mime: &str) -> String {
    let given = sanitize_name(res.file_name.trim());
    if !given.is_empty() {
        return given;
    }
    let stem = if res.kind == "image" { "image" } else { "file" };
    format!("{stem}_{seq}.{}", ext_from_mime(mime))
}

/// 把下载到的字节写到 `download_dir()/attachments/<mid>_<seq>_<name>`。
fn save_download(
    message_id: &str,
    seq: usize,
    file_name: &str,
    bytes: &[u8],
) -> Result<String, String> {
    let dir = download_dir().join("attachments");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("建下载目录失败（{}）：{e}", dir.display()))?;
    let path = dir.join(format!("{}_{seq}_{file_name}", sanitize_mid(message_id)));
    std::fs::write(&path, bytes).map_err(|e| format!("写附件失败（{}）：{e}", path.display()))?;
    Ok(path.display().to_string())
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
            let rt = rt();
            rt.block_on(ch.client.send_text(chat_id, text))
                .map_err(|e| format!("send_text 失败：{e:#}"))?;
            Ok(json!({"content": [{"type": "text", "text": "ok"}]}))
        }
        "send_thread_reply" => {
            // `chat_id` 是契约的必需参数（ABB 总会带），但飞书的 reply 接口只认
            // message_id——这里不拿它做事，只留一行日志便于对账。
            let chat_id = a.get("chat_id").and_then(|v| v.as_str()).unwrap_or("");
            let message_id = a.get("message_id").and_then(|v| v.as_str()).unwrap_or("");
            let text = a.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if message_id.is_empty() {
                return Err("send_thread_reply 缺 message_id（话题回复按消息 id 定位）".into());
            }
            if text.is_empty() {
                return Err("send_thread_reply 缺 text".into());
            }
            eprintln!("[feishu-mcp] 话题回复 chat_id={chat_id} message_id={message_id}");
            let rt = rt();
            rt.block_on(ch.client.reply_text(message_id, text))
                .map_err(|e| format!("send_thread_reply 失败：{e:#}"))?;
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
            let data = base64_decode(b64).ok_or_else(|| "data_b64 不是合法 base64".to_string())?;
            let fname = upload_name(name, mime, kind);
            let as_image = kind == "image" && feishu::feishu_image_uploadable(&fname);
            let cap = if as_image {
                FEISHU_IMAGE_MAX_BYTES
            } else {
                FEISHU_FILE_MAX_BYTES
            };
            if data.len() > cap {
                return Err(format!(
                    "附件过大（{} MB > 平台上限 {} MB）：{fname}",
                    data.len() / (1024 * 1024),
                    cap / (1024 * 1024)
                ));
            }
            let rt = rt();
            rt.block_on(async {
                if as_image {
                    let key = ch.client.upload_image(data, &fname).await?;
                    ch.client.send_media_message(chat_id, "image", &key).await
                } else {
                    let ft = feishu::feishu_file_type(&fname);
                    let key = ch.client.upload_file(ft, &fname, data).await?;
                    ch.client.send_media_message(chat_id, "file", &key).await
                }
            })
            .map_err(|e| format!("send_attachment 失败：{e:#}"))?;
            Ok(json!({"content": [{"type": "text", "text": "ok"}]}))
        }
        // ── 契约 §3.4 的可选能力 ────────────────────────────────────────
        //
        // 这些工具的错误都用 `Err` 上报（不是静默空串）：ABB 侧 `call_optional` 会把它
        // 记成一行日志并回落 trait 默认值，所以「如实报错」对它无害，而「假装成功」
        // （比如回空 path）反而会把一次下载失败变成一个读不到的假文件。
        "get_quoted_message" => {
            let message_id = need(&a, "get_quoted_message", "message_id")?;
            let parsed = rt()
                .block_on(ch.client.get_quoted_message(message_id))
                .map_err(|e| format!("get_quoted_message 失败：{e:#}"))?;
            // `msg_type` 是「为什么读不到内容」的唯一线索（merge_forward/sticker/卡片），
            // 文本为空时也得交出去。
            // 未识别的富文本元素同样落 stderr：不带这行就只能看到「内容为空」而猜不原因。
            if !parsed.unparsed_tags.is_empty() {
                eprintln!(
                    "[feishu-mcp] 引用消息 {message_id} 含未识别元素 {:?}（可能因此读不全）",
                    parsed.unparsed_tags
                );
            }
            Ok(json_text(json!({
                "text": parsed.text,
                "msg_type": parsed.msg_type,
            })))
        }
        "download_attachment" => {
            let message_id = need(&a, "download_attachment", "message_id")?;
            let seq = a.get("seq").and_then(|v| v.as_u64()).ok_or_else(|| {
                "download_attachment 缺 `seq`（第几个附件，从 0 起；>=100 表示被引用消息）"
                    .to_string()
            })? as usize;
            let rt = rt();
            let (res, owner_mid) = rt.block_on(resolve_resource(ch, message_id, seq))?;
            let (bytes, mime) = rt
                .block_on(
                    ch.client
                        .download_resource(&owner_mid, &res.file_key, &res.kind),
                )
                .map_err(|e| format!("download_attachment 下载失败：{e:#}"))?;
            let mime = if mime.trim().is_empty() {
                "application/octet-stream".to_string()
            } else {
                mime
            };
            let file_name = download_file_name(&res, seq, &mime);
            let path = save_download(message_id, seq, &file_name, &bytes)?;
            eprintln!(
                "[feishu-mcp] 附件已下载 mid={message_id} seq={seq} kind={} size={} → {path}",
                res.kind,
                bytes.len()
            );
            Ok(json_text(json!({
                "kind": res.kind,
                "file_name": file_name,
                "mime": mime,
                "size": bytes.len(),
                "path": path,
            })))
        }
        "create_chat" => {
            let name = need(&a, "create_chat", "name")?;
            let description = opt(&a, "description");
            // owner 不能为空：群主是机器人，owner 靠 `user_id_list` 被拉进群
            // （不传的话用户在飞书里根本看不到这个群，实测踩过）。
            let owner = need(&a, "create_chat", "owner_user_id")?;
            let chat_id = rt()
                .block_on(ch.client.create_chat(name, &description, owner))
                .map_err(|e| format!("create_chat 失败：{e:#}"))?;
            eprintln!("[feishu-mcp] 已建群 {chat_id} name={name} owner={owner}");
            Ok(json_text(json!({"chat_id": chat_id})))
        }
        "update_chat" => {
            let cid = a.get("chat_id").and_then(|v| v.as_str()).unwrap_or("");
            let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let desc = a.get("description").and_then(|v| v.as_str()).unwrap_or("");
            if cid.is_empty() {
                return Err("update_chat 需要 chat_id".to_string());
            }
            if name.trim().is_empty() {
                // 空群名会被飞书拒（400）且报错含糊，这里先挡。
                return Err("update_chat 需要非空 name".to_string());
            }
            rt().block_on(ch.client.update_chat(cid, name, desc))
                .map_err(|e| format!("改群资料失败：{e:#}"))?;
            Ok(json!({"content": [{"type": "text", "text": "{}"}]}))
        }
        "bot_info" => {
            let (name, oid) = rt()
                .block_on(ch.client.bot_info())
                .map_err(|e| format!("查 bot 资料失败：{e:#}"))?;
            Ok(json!({"content": [{"type": "text",
                "text": json!({"name": name, "open_id": oid}).to_string()}]}))
        }
        "delete_chat" => {
            let cid = a.get("chat_id").and_then(|v| v.as_str()).unwrap_or("");
            if cid.is_empty() {
                return Err("delete_chat 需要 chat_id".to_string());
            }
            rt().block_on(ch.client.delete_chat(cid))
                .map_err(|e| format!("解散群失败：{e:#}"))?;
            Ok(json!({"content": [{"type": "text", "text": "{}"}]}))
        }
        "user_display_name" => {
            let user_id = need(&a, "user_display_name", "user_id")?;
            let name = rt().block_on(ch.client.user_name(user_id)).ok_or_else(|| {
                format!(
                    "user_display_name 查不到 {user_id} 的姓名\
                         （缺通讯录 scope、用户离职或机器人看不到该用户）"
                )
            })?;
            Ok(json_text(json!({"name": name})))
        }
        "typing" => {
            let message_id = need(&a, "typing", "message_id")?;
            let reaction_id = rt()
                .block_on(ch.client.add_reaction(message_id, EMOJI_TYPING))
                .ok_or_else(|| {
                    format!(
                        "typing 加表情（{EMOJI_TYPING}）失败\
                         ：消息不可见 / 缺 im:message.reaction 权限"
                    )
                })?;
            Ok(json_text(json!({"reaction_id": reaction_id})))
        }
        "del_typing" => {
            let message_id = need(&a, "del_typing", "message_id")?;
            // `reaction_id` 允许 null（ABB 的 `Option<String>`）：没加成功过就无事可撤，
            // 算成功——这跟「删不掉却报成功」不是一回事（真给了 id 才真去删）。
            let reaction_id = opt(&a, "reaction_id");
            rt().block_on(ch.client.del_reaction(message_id, &reaction_id));
            Ok(json_text(json!({})))
        }
        "done" => {
            let message_id = need(&a, "done", "message_id")?;
            if rt()
                .block_on(ch.client.add_reaction(message_id, EMOJI_DONE))
                .is_none()
            {
                return Err(format!(
                    "done 加表情（{EMOJI_DONE}）失败：消息不可见 / 缺 im:message.reaction 权限"
                ));
            }
            Ok(json_text(json!({})))
        }
        "chat_brief" => {
            let chat_id = need(&a, "chat_brief", "chat_id")?;
            let (chat_type, chat_name) = rt()
                .block_on(ch.client.chat_brief(chat_id))
                .map_err(|e| format!("chat_brief 失败：{e:#}"))?;
            Ok(json_text(json!({
                "chat_type": chat_type,
                "chat_name": chat_name,
            })))
        }
        "get_chat_info" => {
            let chat_id = need(&a, "get_chat_info", "chat_id")?;
            let (name, description) = rt()
                .block_on(ch.client.get_chat_info(chat_id))
                .map_err(|e| format!("get_chat_info 失败：{e:#}"))?;
            Ok(json_text(json!({
                "name": name,
                "description": description,
            })))
        }
        other => Err(format!("未知工具 `{other}`")),
    }
}

/// 标准 base64 解码（含 padding）。返回 `None` 表示输入非法。
///
/// 手写不引 crate：与 ABB 侧 `mcp_channel::base64_encode` 对称，且
/// `crates/abb-channel-wechat` 里也是同一份实现（那边是私有函数，跨 crate 用不了）。
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

    /// 一条最小的入站消息（测试用）。`inbox` 存的是已序列化的 JSON（见 `Channel::inbox`）。
    fn sample_message() -> serde_json::Value {
        serde_json::json!({
            "chat_id": "c1", "message_id": "m1", "sender_id": "u1",
            "sender_name": "", "text": "hi", "ts": 1793600001, "attachments": [],
        })
    }

    fn fake_channel() -> Arc<Channel> {
        Arc::new(Channel {
            client: FeishuClient::new("app", "secret"),
            inbox: Mutex::new(Vec::new()),
            subscribed: Mutex::new(Vec::new()),
            notify_pending: AtomicBool::new(false),
            // 单测的 stdout 会被 `cargo test` 捕获，写进去不会污染真实协议通道。
            out: Mutex::new(std::io::stdout()),
        })
    }

    fn call(ch: &Arc<Channel>, req: Value) -> Value {
        handle(ch, &req).expect("要回包")
    }

    /// 所有工具必须全部声明。`send_thread_reply` 尤其重要：
    /// 飞书**有话题**，不声明它 ABB 就会把所有回复拍平到主聊天。
    /// 剩下的九个是契约 §3.4 的可选能力：ABB 侧缺了就回落 trait 默认，
    /// 但本渠道**能实现**，所以一个都不该漏。
    #[test]
    fn tools_list_declares_the_whole_contract_surface() {
        let names: Vec<String> = tools()
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            names,
            vec![
                "send_text",
                "send_thread_reply",
                "send_attachment",
                "get_quoted_message",
                "download_attachment",
                "create_chat",
                "update_chat",
                "bot_info",
                "delete_chat",
                "user_display_name",
                "typing",
                "del_typing",
                "done",
                "chat_brief",
                "get_chat_info",
            ],
            "声明面 = ABB 能看到的全部能力，改动要同步 README 与 tests/contract.rs"
        );
    }

    /// 每个工具都要有 `inputSchema.required`（空的也要是数组）：
    /// 只写名字不写 schema 等于让调用方猜参数名，猜错就是运行时错误。
    #[test]
    fn every_tool_declares_an_input_schema() {
        for t in tools().as_array().unwrap() {
            let name = t["name"].as_str().unwrap();
            assert_eq!(
                t["inputSchema"]["type"], "object",
                "{name} 的 schema 是对象"
            );
            assert!(
                t["inputSchema"]["required"].is_array(),
                "{name} 的 required 必须是数组（哪怕是空的）：{t}"
            );
            assert!(
                !t["description"].as_str().unwrap_or("").is_empty(),
                "{name} 缺 description（ABB 面板靠它展示能力）"
            );
        }
    }

    /// **结果必须是「`content[0].text` 里装一份 JSON」**：ABB 的 `call_optional`
    /// 拿到文本就 `serde_json::from_str`，解不了就当作「该渠道不支持」（静默丢能力）。
    #[test]
    fn tool_result_carries_json_in_text_segment() {
        let v = json_text(json!({"chat_id": "oc_1"}));
        assert_eq!(v["content"][0]["type"], "text");
        let text = v["content"][0]["text"].as_str().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(text).unwrap()["chat_id"],
            "oc_1"
        );
    }

    /// 必填参数缺失要**点名是哪个**（不是模糊的“参数错误”）。
    /// 这些路径在触网之前就返回，所以能离线跑。
    #[test]
    fn missing_required_arguments_name_the_field() {
        let ch = fake_channel();
        for (tool, args, want) in [
            ("get_quoted_message", json!({}), "message_id"),
            (
                "get_quoted_message",
                json!({"message_id": "  "}),
                "message_id",
            ),
            ("download_attachment", json!({}), "message_id"),
            ("download_attachment", json!({"message_id": "om_1"}), "seq"),
            ("create_chat", json!({}), "name"),
            ("create_chat", json!({"name": "群"}), "owner_user_id"),
            ("user_display_name", json!({}), "user_id"),
            ("typing", json!({}), "message_id"),
            ("del_typing", json!({}), "message_id"),
            ("done", json!({}), "message_id"),
            ("chat_brief", json!({}), "chat_id"),
            ("get_chat_info", json!({}), "chat_id"),
        ] {
            let e = call_tool(&ch, &json!({"name": tool, "arguments": args}))
                .expect_err("缺参数必须报错，不能静默成功");
            assert!(e.contains(want), "{tool} 的报因要点名缺 `{want}`：{e}");
        }
    }

    /// `del_typing` 的 `reaction_id` 为 null / 缺失时要**当成无事可做而成功**：
    /// 没加成功过就没有东西可撤（`del_reaction` 遇到空 id 直接返回，不触网）。
    #[test]
    fn del_typing_without_reaction_id_is_a_noop_success() {
        let ch = fake_channel();
        for args in [
            json!({"message_id": "om_1"}),
            json!({"message_id": "om_1", "reaction_id": Value::Null}),
        ] {
            let r = call_tool(&ch, &json!({"name": "del_typing", "arguments": args}))
                .expect("无 reaction_id 不该报错");
            let text = r["content"][0]["text"].as_str().unwrap();
            assert_eq!(serde_json::from_str::<Value>(text).unwrap(), json!({}));
        }
    }

    /// 发送方能控制 `file_name` ⇒ 必须抽掉目录分隔符与前导点，否则能写到目录外。
    #[test]
    fn download_name_cannot_escape_the_directory() {
        assert_eq!(sanitize_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name("..\\..\\win.ini"), "win.ini");
        assert_eq!(sanitize_name(".."), "");
        assert_eq!(sanitize_name(".hidden"), "hidden");
        assert_eq!(
            sanitize_name("报表 2026.xlsx"),
            "报表 2026.xlsx",
            "中文名是合法的"
        );
        assert_eq!(sanitize_name("a\u{7}b"), "ab", "控制字符要去掉");
        // 消息 id 同样只用于拼名字，也消毒。
        assert_eq!(sanitize_mid("om_1/../x"), "om_1_.._x");
        assert_eq!(sanitize_mid(""), "unknown");
    }

    /// 没给文件名时按 mime 补一个（飞书图片消息常带空 file_name）：
    /// ABB 拿 `file_name` 判图片/文件并抽扩展名，空名字会让它整份失真。
    #[test]
    fn download_file_name_falls_back_to_mime() {
        let img = feishu::FeishuResource {
            kind: "image".into(),
            file_key: "img_1".into(),
            file_name: String::new(),
        };
        assert_eq!(download_file_name(&img, 0, "image/png"), "image_0.png");
        assert_eq!(
            download_file_name(&img, 2, "application/octet-stream; charset=x"),
            "image_2.bin"
        );
        let doc = feishu::FeishuResource {
            kind: "file".into(),
            file_key: "file_1".into(),
            file_name: "报表.xlsx".into(),
        };
        assert_eq!(download_file_name(&doc, 1, "text/plain"), "报表.xlsx");
        assert_eq!(ext_from_mime("application/pdf"), "pdf");
        assert_eq!(ext_from_mime("image/jpeg"), "jpg");
        assert_eq!(ext_from_mime(""), "bin");
    }

    /// 下载目录：env 覆盖优先，否则临时目录下的固定子目录；落盘后读得回来，
    /// 且文件名带 mid/seq 前缀（同一消息的多个附件不会互盖）。
    ///
    /// **两件事写在一个用例里是故意的**：中间要改 `ABB_FEISHU_DOWNLOAD_DIR`，
    /// 而 env 是进程级共享状态，拆成两个用例会在并行执行时互相干扰。
    #[test]
    fn download_dir_env_override_and_save_download() {
        // 默认值形状：绝对路径 + 带渠道名（同一台机器上能认出是谁写的）。
        // 前提是本文件里只有这个用例碰该 env（见上）。
        std::env::remove_var(DOWNLOAD_DIR_ENV);
        let d = download_dir();
        assert!(d.is_absolute(), "下载目录必须是绝对路径：{d:?}");
        assert!(
            d.ends_with("abb-channel-feishu"),
            "默认目录应带渠道名：{d:?}"
        );

        let dir = std::env::temp_dir().join(format!("abb-cfs-test-{}", std::process::id()));
        std::env::set_var(DOWNLOAD_DIR_ENV, &dir);
        assert_eq!(download_dir(), dir, "env 覆盖必须生效");
        let written = (|| {
            let p1 = save_download("om_1", 0, "a.txt", b"hello")?;
            let p2 = save_download("om_1", 1, "a.txt", b"world")?;
            Ok::<_, String>((p1, p2))
        })();
        std::env::remove_var(DOWNLOAD_DIR_ENV);
        let (p1, p2) = written.expect("写附件");
        assert_ne!(p1, p2, "同一消息的不同 seq 不能互相覆盖");
        assert_eq!(std::fs::read(&p1).unwrap(), b"hello");
        assert!(p1.ends_with("om_1_0_a.txt"), "实际：{p1}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 入站资源必须列出来（不是空数组——那会让 ABB 判「无入站」）。
    #[test]
    fn resources_list_declares_the_inbox() {
        let r = call(
            &fake_channel(),
            json!({"jsonrpc":"2.0","id":1,"method":"resources/list"}),
        );
        assert_eq!(r["result"]["resources"][0]["uri"], INBOX_URI);
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

    /// **缓冲非空时订阅要置「待补通知」标记**，否则那条消息要等下一轮才被取走。
    ///
    /// 为什么单独立一条：`notify_pending` 的清零发生在**主循环**里
    /// （`notify_pending.swap(false, …)` 之后补通知），而单测直接调 `call()`
    /// 不经过主循环 ⇒ 在同一个用例里既验"置上"又验"清掉"必然自相矛盾。
    /// （第一版就是这么写的，把实现正确的代码判成了失败。）
    #[test]
    fn subscribing_with_buffered_messages_marks_pending_notify() {
        let ch = fake_channel();
        ch.inbox.lock().unwrap().push(sample_message());
        let _ = call(
            &ch,
            json!({"jsonrpc":"2.0","id":1,"method":"resources/subscribe",
                   "params":{"uri": INBOX_URI}}),
        );
        assert!(
            ch.notify_pending.load(Ordering::SeqCst),
            "缓冲有消息时订阅必须置「待补通知」标记"
        );
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

    /// 未订阅时**不推**通知（MCP 规矩）；订阅后才推。
    #[test]
    fn notification_only_after_subscribe() {
        let ch = fake_channel();
        // 未订阅：push 只入缓冲，不设任何待推标记（写 stdout 由 write_line 直接做，
        // 这里的可观测判据是「没有订阅」）。
        ch.push_inbound(json!({"chat_id":"c","message_id":"m"}));
        assert_eq!(ch.take_inbox().len(), 1, "未订阅也要入缓冲，不能丢消息");
        ch.subscribe(INBOX_URI);
        assert!(ch.is_subscribed(INBOX_URI));
    }

    /// **先收到消息、后订阅**时不能丢：这条路上唯一的补救就是「订阅成功后补一条
    /// 通知」，靠的就是 `notify_pending` 标记。把它写坏（永远 false）的症状是
    /// 「订阅前到达的消息就死在缓冲里」——最难查的那类丢失。
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

    /// 话题回复缺 message_id ⇒ 明确报错（不是发到主聊天去）。
    #[test]
    fn thread_reply_requires_message_id() {
        let e = call_tool(
            &fake_channel(),
            &json!({"name":"send_thread_reply","arguments":{"chat_id":"c","text":"hi"}}),
        )
        .unwrap_err();
        assert!(e.contains("message_id"), "报因要点名缺哪个字段：{e}");
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

    /// 空 filenames 按 mime 补后缀：上传按后缀校验格式，没名字就必然被拒。
    #[test]
    fn upload_name_falls_back_to_mime() {
        assert_eq!(upload_name("pic.png", "", "image"), "pic.png");
        assert_eq!(upload_name("", "application/pdf", "file"), "attachment.pdf");
        assert_eq!(upload_name("", "", "image"), "attachment.png");
        assert_eq!(upload_name("", "", "file"), "attachment.bin");
    }

    /// 图片判定看扩展名（`kind=image` 但后缀是 heic 要走文件卡片）。
    #[test]
    fn image_upload_decides_on_extension() {
        assert!(feishu::feishu_image_uploadable("a.PNG"), "大小写不敏感");
        assert!(!feishu::feishu_image_uploadable("a.heic"));
        assert!(!feishu::feishu_image_uploadable("a"));
        assert_eq!(feishu::feishu_file_type("报表.XLSX"), "xls");
        assert_eq!(feishu::feishu_file_type("a.mov"), "stream");
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
