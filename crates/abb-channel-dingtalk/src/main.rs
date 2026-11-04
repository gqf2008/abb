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
//! | 出站 | 工具 `create_chat`（可选） | ✅ `topapi/im/chat/create`（`owner_user_id` 同时作群主与唯一成员） |
//! | 入站 | `channel://` 资源订阅 | ✅ **资源订阅**（`channel://inbox`），见下 |
//! | 入站 | 工具 `download_attachment`（可选） | ✅ `downloadCode` 台账 + `messageFiles/download` → 落盘给路径 |
//! | 查询 | 工具 `get_chat_info`（可选） | ✅ `topapi/im/chat/get`（单聊走 `contact/users/:id`） |
//! | 查询 | 工具 `chat_brief`（可选） | ✅ 入站 `conversationTitle` 台账 + 上面的反查兜底 |
//! | 查询 | 工具 `user_display_name`（可选） | ✅ `contact/users/:id` |
//! | 状态 | 工具 `note_sender`（可选） | ✅ 写入进程内「最近提问者」表（与入站路径同一张） |
//!
//! ## 明确**不声明**的可选能力（宁可缺，不可假）
//!
//! | 工具 | 为什么不声明 |
//! |---|---|
//! | `send_thread_reply` | 钉钉没有话题维度（见下节） |
//! | `typing` / `del_typing` / `done` | 钉钉机器人**没有表情/reaction API**——没有能落地的平台动作 |
//! | `note_context` | 它是微信 `context_token` 的专用形状；钉钉的出站不需要任何会话令牌 |
//! | `get_quoted_message` | 钉钉的引用内容**随入站帧到达**（`text.repliedMsg`），本进程在入站时已按
//!   ABB 原 prompt 形状并进 `text`（`[引用消息]…`）；钉钉也没有「按 msgId 取任意历史消息」
//!   的接口，声明它只会让 ABB 多跑一次注定落空的调用 |
//! | `poll` | 入站是服务端推的长连接（见下节），没有「按游标取一批」的原语 |
//!
//! ## `download_attachment` 为什么要一张「最近入站消息」台账
//!
//! 契约把参数定成 `{message_id, seq}`（§3.4：**怎么下载是渠道的事**），而钉钉的
//! `downloadCode` 只在入站 CALLBACK 帧里出现**一次**——不记下来就永远拿不到。
//! 所以入站时把「message_id → 附件引用（kind / downloadCode / 文件名）+ robotCode」
//! 记进容量 [`RECENT_CAP`] 的台账，工具调用时反查。台账只留最近若干条：这是**能力**
//! 而不是**保证**——查不到就明确报错，让 ABB 如实记成「附件没下来」，绝不编造路径。
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

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use dingtalk::{DingTalkClient, DingtalkAttachment, DingtalkMessage};
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

/// 「最近入站消息」台账容量（条）。
///
/// 只服务两个可选能力：`download_attachment`（按 `message_id` 反查 `downloadCode`）
/// 与 `chat_brief`（按 `chat_id` 反查群名）。正常调用是「收到消息后立刻处理」，
/// 几百条足够覆盖重试/并发；上限只为**不吃内存**——满了丢最旧的，且丢掉的
/// 消息会得到一条说得清原因的报错（见 `download_attachment` 臂）。
const RECENT_CAP: usize = 500;

/// ABB 对**引用消息**的附件用的 `seq` 约定是 `100 + i`（`bridge` 的引用路径），
/// 主消息自己的附件则是 `i`。台账按同一套编号分派，免得两个方向错位。
const QUOTED_SEQ_BASE: usize = 100;

/// 一条待下载附件的台账项（`downloadCode` 只在入站帧里出现一次，必须记下来）。
#[derive(Debug, Clone)]
pub(crate) struct CachedAttachment {
    kind: String,
    download_code: String,
    file_name: String,
    /// 语音消息的转写文本（钉钉 `recognition`）：落盘时原样回报给 ABB 当备注。
    voice_text: String,
}

impl CachedAttachment {
    fn of(a: &DingtalkAttachment) -> CachedAttachment {
        CachedAttachment {
            kind: a.kind.clone(),
            download_code: a.download_code.clone(),
            file_name: a.file_name.clone(),
            voice_text: a.voice_text.clone(),
        }
    }
}

/// 一条入站消息的台账项。
#[derive(Debug, Clone)]
pub(crate) struct CachedMessage {
    /// 回调里的 `robotCode`（下载附件必需）。空 ⇒ 回落渠道级 `DT_ROBOT_CODE`。
    robot_code: String,
    /// 本条消息自己的附件（`seq` 就是下标）。
    attachments: Vec<CachedAttachment>,
    /// 被引用消息的附件（`seq` = `QUOTED_SEQ_BASE + i`）。
    quoted_attachments: Vec<CachedAttachment>,
}

/// 定长「最近入站消息」台账（FIFO 淘汰）。
#[derive(Default)]
pub(crate) struct Recent {
    order: VecDeque<String>,
    items: HashMap<String, CachedMessage>,
}

impl Recent {
    fn put(&mut self, mid: &str, m: CachedMessage) {
        if self.items.insert(mid.to_string(), m).is_none() {
            self.order.push_back(mid.to_string());
        }
        while self.order.len() > RECENT_CAP {
            if let Some(old) = self.order.pop_front() {
                self.items.remove(&old);
            }
        }
    }

    fn get(&self, mid: &str) -> Option<&CachedMessage> {
        self.items.get(mid)
    }
}

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
    /// 「最近入站消息」台账（`download_attachment` 按 `message_id` 反查下载引用用）。
    recent: Mutex<Recent>,
    /// 会话名台账（钉钉只在**入站帧**里给 `conversationTitle`，出站/查询接口的群资料
    /// 需要额外权限；先记下来，`chat_brief`/`get_chat_info` 才能不联网就答）。
    chat_names: Mutex<HashMap<String, String>>,
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

    /// 记下一条入站消息的附件引用与 robotCode（`download_attachment` 反查用）。
    ///
    /// 顺手记下会话名：钉钉只在入站帧里给 `conversationTitle`，出站/查询接口的群资料
    /// 需要额外权限——不在这里记，`chat_brief` 就得多跑一次注定失败的 API。
    pub(crate) fn remember(&self, msg: &DingtalkMessage) {
        let convert = |atts: &[DingtalkAttachment]| -> Vec<CachedAttachment> {
            atts.iter().map(CachedAttachment::of).collect()
        };
        let cached = CachedMessage {
            robot_code: msg.robot_code.clone(),
            attachments: convert(&msg.attachments),
            quoted_attachments: convert(&msg.quoted_attachments),
        };
        self.recent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .put(&msg.mid, cached);
        let chat_id = msg.chat_id();
        if !chat_id.is_empty() && !msg.conversation_title.is_empty() {
            self.note_chat_name(&chat_id, &msg.conversation_title);
        }
    }

    /// 取某条入站消息的台账项（没有 = 没收到过 / 已被 FIFO 腾出）。
    fn cached_message(&self, mid: &str) -> Option<CachedMessage> {
        self.recent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(mid)
            .cloned()
    }

    fn note_chat_name(&self, chat_id: &str, name: &str) {
        self.chat_names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(chat_id.to_string(), name.to_string());
    }

    fn chat_name_of(&self, chat_id: &str) -> Option<String> {
        self.chat_names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(chat_id)
            .cloned()
            .filter(|n| !n.is_empty())
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
        recent: Mutex::new(Recent::default()),
        chat_names: Mutex::new(HashMap::new()),
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
        },
        {
            "name": "create_chat",
            "description": "创建钉钉群会话（topapi/im/chat/create）。注意：钉钉建群必须给群主与成员，所以 owner_user_id 同时当群主和唯一成员；钉钉群无「群介绍」字段，description 被忽略",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "群名"},
                    "description": {"type": "string", "description": "群介绍（钉钉无此字段，传了也忽略）"},
                    "owner_user_id": {"type": "string", "description": "群主 staffId（钉钉建群必需；空则明确报错）"}
                },
                "required": ["name", "owner_user_id"]
            }
        },
        {
            "name": "update_chat",
            "description": "改群名（topapi/im/chat/update）。钉钉群没有群介绍字段，description 被忽略",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat_id": {"type": "string", "description": "群 cid…"},
                    "name": {"type": "string", "description": "新群名"},
                    "description": {"type": "string", "description": "群介绍（钉钉无此字段，传了也忽略）"}
                },
                "required": ["chat_id", "name"]
            }
        },
        {
            "name": "download_attachment",
            "description": "把某条入站消息的第 seq 个附件下载到本地，返回落盘路径（钉钉的 downloadCode 只在入站帧里出现一次，本进程按 message_id 反查）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message_id": {"type": "string", "description": "入站消息的 message_id"},
                    "seq": {"type": "integer", "description": "附件序号（主消息附件从 0 起；引用消息的附件按 ABB 约定从 100 起）"}
                },
                "required": ["message_id", "seq"]
            }
        },
        {
            "name": "get_chat_info",
            "description": "查群资料（topapi/im/chat/get，返回群名；钉钉群没有群介绍字段，description 恒空）。单聊会话走通讯录反查对方姓名",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat_id": {"type": "string", "description": "会话键：群聊 = cid…，单聊 = staffId"}
                },
                "required": ["chat_id"]
            }
        },
        {
            "name": "chat_brief",
            "description": "会话类型 + 会话名（历史回填用）：群聊回 group、单聊回 p2p；名字优先用入站帧里的 conversationTitle",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat_id": {"type": "string", "description": "会话键：群聊 = cid…，单聊 = staffId"}
                },
                "required": ["chat_id"]
            }
        },
        {
            "name": "user_display_name",
            "description": "按 staffId 反查姓名（contact/users/:id；查不到就报错，不返回空串）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "user_id": {"type": "string", "description": "对方 staffId"}
                },
                "required": ["user_id"]
            }
        },
        {
            "name": "note_sender",
            "description": "记下会话最近一条入站消息的发送者（send_text 在群聊里 @ 他）。与入站时本进程自己刷新的那张表是同一张",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat_id": {"type": "string", "description": "会话键（群聊 = cid…）"},
                    "sender_id": {"type": "string", "description": "发送者 staffId"}
                },
                "required": ["chat_id", "sender_id"]
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
            Ok(tool_text("ok"))
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
            Ok(tool_text("ok"))
        }
        // ── 可选能力（契约 §3.4）：钉钉能做的，声明了就必须真做 ──
        "create_chat" => create_chat(ch, &a),
        "download_attachment" => download_attachment(ch, &a),
        "update_chat" => {
            let cid = a.get("chat_id").and_then(|v| v.as_str()).unwrap_or("");
            let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let desc = a.get("description").and_then(|v| v.as_str()).unwrap_or("");
            if cid.is_empty() {
                return Err("update_chat 缺 chat_id".into());
            }
            if name.trim().is_empty() {
                return Err("update_chat 缺 name（群名）".into());
            }
            rt().block_on(ch.client.update_chat(cid, name, desc))
                .map_err(|e| format!("改群名失败：{e:#}"))?;
            // 本进程的群名缓存同步更新：`chat_brief`/`get_chat_info` 立即答新名，
            // 不必等下一帧入站消息把新标题带回来。
            ch.note_chat_name(cid, name);
            Ok(json!({"content": [{"type": "text", "text": "{}"}]}))
        }
        "get_chat_info" => get_chat_info(ch, &a),
        "chat_brief" => chat_brief(ch, &a),
        "user_display_name" => user_display_name(ch, &a),
        "note_sender" => {
            let chat_id = a.get("chat_id").and_then(|v| v.as_str()).unwrap_or("");
            let sender_id = a.get("sender_id").and_then(|v| v.as_str()).unwrap_or("");
            if chat_id.is_empty() {
                return Err("note_sender 缺 chat_id".into());
            }
            if sender_id.is_empty() {
                return Err("note_sender 缺 sender_id".into());
            }
            // 与入站路径同一张表：`send_text` 在群聊里查它做 @（见 `at_user_for`）。
            ch.note_sender(chat_id, sender_id);
            Ok(tool_text("ok"))
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

/// 工具成功的统一回包形状（MCP `tools/call` 的 `content` 数组）。
fn tool_text(s: &str) -> Value {
    json!({"content": [{"type": "text", "text": s}]})
}

/// `create_chat`：建群。
///
/// 契约把 `owner_user_id` 一起给了过来（内置实现当时没有这个参数、只能传空 `owner`）。
/// 钉钉的 `topapi/im/chat/create` **要求** `owner` 与 `useridlist` 都非空，传空串的
/// 请求必然被平台拒——所以这里把 `owner_user_id` 同时当群主与唯一成员。
/// 拿不到 `owner_user_id` 就**当场报错并指路登记制**，而不是发一条注定被拒的请求。
fn create_chat(ch: &Arc<Channel>, a: &Value) -> Result<Value, String> {
    let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let description = a.get("description").and_then(|v| v.as_str()).unwrap_or("");
    let owner = a
        .get("owner_user_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if name.trim().is_empty() {
        return Err("create_chat 缺 name（群名）".into());
    }
    if owner.trim().is_empty() {
        return Err(
            "create_chat 缺 owner_user_id（群主 staffId）：钉钉建群必须指定群主与成员，\
             没有群主只能手动建群后在虚拟 Bot 面板里登记 chat_id"
                .into(),
        );
    }
    let chat_id = rt()
        .block_on(ch.client.create_chat(name, description, owner))
        .map_err(|e| format!("create_chat 失败：{e:#}"))?;
    // 新群记下群名：`chat_brief`/`get_chat_info` 能立即答（此时还没有任何入站消息）。
    ch.note_chat_name(&chat_id, name);
    Ok(json!({"chat_id": chat_id}))
}

/// `download_attachment`：按 `{message_id, seq}` 反查附件引用并下载到本地。
///
/// 参数形状是契约定的（§3.4「怎么下载是渠道的事」）：ABB 只给消息 id 与序号，
/// `downloadCode` 由本进程的入站台账提供（它只在 CALLBACK 帧里出现一次）。
fn download_attachment(ch: &Arc<Channel>, a: &Value) -> Result<Value, String> {
    let mid = a.get("message_id").and_then(|v| v.as_str()).unwrap_or("");
    if mid.is_empty() {
        return Err("download_attachment 缺 message_id".into());
    }
    // seq 缺失当 0（单附件消息的常见调用），但**非法数字**要报错，不猜。
    let seq = match a.get("seq") {
        None | Some(Value::Null) => 0usize,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| format!("download_attachment 的 seq 不是非负整数：{v}"))?
            as usize,
    };
    let cached = ch.cached_message(mid).ok_or_else(|| {
        format!(
            "没有 message_id={mid} 的入站记录：钉钉的 downloadCode 只在入站帧里出现一次，\
             本进程只存最近 {RECENT_CAP} 条消息的附件引用；查不到就不能编一个文件出来"
        )
    })?;
    // 主消息附件直接用 seq；引用消息的附件按 ABB 的 `QUOTED_SEQ_BASE + i` 约定。
    let (att, hint) = if seq >= QUOTED_SEQ_BASE {
        (
            cached.quoted_attachments.get(seq - QUOTED_SEQ_BASE),
            format!(
                "该消息的引用消息只有 {} 个附件",
                cached.quoted_attachments.len()
            ),
        )
    } else {
        (
            cached.attachments.get(seq),
            format!("该消息只有 {} 个附件", cached.attachments.len()),
        )
    };
    let att = att.ok_or_else(|| format!("download_attachment 的 seq={seq} 越界：{hint}"))?;
    if att.download_code.is_empty() {
        return Err(format!(
            "download_attachment：seq={seq} 的附件没有 downloadCode（入站帧未提供），无法下载"
        ));
    }
    // robotCode 以回调里的为准（回调值仅当非空才用），否则回落渠道级配置。
    let rc = if cached.robot_code.is_empty() {
        ch.robot_code.as_str()
    } else {
        cached.robot_code.as_str()
    };
    if rc.is_empty() {
        return Err(
            "download_attachment 失败：未配置机器人码（DT_ROBOT_CODE / DT_APP_ID 都为空）".into(),
        );
    }
    let bytes = rt()
        .block_on(ch.client.download_msg_file(&att.download_code, rc))
        .map_err(|e| format!("download_attachment 失败（下载下载码对应的文件）：{e:#}"))?;
    let size = bytes.len();
    let path = save_download(mid, seq, att, bytes)?;
    Ok(json!({
        "path": path.to_string_lossy(),
        "kind": att.kind,
        "file_name": att.file_name,
        "mime": mime_of(&att.file_name),
        "size": size,
        // 语音消息的转写文本（其它类型为空）：ABB 会把它当附件备注进 prompt。
        "note": att.voice_text,
    }))
}

/// 落盘。目录：`DT_DOWNLOAD_DIR` 优先，否则 `<系统临时目录>/abb-channel-dingtalk`。
///
/// 文件名带消息 id 与序号（`<mid>-<seq>-<原名>`）避免同名覆盖，两段都做过清洗
/// （`mid` 里常见 `=`、`/` 等字符，直接当文件名会跳出目录）。
fn save_download(
    mid: &str,
    seq: usize,
    att: &CachedAttachment,
    bytes: Vec<u8>,
) -> Result<PathBuf, String> {
    let dir = download_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("建下载目录 {} 失败：{e}", dir.display()))?;
    let name = download_file_name(att, seq);
    let path = dir.join(format!("{}-{seq}-{name}", safe_component(mid)));
    std::fs::write(&path, &bytes).map_err(|e| format!("写附件 {} 失败：{e}", path.display()))?;
    Ok(path)
}

/// 下载目录（`DT_DOWNLOAD_DIR` 显式覆盖；默认放系统临时目录下的专属子目录）。
fn download_dir() -> PathBuf {
    match std::env::var("DT_DOWNLOAD_DIR") {
        Ok(d) if !d.trim().is_empty() => PathBuf::from(d),
        _ => std::env::temp_dir().join("abb-channel-dingtalk"),
    }
}

/// 落盘文件名：原名可用就用（已清洗路径分隔符），否则按 kind + seq 补一个。
fn download_file_name(att: &CachedAttachment, seq: usize) -> String {
    let cleaned = safe_component(att.file_name.trim());
    if !cleaned.is_empty() && cleaned != "_" {
        return cleaned;
    }
    let ext = match att.kind.as_str() {
        "image" => "png",
        "audio" => "amr",
        "video" => "mp4",
        _ => "bin",
    };
    format!("attachment-{seq}.{ext}")
}

/// 把任意字符串清洗成安全的文件名片段：只留字母/数字/`.`/`-`/`_`，其余换 `_`；
/// 连 `..` 也压成单个 `.`——落盘文件名始终带 `<mid>-<seq>-` 前缀，理论上跳不出去，
/// 但没理由留一个「跳目录」的形状给下一个人误解。
fn safe_component(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    while out.contains("..") {
        out = out.replace("..", ".");
    }
    out
}

/// 按扩展名猜 mime（ABB 只拿它做提示；不给就空，**不猜成 `application/octet-stream`**
/// 之外的东西）。
fn mime_of(file_name: &str) -> &'static str {
    let ext = file_name
        .rsplit_once('.')
        .map(|(_, e)| e.trim().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        "amr" => "audio/amr",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "rar" => "application/vnd.rar",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}

/// `get_chat_info`：群聊走 `topapi/im/chat/get`；单聊没有群可查，改走通讯录拿对方姓名。
///
/// 钉钉的群资料**没有群介绍字段** ⇒ `description` 恒空（平台限制，与内置实现同口径）。
fn get_chat_info(ch: &Arc<Channel>, a: &Value) -> Result<Value, String> {
    let chat_id = a.get("chat_id").and_then(|v| v.as_str()).unwrap_or("");
    if chat_id.is_empty() {
        return Err("get_chat_info 缺 chat_id".into());
    }
    if dingtalk::is_group_chat(chat_id) {
        let (name, _) = rt()
            .block_on(ch.client.get_chat_info(chat_id))
            .map_err(|e| format!("get_chat_info 失败：{e:#}"))?;
        if !name.is_empty() {
            ch.note_chat_name(chat_id, &name);
        }
        Ok(json!({"name": name, "description": ""}))
    } else {
        let name = rt().block_on(ch.client.user_name(chat_id)).ok_or_else(|| {
            format!(
                "get_chat_info：单聊会话 {chat_id} 查不到对方姓名\
                     （通讯录权限不足/已离职/网络失败）"
            )
        })?;
        ch.note_chat_name(chat_id, &name);
        Ok(json!({"name": name, "description": ""}))
    }
}

/// `chat_brief`：会话类型 + 会话名（历史回填用）。
///
/// 类型按会话键前缀定（`cid…` = 群），不需要联网；名字先用入站帧里的
/// `conversationTitle`，没有才去反查（钉钉没有现成的「按 chat_id 取会话」接口）。
fn chat_brief(ch: &Arc<Channel>, a: &Value) -> Result<Value, String> {
    let chat_id = a.get("chat_id").and_then(|v| v.as_str()).unwrap_or("");
    if chat_id.is_empty() {
        return Err("chat_brief 缺 chat_id".into());
    }
    let chat_type = if dingtalk::is_group_chat(chat_id) {
        "group"
    } else {
        "p2p"
    };
    let name = match ch.chat_name_of(chat_id) {
        Some(n) => n,
        // 名字查不到不算错：契约允许 `chat_name` 为空（回填不上名字比整条失败可接受）。
        None => get_chat_info(ch, &json!({"chat_id": chat_id}))
            .ok()
            .and_then(|v| v.get("name").and_then(|x| x.as_str()).map(str::to_string))
            .unwrap_or_default(),
    };
    Ok(json!({"chat_type": chat_type, "chat_name": name}))
}

/// `user_display_name`：按 staffId 反查姓名。查不到就**报错**（不回空串——空名字
/// 在授权记录里跟「没查」长得一样）。
fn user_display_name(ch: &Arc<Channel>, a: &Value) -> Result<Value, String> {
    let user_id = a.get("user_id").and_then(|v| v.as_str()).unwrap_or("");
    if user_id.is_empty() {
        return Err("user_display_name 缺 user_id".into());
    }
    let name = rt().block_on(ch.client.user_name(user_id)).ok_or_else(|| {
        format!("user_display_name：查不到 {user_id} 的姓名（通讯录权限不足/已离职/网络失败）")
    })?;
    Ok(json!({"name": name}))
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
            recent: Mutex::new(Recent::default()),
            chat_names: Mutex::new(HashMap::new()),
            // 单测的 stdout 会被 `cargo test` 捕获，写进去不会污染真实协议通道。
            out: Mutex::new(std::io::stdout()),
        })
    }

    fn call(ch: &Arc<Channel>, req: Value) -> Value {
        handle(ch, &req).expect("要回包")
    }

    /// **工具集必须精确等于「真的实现了的那些」**：多一条就是虚报能力，少一条就是
    /// 静默能力退化。做不到的可选能力一条都不许声明（下一条测试钉住反面）。
    #[test]
    fn tools_list_declares_exactly_the_implemented_tools() {
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
                "send_attachment",
                "create_chat",
                "update_chat",
                "download_attachment",
                "get_chat_info",
                "chat_brief",
                "user_display_name",
                "note_sender",
            ]
        );
        // 每个声明了的工具都要能在 `call_tool` 里落到真分支：空参数时必须报
        // 「缺 X」这类业务报错，而不是「未知工具」。
        let ch = fake_channel();
        for name in &names {
            let e = call_tool(&ch, &json!({"name": name, "arguments": {}}))
                .expect_err("空参数必须报错");
            assert!(!e.contains("未知工具"), "`{name}` 声明了却没实现：{e}");
        }
    }

    /// 做不到的可选能力**一条都不能声明**：钉钉无话题、无表情/reaction API；
    /// `note_context` 是微信专用；`get_quoted_message` 的引用内容已随入站帧并进 `text`，
    /// 钉钉也没有「按 msgId 取历史消息」的接口；入站走资源订阅，所以不声明 `poll`。
    /// 声明它们只会让 ABB 走到必然失败（或白跑一趟）的路径。
    #[test]
    fn unachievable_optional_tools_are_not_declared() {
        let names: Vec<String> = tools()
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        for absent in [
            "send_thread_reply",
            "typing",
            "del_typing",
            "done",
            "note_context",
            "get_quoted_message",
            "poll",
        ] {
            assert!(
                !names.iter().any(|n| n == absent),
                "`{absent}` 钉钉做不到（或无此概念），不许声明：{names:?}"
            );
        }
    }

    /// 每个工具的必需参数必须写进 schema（只写名字不写 schema 等于让调用方猜）。
    #[test]
    fn every_tool_declares_its_required_arguments() {
        let list = tools();
        let required = |name: &str| -> Value {
            let t = list
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == name)
                .unwrap_or_else(|| panic!("没有工具 {name}"));
            t["inputSchema"]["required"].clone()
        };
        assert_eq!(required("send_text"), json!(["chat_id", "text"]));
        assert_eq!(
            required("send_attachment"),
            json!(["chat_id", "name", "data_b64"])
        );
        assert_eq!(required("create_chat"), json!(["name", "owner_user_id"]));
        assert_eq!(
            required("download_attachment"),
            json!(["message_id", "seq"])
        );
        assert_eq!(required("get_chat_info"), json!(["chat_id"]));
        assert_eq!(required("chat_brief"), json!(["chat_id"]));
        assert_eq!(required("user_display_name"), json!(["user_id"]));
        assert_eq!(required("note_sender"), json!(["chat_id", "sender_id"]));
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
            recent: Mutex::new(Recent::default()),
            chat_names: Mutex::new(HashMap::new()),
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

    // ── 可选能力（契约 §3.4）─────────────────────────────────────

    fn dt_attachment(kind: &str, code: &str, name: &str) -> DingtalkAttachment {
        DingtalkAttachment {
            kind: kind.to_string(),
            download_code: code.to_string(),
            file_name: name.to_string(),
            voice_text: String::new(),
        }
    }

    fn dt_message(mid: &str, atts: Vec<DingtalkAttachment>) -> DingtalkMessage {
        DingtalkMessage {
            mid: mid.to_string(),
            sender_staff_id: "staff_1".to_string(),
            conversation_id: "cid_room".to_string(),
            conversation_type: "2".to_string(),
            conversation_title: "测试群".to_string(),
            text: String::new(),
            mentioned: true,
            robot_code: "robot_from_callback".to_string(),
            quoted_text: String::new(),
            quoted_attachments: Vec::new(),
            attachments: atts,
        }
    }

    /// 台账的核心用途：**入站时没记，`download_attachment` 就永远拿不到 downloadCode**。
    #[test]
    fn remember_keeps_attachment_codes_and_chat_name() {
        let ch = fake_channel();
        let m = dt_message("mid_1", vec![dt_attachment("image", "dc_1", "a.png")]);
        ch.remember(&m);
        let cached = ch.cached_message("mid_1").expect("要能反查到");
        assert_eq!(cached.attachments.len(), 1);
        assert_eq!(cached.attachments[0].download_code, "dc_1");
        assert_eq!(
            cached.robot_code, "robot_from_callback",
            "回调里的 robotCode 必须优先于渠道级配置"
        );
        assert_eq!(ch.chat_name_of("cid_room").as_deref(), Some("测试群"));
        assert!(
            ch.cached_message("mid_unknown").is_none(),
            "没收到就是没收到，不编造"
        );
    }

    /// 台账有上限：满了丢最旧（否则长跑进程会吃内存）。
    #[test]
    fn recent_cache_is_bounded_and_drops_oldest() {
        let ch = fake_channel();
        for i in 0..(RECENT_CAP + 1) {
            ch.remember(&dt_message(&format!("mid_{i}"), Vec::new()));
        }
        assert!(ch.cached_message("mid_0").is_none(), "最旧的应被腾出");
        assert!(
            ch.cached_message(&format!("mid_{RECENT_CAP}")).is_some(),
            "最新的必须还在"
        );
    }

    /// 查不到 / 越界 / 缺 seq 都要**明确报错**（不能回空路径，那等于谎称下载了）。
    #[test]
    fn download_attachment_argument_and_cache_errors() {
        let ch = fake_channel();
        let e = download_attachment(&ch, &json!({})).unwrap_err();
        assert!(e.contains("message_id"), "要点名缺哪个字段：{e}");

        let e = download_attachment(&ch, &json!({"message_id":"mid_unknown","seq":0})).unwrap_err();
        assert!(e.contains("没有 message_id"), "要说清是没记到：{e}");

        ch.remember(&dt_message(
            "mid_1",
            vec![dt_attachment("image", "dc_1", "a.png")],
        ));
        let e = download_attachment(&ch, &json!({"message_id":"mid_1","seq":1})).unwrap_err();
        assert!(e.contains("越界"), "越界要说清：{e}");

        // seq 不是非负整数：不猜成 0。
        let e = download_attachment(&ch, &json!({"message_id":"mid_1","seq":"zero"})).unwrap_err();
        assert!(e.contains("非负整数"), "报因要说清 seq 形状：{e}");

        // 引用消息的附件走 100+i 通道；没有就报越界（不能悄悄回落到主消息）。
        let e = download_attachment(&ch, &json!({"message_id":"mid_1","seq":100})).unwrap_err();
        assert!(e.contains("引用消息"), "要说清是引用通道：{e}");

        // 有记录但 downloadCode 为空：同样明确报错（不去发一个空下载码）。
        ch.remember(&dt_message(
            "mid_2",
            vec![dt_attachment("file", "", "b.pdf")],
        ));
        let e = download_attachment(&ch, &json!({"message_id":"mid_2","seq":0})).unwrap_err();
        assert!(e.contains("downloadCode"), "要说清缺下载码：{e}");
    }

    /// 落盘文件名要带消息 id + 序号（防同名覆盖），且必须清洗路径分隔符。
    #[test]
    fn download_file_names_are_safe_and_unique() {
        let cached =
            |kind: &str, name: &str| CachedAttachment::of(&dt_attachment(kind, "dc", name));
        assert_eq!(download_file_name(&cached("image", "a.png"), 3), "a.png");
        let evil = cached("file", "../../etc/passwd");
        let name = download_file_name(&evil, 0);
        assert!(!name.contains('/'), "路径分隔符必须被清洗：{name}");
        assert!(!name.contains(".."), "不许留 ..（防跳出目录）：{name}");
        // 没名字时按 kind 补后缀（上传/落盘都要有扩展名）。
        let unnamed = cached("audio", "");
        assert_eq!(download_file_name(&unnamed, 2), "attachment-2.amr");
        assert_eq!(safe_component("a b"), "a_b");
    }

    /// mime 按扩展名给；不认识的落到 octet-stream（不猜具体类型）。
    #[test]
    fn mime_is_derived_from_extension() {
        assert_eq!(mime_of("a.PNG"), "image/png");
        assert_eq!(
            mime_of("报表.xlsx"),
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        );
        assert_eq!(mime_of("voice.amr"), "audio/amr");
        assert_eq!(mime_of("noext"), "application/octet-stream");
    }

    /// `chat_brief`：类型按前缀给、名字用台账（**不联网**就能答的部分要真的不联网）。
    #[test]
    fn chat_brief_answers_from_the_inbound_cache() {
        let ch = fake_channel();
        ch.note_chat_name("cid_room", "测试群");
        let r = chat_brief(&ch, &json!({"chat_id":"cid_room"})).unwrap();
        assert_eq!(r["chat_type"], "group");
        assert_eq!(r["chat_name"], "测试群");
        // 单聊的类型判定不需要任何台账/网络。
        ch.note_chat_name("staff_1", "张三");
        let r = chat_brief(&ch, &json!({"chat_id":"staff_1"})).unwrap();
        assert_eq!(r["chat_type"], "p2p");
        assert_eq!(r["chat_name"], "张三");
        let e = chat_brief(&ch, &json!({})).unwrap_err();
        assert!(e.contains("chat_id"), "要点名缺哪个字段：{e}");
    }

    /// `note_sender` 必须真的影响出站 @（否则就是个假工具）。
    #[test]
    fn note_sender_feeds_the_group_at_path() {
        let ch = fake_channel();
        let r = call_tool(
            &ch,
            &json!({"name":"note_sender","arguments":{"chat_id":"cid_room","sender_id":"staff_9"}}),
        )
        .expect("note_sender 不该失败");
        assert_eq!(r["content"][0]["text"], "ok");
        assert_eq!(at_user_for(&ch, "cid_room").as_deref(), Some("staff_9"));
        let e = call_tool(
            &ch,
            &json!({"name":"note_sender","arguments":{"chat_id":"cid_room"}}),
        )
        .unwrap_err();
        assert!(e.contains("sender_id"), "要点名缺哪个字段：{e}");
    }

    /// `create_chat` 没有群主就当场报错并指路登记制，不发注定被拒的请求。
    #[test]
    fn create_chat_requires_name_and_owner_before_any_network_call() {
        let ch = fake_channel();
        let e = create_chat(&ch, &json!({"owner_user_id":"staff_1"})).unwrap_err();
        assert!(e.contains("name"), "要点名缺群名：{e}");
        let e = create_chat(&ch, &json!({"name":"群"})).unwrap_err();
        assert!(e.contains("owner_user_id"), "要点名缺群主：{e}");
        assert!(e.contains("登记"), "要指路登记制降级：{e}");
    }

    /// 查询类工具的入参校验（网络分支要真凭证，这里只钉「不许静默空串」）。
    #[test]
    fn query_tools_reject_missing_arguments() {
        let ch = fake_channel();
        let e = get_chat_info(&ch, &json!({})).unwrap_err();
        assert!(e.contains("chat_id"), "要点名缺哪个字段：{e}");
        let e = user_display_name(&ch, &json!({})).unwrap_err();
        assert!(e.contains("user_id"), "要点名缺哪个字段：{e}");
    }
}
