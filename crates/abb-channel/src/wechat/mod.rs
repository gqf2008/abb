//! 微信渠道的 MCP server 主程序。
//!
//! ## 与 ABB 的关系
//!
//! ABB 是总线，本进程是它拉起的**渠道服务**之一。契约见
//! `docs/mcp-bus-architecture.md` §3：
//!
//! | 方向 | 契约 | 本进程的实现 |
//! |---|---|---|
//! | 出站 | 工具 `send_text`（必需）/ `send_thread_reply`（可选） | 微信无话题概念 ⇒ 只实现 `send_text` |
//! | 出站附件 | `send_attachment`（可选） | 已实现（`send_media`：图片/文件/视频，base64 内联） |
//! | 入站附件 | `download_attachment`（可选，`{message_id, seq}`） | 已实现（CDN AES 解密后落盘；入站时缓存媒体句柄） |
//! | 回复上下文 | `note_context`（可选） | 已实现（写进 `send_text` 用的同一份 `contexts`） |
//! | 入站 | `poll`（兜底）或 `channel://` 资源订阅 | 本进程用 `poll`：微信 `get_updates` 天然是游标长轮询 |
//! | 登录 | —— | 工具 `login`（二维码写到 `--dir`，路径打 stderr） |
//!
//! ## 可选能力里**明确不声明**的那些（微信做不到，声明了就是骗 ABB）
//!
//! | 工具 | 为什么不声明 |
//! |---|---|
//! | `typing` / `del_typing` / `done` | ilink 协议没有表情/reaction API |
//! | `create_chat` | 没有建群 API（单聊只有会话对象，无群实体） |
//! | `chat_brief` / `get_chat_info` | 微信侧没有群概念，也没有会话资料查询 |
//! | `user_display_name` | 没有联系人/资料 API，入站消息只带 `from_user_id`（没有人名） |
//! | `note_sender` | 单聊里 `sender_id == chat_id`，没有「另一个发送者」可记 |
//! | `get_quoted_message` | ilink 的 `ref_msg` **不带原消息 id**，也没有消息库 API ⇒ 按 id 反查不可能；
//!   被引用内容只随入站那一帧内联到达（见 `weixin::WeixinMessage::quoted_text`） |
//!
//! ## 三个必须做对的地方
//!
//! 1. **stdout 只能出 JSON-RPC**。任何一行杂输出都会让 ABB 侧的解析炸掉，
//!    所以日志一律 `eprintln!`（`weixin.rs` 里的 `aeprintln!` 就是为此）。
//! 2. **`context_token` 自己记**。微信 `send_text` 需要它，而 MCP 契约只有
//!    `{chat_id, text}`。每条入站消息都带 `context_token` ⇒ 入站时记进 map。
//!    这正是原来 ABB 里 `Messenger::note_context` 那个渠道特例的归宿——
//!    **搬出去之后 ABB 就不需要它了**。
//! 3. **游标只在成功后推进**。失败却推进 = 那批消息永久丢失。

mod weixin;

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use weixin::{WechatMedia, WeixinClient, WeixinMessage};

/// 入站媒体缓存：`message_id → 该消息的媒体列表`（**列表下标就是 `download_attachment` 的 `seq`**）。
///
/// 为什么必须在进程内缓存：ilink **没有消息库/历史 API**，一条消息的
/// `encrypt_query_param` + `aes_key` 只随入站那一帧来一次；不在那时记下来，
/// 之后谁也拿不到（契约里 `download_attachment` 只传 `{message_id, seq}`）。
/// 引用附件的 `seq` 段起点（与 ABB 的 `mcp_bus::QUOTED_SEQ_BASE` 同值）。
///
/// 两个 crate 不能互相依赖，所以各存一份；改一处必须同步另一处
/// （契约 §3.4 的约定，`seq >= 100` 即「被引用消息的附件」）。
const QUOTED_SEQ_BASE: usize = 100;

#[derive(Default)]
struct MediaCache {
    /// 旧 → 新。超过 [`MAX_CACHED_MESSAGES`] 从最旧的丢。
    entries: Vec<(String, Vec<WechatMedia>)>,
    /// 引用消息（被回复的那条）的媒体，按 `message_id` 记。
    ///
    /// 为什么单独一张表：一条消息可能**既有自己的图、又引用了带图的消息**。
    /// 塞进同一个 `Vec` 会让 `seq` 语义含糊（第 0 个到底是哪张？），
    /// 而契约已经用「`seq >= 100` = 引用段」把两段分开了。
    quoted: Vec<(String, Vec<WechatMedia>)>,
}

/// 缓存多少条消息：ABB 的入站循环「拉到就处理」，几十条够用；
/// 无上限会让长跑进程随消息量线性吃内存。
const MAX_CACHED_MESSAGES: usize = 64;

impl MediaCache {
    /// 记下一条消息的媒体。同 `message_id` 覆盖（并把位置移到队尾 = 最新）。
    ///
    /// 空列表直接返回：没媒体的消息不该占缓存位（否则几条纯文本就能把带图的那条挤掉）。
    fn put(&mut self, message_id: &str, items: Vec<WechatMedia>) {
        if items.is_empty() {
            return;
        }
        self.entries.retain(|(id, _)| id != message_id);
        self.entries.push((message_id.to_string(), items));
        while self.entries.len() > MAX_CACHED_MESSAGES {
            self.entries.remove(0);
        }
    }

    fn get(&self, message_id: &str) -> Option<&Vec<WechatMedia>> {
        self.entries
            .iter()
            .find(|(id, _)| id == message_id)
            .map(|(_, v)| v)
    }

    /// 记下**被引用消息**的媒体（`seq` 从 [`QUOTED_SEQ_BASE`] 起）。
    fn put_quoted(&mut self, message_id: &str, items: Vec<WechatMedia>) {
        if items.is_empty() {
            return;
        }
        self.quoted.retain(|(id, _)| id != message_id);
        self.quoted.push((message_id.to_string(), items));
        while self.quoted.len() > MAX_CACHED_MESSAGES {
            self.quoted.remove(0);
        }
    }

    fn get_quoted(&self, message_id: &str) -> Option<&Vec<WechatMedia>> {
        self.quoted
            .iter()
            .find(|(id, _)| id == message_id)
            .map(|(_, v)| v)
    }
}

/// 附件落盘目录（相对 `--dir`）。
const ATTACHMENTS_DIR: &str = "attachments";
/// 单个附件落盘上限，与 ABB `attachments::MAX_ATTACHMENT_BYTES` 同口径。
const MAX_ATTACHMENT_BYTES: usize = 200 * 1024 * 1024;

/// 渠道进程的共享状态。
struct Channel {
    client: WeixinClient,
    /// `chat_id → context_token`。微信发消息要它，契约里没有 ⇒ 本进程记。
    contexts: Mutex<HashMap<String, String>>,
    /// 入站媒体句柄（`download_attachment` 的查找表）。
    media: Mutex<MediaCache>,
    /// 长轮询游标（`get_updates_buf`）。**只在成功后推进**。
    cursor: Mutex<String>,
    /// 扫码得到的 `wx_user_id`（= ABB 侧的 owner 标识；`chat_id` 就是它）。
    user_id: Mutex<String>,
    /// 二维码与登录态的输出目录（`--dir`）。
    dir: PathBuf,
    /// 当前待扫的二维码句柄（`login` 拿到的那个）。`login_status` 用它轮询。
    ///
    /// 为什么存在进程内存而不是磁盘：它是**一次登录会话**的临时句柄，
    /// 进程重启后本来就该重新扫码。落盘反而会让「上次那个二维码还能不能扫」
    /// 变成一个需要判断的状态。
    qrcode: Mutex<Option<String>>,
}

pub fn run() {
    let dir = parse_dir();
    eprintln!("[wechat-mcp] 启动 dir={}", dir.display());

    // 凭证从环境变量拿（ABB 的 `mcp_channel.env` 下发）——渠道配置由提供方决定，
    // 本进程不认识 ABB 的 config.json。
    let token = std::env::var("WX_TOKEN").unwrap_or_default();
    let base = std::env::var("WX_BASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| weixin::FIXED_BASE_URL.to_string());
    let cdn = std::env::var("WX_CDN_BASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| weixin::DEFAULT_CDN_BASE_URL.to_string());
    let user_id = std::env::var("WX_USER_ID").unwrap_or_default();

    let client = WeixinClient::new(&base, &token, &cdn);
    let ch = Arc::new(Channel {
        client,
        contexts: Mutex::new(HashMap::new()),
        media: Mutex::new(MediaCache::default()),
        cursor: Mutex::new(String::new()),
        user_id: Mutex::new(user_id),
        dir,
        qrcode: Mutex::new(None),
    });

    if token.is_empty() {
        eprintln!(
            "[wechat-mcp] 未提供 WX_TOKEN —— 先调 login 工具扫码，\
             再把拿到的 token 写进 ABB 的 mcp_channel.env"
        );
    }

    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(line) else {
            eprintln!("[wechat-mcp] 非法 JSON，忽略");
            continue;
        };
        let Some(resp) = handle(&ch, &req) else {
            continue; // 通知不回
        };
        // 一行一条：MCP 是 NDJSON。
        if serde_json::to_writer(&mut out, &resp).is_err() {
            break;
        }
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    }
    eprintln!("[wechat-mcp] stdin 关闭，退出");
}

/// `--dir <path>`：二维码与登录态输出目录。默认当前目录。
fn parse_dir() -> PathBuf {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--dir" {
            if let Some(v) = args.next() {
                return PathBuf::from(v);
            }
        } else if let Some(rest) = a.strip_prefix("--dir=") {
            return PathBuf::from(rest);
        }
    }
    PathBuf::from(".")
}

fn handle(ch: &Arc<Channel>, req: &Value) -> Option<Value> {
    let method = req.get("method")?.as_str()?;
    let rid = req.get("id")?;
    if rid.is_null() {
        return None; // 通知
    }
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}, "resources": {}},
            "serverInfo": {"name": "abb-channel-wechat", "version": env!("CARGO_PKG_VERSION")},
        })),
        // 只暴露 `send_text` + `poll` + `login`：
        // - `send_thread_reply`：微信没有话题概念，不声明（ABB 会自动回落 send_text）
        // - `resources`：本进程入站走 poll，不声明资源（ABB 会据能力选路）
        "tools/list" => Ok(json!({"tools": tools()})),
        "resources/list" => Ok(json!({"resources": []})),
        "tools/call" => call_tool(ch, &params),
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

fn tools() -> Value {
    json!([
        {
            "name": "send_text",
            "description": "发文本给微信会话（内部自动带 context_token）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat_id": {"type": "string", "description": "会话键（微信 = user id）"},
                    "text": {"type": "string"}
                },
                "required": ["chat_id", "text"]
            }
        },
        {
            "name": "poll",
            "description": "取入站消息。首次不传 cursor；之后回传上次的 next_cursor",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "cursor": {"type": "string"},
                    "limit": {"type": "integer"}
                }
            }
        },
        {
            "name": "send_attachment",
            "description": "发媒体给微信会话（图片/文件；内容 base64 内联）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat_id": {"type": "string", "description": "会话键（微信 = user id）"},
                    "name": {"type": "string", "description": "文件名（用于判断类型）"},
                    "mime": {"type": "string"},
                    "kind": {"type": "string", "description": "image | file | video"},
                    "data_b64": {"type": "string", "description": "文件内容的 base64"}
                },
                "required": ["chat_id", "name", "data_b64"]
            }
        },
        {
            "name": "download_attachment",
            "description": "下载入站附件（微信 CDN 加密媒体在这里解密后落盘，返回本地路径）。\
                            message_id 必须是本进程 poll 过的消息；seq 是该消息附件列表的下标",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message_id": {"type": "string", "description": "入站消息 id（poll 结果里的 message_id）"},
                    "seq": {"type": "integer", "description": "第几个附件，从 0 开始"}
                },
                "required": ["message_id", "seq"]
            }
        },
        {
            "name": "note_context",
            "description": "显式记下会话的 context_token（与 poll 自动记的是同一份）。\
                            crate 重启后 ABB 可用它把 token 补回来，不用等对方先说话",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat_id": {"type": "string", "description": "会话键（微信 = user id）"},
                    "context_token": {"type": "string"}
                },
                "required": ["chat_id", "context_token"]
            }
        },
        {
            "name": "login",
            "description": "扫码登录：渲染二维码 PNG 到 --dir，返回图片路径（给人扫）",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "login_status",
            "description": "轮询扫码状态。confirmed 时返回 token/base_url/user_id（写进 ABB 的 mcp_channel.env）",
            "inputSchema": {"type": "object", "properties": {}}
        }
    ])
}

/// 标准 base64 解码（含 padding）。返回 `None` 表示输入非法。
///
/// 手写不引 crate：与 ABB 侧 `mcp_channel::base64_encode` 对称，
/// 且 `weixin.rs` 里本来就有 base64 实现（这里独立写一份是因为那边的函数私有）。
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

/// 登录成功时返回给 ABB 的**结果体**。
///
/// 抽成独立函数只有一个理由：**让单测验真代码路径**。
/// 原来测试里自己写了一遍 `json!`，于是「把 token 塞进结果」的变异抓不到——
/// 那是空断言。
///
/// 契约：**绝不包含 token 本身**。工具结果会被 ABB 落进会话历史/日志，
/// 凭证只该走一次性的 stderr 提示（见调用处的 eprintln）。
fn login_result(login: &weixin::WeixinLogin) -> Value {
    json!({
        "status": "confirmed",
        "base_url": login.base_url,
        "user_id": login.user_id,
        "token_len": login.token.chars().count(),
        "hint": "token 已打印到 stderr（不放进工具结果，避免被日志留存）",
    })
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
            let ctx = ch
                .contexts
                .lock()
                .map_err(|_| "contexts 锁中毒".to_string())?
                .get(chat_id)
                .cloned()
                .unwrap_or_default();
            // 空 context_token 也能发（微信侧会按新会话处理），但要记日志，
            // 因为这意味着「ABB 在对方还没说过话时主动发」——那种消息可能被丢弃。
            if ctx.is_empty() {
                eprintln!("[wechat-mcp] 警告：chat_id={chat_id} 无 context_token，按新会话发送");
            }
            let rt = crate::jsonrpc::rt();
            rt.block_on(ch.client.send_text(chat_id, &ctx, text))
                .map_err(|e| format!("send_text 失败：{e}"))?;
            Ok(json!({"content": [{"type": "text", "text": "ok"}]}))
        }
        "send_attachment" => {
            let chat_id = a.get("chat_id").and_then(|v| v.as_str()).unwrap_or("");
            let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("file");
            let kind_s = a.get("kind").and_then(|v| v.as_str()).unwrap_or("file");
            let b64 = a.get("data_b64").and_then(|v| v.as_str()).unwrap_or("");
            if chat_id.is_empty() {
                return Err("send_attachment 缺 chat_id".into());
            }
            if b64.is_empty() {
                return Err("send_attachment 缺 data_b64".into());
            }
            let data = base64_decode(b64).ok_or_else(|| "data_b64 不是合法 base64".to_string())?;
            let ctx = ch
                .contexts
                .lock()
                .map_err(|_| "contexts 锁中毒".to_string())?
                .get(chat_id)
                .cloned()
                .unwrap_or_default();
            if ctx.is_empty() {
                eprintln!("[wechat-mcp] 警告：chat_id={chat_id} 无 context_token，媒体可能被丢弃");
            }
            // 微信侧的类型判定：按 kind 走。图片走 Image，其余一律按 File
            // （VOICE 类型实测被官方丢弃，所以音频也按 File 发——沿用 ABB 原实现的结论）。
            let kind = match kind_s {
                "image" => weixin::OutboundMediaKind::Image,
                "video" => weixin::OutboundMediaKind::Video,
                _ => weixin::OutboundMediaKind::File,
            };
            let rt = crate::jsonrpc::rt();
            rt.block_on(ch.client.send_media(chat_id, &ctx, kind, name, &data))
                .map_err(|e| format!("send_attachment 失败：{e}"))?;
            Ok(json!({"content": [{"type": "text", "text": "ok"}]}))
        }
        "download_attachment" => {
            let mid = a.get("message_id").and_then(|v| v.as_str()).unwrap_or("");
            if mid.is_empty() {
                return Err("download_attachment 缺 message_id".into());
            }
            // `seq` 不给默认值：默认 0 会让调用方“猜错附件”时静默拿到另一个文件。
            let seq = a
                .get("seq")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| "download_attachment 缺 seq（整数，从 0 开始）".to_string())?
                as usize;
            // `seq >= QUOTED_SEQ_BASE` ⇒ 要的是**被引用消息**的附件（契约 §3.4）。
            let (list, is_quoted, idx) = if seq >= QUOTED_SEQ_BASE {
                let g = ch
                    .media
                    .lock()
                    .map_err(|_| "media 缓存锁中毒".to_string())?;
                (g.get_quoted(mid).cloned(), true, seq - QUOTED_SEQ_BASE)
            } else {
                let g = ch
                    .media
                    .lock()
                    .map_err(|_| "media 缓存锁中毒".to_string())?;
                (g.get(mid).cloned(), false, seq)
            };
            let media = list.ok_or_else(|| {
                format!(
                    "message_id={mid} 不在媒体缓存里：只缓存本进程 poll 见过的最近 \
                         {MAX_CACHED_MESSAGES} 条带附件消息（ilink 没有历史消息 API，\
                         进程重启前的附件已无法下载）"
                )
            })?;
            let item = media.get(idx).cloned().ok_or_else(|| {
                if is_quoted {
                    format!(
                        "seq={seq} 越界：message_id={mid} 的被引用消息只有 {} 个附件",
                        media.len()
                    )
                } else {
                    format!(
                        "seq={seq} 越界：message_id={mid} 只有 {} 个附件",
                        media.len()
                    )
                }
            })?;
            let rt = crate::jsonrpc::rt();
            let got = rt
                .block_on(ch.client.download_media(&item))
                .map_err(|e| format!("download_attachment 失败：{e}"))?;
            let Some((bytes, mime, file_name, note)) = got else {
                return Err(format!(
                    "message_id={mid} seq={seq} 没有可下载地址（无 full_url / encrypt_query_param）"
                ));
            };
            let mut meta =
                save_attachment(&ch.dir, mid, seq, &item.kind, &file_name, &mime, &bytes)?;
            if !note.is_empty() {
                meta["note"] = json!(note);
            }
            Ok(json!({"content": [{"type": "text", "text": meta.to_string()}]}))
        }
        "note_context" => {
            let chat_id = a.get("chat_id").and_then(|v| v.as_str()).unwrap_or("");
            let token = a
                .get("context_token")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // 两个都非空才记：空 token 入表等于把“能发”变成“发不出去”。
            if chat_id.is_empty() {
                return Err("note_context 缺 chat_id".into());
            }
            if token.is_empty() {
                return Err("note_context 缺 context_token".into());
            }
            let chats = {
                let mut m = ch
                    .contexts
                    .lock()
                    .map_err(|_| "contexts 锁中毒".to_string())?;
                m.insert(chat_id.to_string(), token.to_string());
                m.len()
            };
            Ok(json!({"content": [{"type": "text", "text": json!({
                "chat_id": chat_id,
                "stored": true,
                "chats": chats,
            })
            .to_string()}]}))
        }
        "poll" => {
            let limit = a.get("limit").and_then(|v| v.as_u64()).unwrap_or(100) as usize;
            let cursor = a
                .get("cursor")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| ch.cursor.lock().map(|c| c.clone()).unwrap_or_default());
            let rt = crate::jsonrpc::rt();
            let got = rt
                .block_on(ch.client.get_updates(&cursor, 20_000))
                .map_err(|e| format!("poll 失败：{e}"))?;
            let (msgs, next, _) = got;
            let mut out = Vec::new();
            for m in msgs.iter().take(limit) {
                if let Some(cm) = to_channel_message(ch, m) {
                    out.push(cm);
                }
            }
            // **游标只在成功后推进**（这里是成功路径）。
            if let Ok(mut c) = ch.cursor.lock() {
                *c = next.clone();
            }
            Ok(json!({"content": [{"type": "text", "text": json!({
                "messages": out,
                "next_cursor": next,
            }).to_string()}]}))
        }
        "login" => {
            let rt = crate::jsonrpc::rt();
            let (qc, _uuid) = rt
                .block_on(weixin::fetch_qrcode())
                .map_err(|e| format!("取二维码失败：{e}"))?;
            // 记下句柄供 `login_status` 轮询（见 Channel::qrcode 的注释）。
            if let Ok(mut q) = ch.qrcode.lock() {
                *q = Some(qc.clone());
            }
            let p = weixin::save_qrcode_image(&ch.dir, &qc)
                .map_err(|e| format!("写二维码失败：{e}"))?;
            // 路径打到 stderr：人看得到，ABB 不会误读成协议数据。
            eprintln!("[wechat-mcp] 请用微信扫码：{}", p.display());
            Ok(json!({"content": [{"type": "text", "text": json!({
                "qrcode_path": p.display().to_string(),
                "next": "扫码后调 login_status；confirmed 时返回 token/base_url/user_id",
            }).to_string()}]}))
        }
        "login_status" => {
            let qc = ch
                .qrcode
                .lock()
                .map_err(|_| "qrcode 锁中毒".to_string())?
                .clone()
                .ok_or_else(|| "还没调 login 拿二维码".to_string())?;
            let rt = crate::jsonrpc::rt();
            match rt.block_on(weixin::poll_qr_status(&qc)) {
                // wait / scaned：还没确认。返回 pending 而不是错误——
                // 「用户还没扫」是正常状态，不是失败。
                Ok(None) => Ok(json!({"content": [{"type": "text", "text": json!({
                    "status": "pending",
                    "hint": "还没确认（未扫或已扫待确认）",
                }).to_string()}]})),
                Ok(Some(login)) => {
                    // 结果体抽成函数：单测要能验**真代码路径**产出的形状，
                    // 而不是测试里自己再写一遍 json!（那样是空断言，变异抓不到）。
                    let body = login_result(&login);
                    // 登录成功后把凭证打到 stderr：人能看见，ABB 不会误读成协议数据。
                    // **不写进返回值里的 token 字段**是有意的——工具结果可能被日志/历史记录，
                    // 凭证只该走一次性的 stderr 提示。
                    eprintln!(
                        "[wechat-mcp] 登录成功。把下面三行写进 ABB 的 mcp_channel.env：\n\
                         WX_TOKEN={}\nWX_BASE_URL={}\nWX_USER_ID={}",
                        login.token, login.base_url, login.user_id
                    );
                    // 同时更新进程内 client 的 user_id（防自循环要用）。
                    if let Ok(mut u) = ch.user_id.lock() {
                        *u = login.user_id.clone();
                    }
                    Ok(json!({"content": [{"type": "text", "text": body.to_string()}]}))
                }
                Err(e) => Ok(json!({"content": [{"type": "text", "text": json!({
                    "status": "error",
                    "message": e.to_string(),
                }).to_string()}]})),
            }
        }
        other => Err(format!("未知工具 `{other}`")),
    }
}

/// 把下载到的字节落盘，返回给 ABB 的附件元数据。
///
/// 文件名 `<mid>_<seq><ext>`：与 ABB `attachments::save_attachment_in` 同构
/// （mid 安全化、扩展名从原文件名/mime 推）。目录是 `<--dir>/attachments/`。
fn save_attachment(
    dir: &Path,
    mid: &str,
    seq: usize,
    kind: &str,
    file_name: &str,
    mime: &str,
    bytes: &[u8],
) -> Result<Value, String> {
    if bytes.len() > MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "附件太大（{}MB，上限 {}MB），拒绝落盘",
            bytes.len() / 1024 / 1024,
            MAX_ATTACHMENT_BYTES / 1024 / 1024
        ));
    }
    let d = dir.join(ATTACHMENTS_DIR);
    std::fs::create_dir_all(&d).map_err(|e| format!("创建附件目录失败（{}）：{e}", d.display()))?;
    let name = format!(
        "{}_{}{}",
        sanitize_mid(mid),
        seq,
        ext_from(file_name, mime, kind)
    );
    let path = d.join(&name);
    std::fs::write(&path, bytes).map_err(|e| format!("写附件失败（{}）：{e}", path.display()))?;
    Ok(json!({
        "kind": kind,
        // 微信的图片/语音不带文件名（`file_name` 空）⇒ 用落盘名兵底，
        // 否则 ABB 的 `AttachmentMeta.file_name` 会是空串，agent 无法引用它。
        "file_name": if file_name.is_empty() { name.clone() } else { file_name.to_string() },
        "mime": mime,
        "size": bytes.len(),
        "path": path.display().to_string(),
        "note": "",
    }))
}

/// 把消息 id 变成安全的文件名片段（防路径穿越、防过长）。
///
/// 只留 ASCII alnum 与 `-` `_`；其余（含 `/` `.`）一律 `_`。
fn sanitize_mid(mid: &str) -> String {
    let mut out = String::new();
    for c in mid.chars().take(64) {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// 推扩展名：先用原文件名的后缀，没有就按 kind/mime 推。
///
/// 无扩展名时返回空串（**不编造**）：ABB 靠 mime 也能把文件交给 agent。
fn ext_from(file_name: &str, mime: &str, kind: &str) -> String {
    if let Some(ext) = file_name.rsplit_once('.').map(|(_, e)| e) {
        let ok =
            !ext.is_empty() && ext.len() <= 10 && ext.chars().all(|c| c.is_ascii_alphanumeric());
        if ok {
            return format!(".{}", ext.to_ascii_lowercase());
        }
    }
    let e = match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "application/pdf" => "pdf",
        "audio/mpeg" => "mp3",
        "video/mp4" => "mp4",
        "text/plain" => "txt",
        _ => match kind {
            "image" => "img",
            "audio" | "voice" => "audio",
            "video" => "video",
            _ => "",
        },
    };
    if e.is_empty() {
        String::new()
    } else {
        format!(".{e}")
    }
}

/// 微信消息 → 契约 §3.3 的 `ChannelMessage`。
///
/// **顺手把 `context_token` 记下来**——这是原 ABB `note_context` 的归宿。
fn to_channel_message(ch: &Arc<Channel>, m: &WeixinMessage) -> Option<Value> {
    let from = m.from_user_id.trim();
    if from.is_empty() {
        return None;
    }
    // 只处理别人发来的（自己发的会 echo 回来，忽略以防自循环）。
    let me = ch.user_id.lock().map(|u| u.clone()).unwrap_or_default();
    if !me.is_empty() && from == me {
        return None;
    }
    if !m.context_token.is_empty() {
        if let Ok(mut c) = ch.contexts.lock() {
            c.insert(from.to_string(), m.context_token.clone());
        }
    }
    // 媒体句柄先缓下来：`encrypt_query_param` / `aes_key` 只随这一帧来一次，
    // 丢了就再也拿不到（`download_attachment` 只给 `{message_id, seq}`）。
    let media = m.media_items();
    if !media.is_empty() {
        if let Ok(mut c) = ch.media.lock() {
            c.put(&m.message_id, media.clone());
        }
    }
    // 附件列表只报**本条消息自带**的媒体（引用消息里的媒体走 `quoted` 段，
    // seq 从 `QUOTED_SEQ_BASE` 起）。顺序 = 缓存顺序 = `seq`。
    let attachments: Vec<Value> = media
        .iter()
        .map(|wm| {
            json!({
                "kind": wm.kind,
                "name": wm.file_name,
                // 位置留空是有意的：微信媒体是**加密 CDN 引用**，ABB 拿到 URL
                // 也读不出内容（没有 aes_key）。真正下载走 `download_attachment`。
                "url_or_path": "",
            })
        })
        .collect();
    let text = m.text();
    // **引用/回复的内联内容**（契约 `quoted`）。
    //
    // 为什么必须内联而不是走 `parent_id` 反查：ilink 的 `ref_msg` 只有
    // `title` + 完整 `message_item`，**不带原消息 id**，而 ilink 也没有历史消息
    // API。内容只随这一帧到达，过了就再也拿不到。不给内联字段 = 微信没有引用功能
    // （用户在群里引用一条消息问「这个是啥」，agent 看不到被引用的是什么）。
    //
    // 引用里的媒体同样在这一帧才有句柄 ⇒ 一起并进缓存，让 `download_attachment`
    // 用 `QUOTED_SEQ_BASE + i` 拿得到。
    let quoted_media = m.quoted_media();
    if !quoted_media.is_empty() {
        if let Ok(mut c) = ch.media.lock() {
            c.put_quoted(&m.message_id, quoted_media.clone());
        }
    }
    let quoted_text = m.quoted_text();
    let quoted = if quoted_text.trim().is_empty() && quoted_media.is_empty() {
        Value::Null
    } else {
        json!({
            "text": quoted_text,
            "attachment_count": quoted_media.len(),
        })
    };
    Some(json!({
        "chat_id": from,
        "message_id": m.message_id,
        "sender_id": from,
        "sender_name": "",
        "text": text,
        "ts": if m.create_time_ms > 0 { m.create_time_ms / 1000 } else { 0 },
        "attachments": attachments,
        "quoted": quoted,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_channel() -> Arc<Channel> {
        Arc::new(Channel {
            client: WeixinClient::new("http://127.0.0.1:1", "t", "http://127.0.0.1:1"),
            contexts: Mutex::new(HashMap::new()),
            media: Mutex::new(MediaCache::default()),
            cursor: Mutex::new(String::new()),
            user_id: Mutex::new("me".into()),
            dir: PathBuf::from("."),
            qrcode: Mutex::new(None),
        })
    }

    /// 能力自报必须**只**声明 send_text + poll + login：
    /// 声明 `send_thread_reply` 会让 ABB 走话题路径（微信没有话题）；
    /// 声明 `channel://` 资源会让 ABB 选订阅（本进程不支持）。
    #[test]
    fn tools_list_declares_exactly_what_we_support() {
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
                "poll",
                "send_attachment",
                // 下面两个是契约 §3.4 的可选能力里**微信真能做**的那两个。
                "download_attachment",
                "note_context",
                "login",
                "login_status"
            ]
        );
        assert!(
            !names.contains(&"send_thread_reply".to_string()),
            "微信无话题概念，不该声明它（否则 ABB 不回落 send_text）"
        );
    }

    /// **做不了的可选能力一个都不许声明**（声明了 ABB 就会去调，然后失败）。
    ///
    /// 这条是反向断言，和上面的精确列表互补：精确列表防「误增」，本条防
    /// 「为了让能力清单好看而凑数」——两种毛病都会被 ABB 当真。
    #[test]
    fn unsupported_optional_tools_are_not_declared() {
        let names: Vec<String> = tools()
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        // 逐条带原因，不只断言不包含：后人加回去时会先看到原因。
        for (tool, why) in [
            (
                "get_quoted_message",
                "ilink 的 ref_msg 不带原消息 id，也没有消息库 API",
            ),
            ("create_chat", "微信无建群 API"),
            (
                "user_display_name",
                "没有联系人/资料 API，入站只带 from_user_id",
            ),
            ("typing", "没有表情/reaction API"),
            ("del_typing", "没有表情/reaction API"),
            ("done", "没有表情/reaction API"),
            (
                "note_sender",
                "单聊里 sender_id == chat_id，无另一个发送者可记",
            ),
            ("chat_brief", "微信无群概念，也没有会话资料查询"),
            ("get_chat_info", "微信无群概念，也没有群资料查询"),
        ] {
            assert!(
                !names.contains(&tool.to_string()),
                "不该声明 `{tool}`：{why}（声明了 ABB 会去调）"
            );
        }
    }

    /// 入站必须产出契约要求的 `quoted` 引用内容。
    ///
    /// **这条测的是「字段有没有生产者」**，不是「字段存不存在」。2026-11-04 之前
    /// `ChannelMessage` 只有 `parent_id`（按 id 反查），而 ilink 的 `ref_msg`
    /// **不带原消息 id** ⇒ 用户引用一条消息问「这个啥意思」时，agent 什么也看不到。
    /// `quoted_text()` / `quoted_media()` 两个提取函数因此一直是死代码。
    #[test]
    fn inbound_produces_inline_quoted_content() {
        let ch = fake_channel();
        let m: WeixinMessage = serde_json::from_value(json!({
            "from_user_id": "wx_u1",
            "message_id": "m1",
            "item_list": [{
                "type": 1,
                "text_item": {"text": "这个啥意思"},
                "ref_msg": {
                    "title": "原消息摘要",
                    "message_item": {"type": 1, "text_item": {"text": "被引用的原文"}},
                },
            }],
        }))
        .expect("构造入站消息");
        let v = to_channel_message(&ch, &m).expect("应当产出 ChannelMessage");
        let q = &v["quoted"];
        assert_eq!(
            q["text"], "原消息摘要 | 被引用的原文",
            "被引用内容必须内联带出（摘要 + 原文），实际：{q}"
        );
        assert_eq!(q["attachment_count"], 0, "纯文本引用没有附件");
    }

    /// 没有引用时 `quoted` 必须是 null——不能让下游看到空引用块。
    #[test]
    fn inbound_without_quote_has_null_quoted() {
        let ch = fake_channel();
        let m: WeixinMessage = serde_json::from_value(json!({
            "from_user_id": "wx_u1",
            "message_id": "m2",
            "item_list": [{"type": 1, "text_item": {"text": "普通消息"}}],
        }))
        .expect("构造入站消息");
        let v = to_channel_message(&ch, &m).expect("应当产出");
        assert!(
            v["quoted"].is_null(),
            "无引用时必须 null，实际：{}",
            v["quoted"]
        );
    }

    /// 引用附件与主附件**分两段**存在，不互相串位。
    ///
    /// `seq >= QUOTED_SEQ_BASE` 取引用段、否则取主段（契约 §3.4）。
    /// 混在一张表里会让「本条的第 0 个」与「它引用的第 0 个」变成同一个。
    #[test]
    fn quoted_media_lives_in_its_own_segment() {
        let mut c = MediaCache::default();
        c.put("m1", vec![sample_media("image")]);
        c.put_quoted("m1", vec![sample_media("file"), sample_media("image")]);
        assert_eq!(c.get("m1").map(|v| v.len()), Some(1), "主段仍是 1 个");
        assert_eq!(
            c.get_quoted("m1").map(|v| v.len()),
            Some(2),
            "引用段是 2 个"
        );
        assert!(
            c.get_quoted("other").is_none(),
            "没引用过的消息不该有引用段"
        );
        // 引用段同样要封顶（否则长跑进程线性吃内存）。
        for i in 0..(MAX_CACHED_MESSAGES + 5) {
            c.put_quoted(&format!("q{i}"), vec![sample_media("image")]);
        }
        assert_eq!(c.quoted.len(), MAX_CACHED_MESSAGES, "引用段也要封顶");
    }

    /// 媒体缓存容量上限真的生效（否则长跑进程随消息量线性吃内存）。
    #[test]
    fn media_cache_is_bounded_and_keeps_newest() {
        let mut c = MediaCache::default();
        for i in 0..(MAX_CACHED_MESSAGES + 10) {
            c.put(&format!("m{i}"), vec![sample_media("image")]);
        }
        assert_eq!(c.entries.len(), MAX_CACHED_MESSAGES, "缓存必须封顶");
        let newest = format!("m{}", MAX_CACHED_MESSAGES + 9);
        assert!(c.get(&newest).is_some(), "最新的必须还在");
        assert!(c.get("m0").is_none(), "最旧的必须被挤掉");
    }

    /// 同一条消息重复入站（重拉/重试）不得在缓存里出现两份，且不占两个位置。
    #[test]
    fn media_cache_overwrites_same_message() {
        let mut c = MediaCache::default();
        c.put("m1", vec![sample_media("image")]);
        c.put("m1", vec![sample_media("image"), sample_media("file")]);
        assert_eq!(c.entries.len(), 1);
        assert_eq!(c.get("m1").unwrap().len(), 2);
    }

    /// 没媒体的消息不占缓存位（否则几条纯文本就能把带图的那条挤掉）。
    #[test]
    fn media_cache_ignores_messages_without_media() {
        let mut c = MediaCache::default();
        c.put("m1", vec![]);
        assert!(c.entries.is_empty());
    }

    fn sample_media(kind: &str) -> WechatMedia {
        WechatMedia {
            kind: kind.into(),
            file_name: if kind == "file" {
                "报告.pdf".into()
            } else {
                String::new()
            },
            encrypt_query_param: "qp".into(),
            aes_key: String::new(),
            aeskey_hex: String::new(),
            full_url: "https://cdn.example/x".into(),
            voice_text: String::new(),
        }
    }

    /// 入站媒体必须**既进缓存又进 attachments**，且两处顺序一致（`seq` 就是这个下标）。
    #[test]
    fn incoming_media_is_cached_and_listed() {
        let ch = fake_channel();
        let m: WeixinMessage = serde_json::from_value(json!({
            "from_user_id": "user-a",
            "message_id": "9001",
            "message_type": 1,
            "item_list": [
                {"type": 2, "image_item": {"media": {"encrypt_query_param": "qp1"}}},
                {"type": 4, "file_item": {"file_name": "a.pdf", "media": {"encrypt_query_param": "qp2", "aes_key": "k"}}}
            ]
        }))
        .unwrap();
        let cm = to_channel_message(&ch, &m).expect("应当转出一条");
        assert_eq!(cm["attachments"].as_array().unwrap().len(), 2);
        assert_eq!(cm["attachments"][0]["kind"], "image");
        assert_eq!(cm["attachments"][1]["name"], "a.pdf");
        // 位置留空：ABB 拿到加密 URL 也读不出内容，下载必须走工具
        assert_eq!(cm["attachments"][0]["url_or_path"], "");
        let cached = ch.media.lock().unwrap();
        let items = cached
            .get("9001")
            .expect("媒体句柄必须被缓存（丢掉就再也拿不到）");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].encrypt_query_param, "qp1");
    }

    /// 未知 `message_id` ⇒ 明确报错（不是回空串/假路径）。
    #[test]
    fn download_unknown_message_errors() {
        let e = call_tool(
            &fake_channel(),
            &json!({"name":"download_attachment","arguments":{"message_id":"nope","seq":0}}),
        )
        .unwrap_err();
        assert!(e.contains("媒体缓存"), "报因要说清缓存里没有：{e}");
    }

    /// `seq` 越界 ⇒ 报错并带上真实附件数（否则调用方无从判断是自己算错还是渠道丢件）。
    #[test]
    fn download_seq_out_of_range_errors() {
        let ch = fake_channel();
        ch.media
            .lock()
            .unwrap()
            .put("m1", vec![sample_media("image")]);
        let e = call_tool(
            &ch,
            &json!({"name":"download_attachment","arguments":{"message_id":"m1","seq":3}}),
        )
        .unwrap_err();
        assert!(e.contains("越界"), "要指名是越界：{e}");
        assert!(e.contains('1'), "要带上实际附件数：{e}");
    }

    /// 缺 `seq` ⇒ 报错（默认 0 会让调用方“猜错附件”时静默拿到另一个文件）。
    #[test]
    fn download_missing_seq_errors() {
        let e = call_tool(
            &fake_channel(),
            &json!({"name":"download_attachment","arguments":{"message_id":"m1"}}),
        )
        .unwrap_err();
        assert!(e.contains("seq"), "报因要点名缺哪个字段：{e}");
    }

    /// 落盘的路径、大小、文件名都要对；图片无文件名时用落盘名兵底（不能是空串）。
    #[test]
    fn save_attachment_writes_bytes_and_reports_path() {
        let dir = std::env::temp_dir().join(format!(
            "abb-wx-attach-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let meta = save_attachment(&dir, "9001/../x", 2, "image", "", "image/png", b"hello")
            .expect("应当落盘");
        let path = std::path::PathBuf::from(meta["path"].as_str().unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert_eq!(meta["size"], 5);
        assert_eq!(meta["kind"], "image");
        assert_eq!(
            meta["file_name"], "9001____x_2.png",
            "无原文件名时用落盘名兵底"
        );
        assert!(
            path.starts_with(dir.join("attachments")),
            "必须落在 <dir>/attachments 下：{}",
            path.display()
        );
        assert!(
            !meta["file_name"].as_str().unwrap().contains('/'),
            "文件名里不得残留路径分隔符"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `note_context` 写的是 `send_text` 用的**同一份** map（否则记了也没用）。
    #[test]
    fn note_context_feeds_send_text() {
        let ch = fake_channel();
        let r = call_tool(
            &ch,
            &json!({"name":"note_context","arguments":{
                "chat_id":"user-a","context_token":"ctx-9"}}),
        )
        .expect("应当成功");
        assert!(r["content"][0]["text"].as_str().unwrap().contains("stored"));
        assert_eq!(
            ch.contexts.lock().unwrap().get("user-a"),
            Some(&"ctx-9".to_string())
        );
    }

    /// 空 `chat_id` / 空 token ⇒ 报错：空 token 入表等于把“能发”变成“发不出去”。
    #[test]
    fn note_context_rejects_empty_fields() {
        let ch = fake_channel();
        let e = call_tool(
            &ch,
            &json!({"name":"note_context","arguments":{"chat_id":"u","context_token":""}}),
        )
        .unwrap_err();
        assert!(e.contains("context_token"), "报因要点名缺哪个字段：{e}");
        let e = call_tool(
            &ch,
            &json!({"name":"note_context","arguments":{"chat_id":"","context_token":"t"}}),
        )
        .unwrap_err();
        assert!(e.contains("chat_id"), "报因要点名缺哪个字段：{e}");
        assert!(ch.contexts.lock().unwrap().is_empty(), "失败不得写进去");
    }

    /// resources/list 必须是空数组（不是 null）：ABB 的 `capabilities_from`
    /// 把「缺 resources 字段」当没有资源，但显式空数组更明确。
    #[test]
    fn resources_list_is_empty_array_not_null() {
        let r = handle(
            &fake_channel(),
            &json!({"jsonrpc":"2.0","id":1,"method":"resources/list"}),
        )
        .expect("要回包");
        assert_eq!(r["result"]["resources"], json!([]));
    }

    /// 自己发的消息不该回灌（防自循环）。
    #[test]
    fn own_message_is_ignored() {
        let ch = fake_channel();
        let m = WeixinMessage {
            from_user_id: "me".into(),
            message_id: "1".into(),
            ..Default::default()
        };
        assert!(
            to_channel_message(&ch, &m).is_none(),
            "自己发的不该进 poll 结果"
        );
    }

    /// **`context_token` 必须被记下来**——这是 send_text 能工作的前提，
    /// 也是原 ABB `note_context` 的归宿。
    #[test]
    fn incoming_message_records_context_token() {
        let ch = fake_channel();
        let m = WeixinMessage {
            from_user_id: "user-a".into(),
            message_id: "1".into(),
            context_token: "ctx-123".into(),
            ..Default::default()
        };
        let cm = to_channel_message(&ch, &m).expect("应当转出一条");
        assert_eq!(cm["chat_id"], "user-a");
        assert_eq!(
            ch.contexts.lock().unwrap().get("user-a"),
            Some(&"ctx-123".to_string()),
            "context_token 没记下来 ⇒ 后续 send_text 会当新会话发"
        );
    }

    /// 空 from 的消息丢弃（微信会推系统事件，那些没有 sending user）。
    #[test]
    fn empty_sender_is_dropped() {
        let ch = fake_channel();
        let m = WeixinMessage {
            from_user_id: "".into(),
            message_id: "x".into(),
            ..Default::default()
        };
        assert!(to_channel_message(&ch, &m).is_none());
    }

    /// **没调 `login` 就调 `login_status` 必须报错**，不能说「pending」——
    /// 后者会让用户一直等一个根本不存在的二维码。
    #[test]
    fn login_status_without_login_errors() {
        let e = call_tool(
            &fake_channel(),
            &json!({"name":"login_status","arguments":{}}),
        )
        .unwrap_err();
        assert!(e.contains("还没调 login"), "报因要指路：{e}");
    }

    /// 登录成功后 **token 不得出现在工具结果里**。
    ///
    /// 工具结果会被 ABB 落进会话历史/日志；凭证只该走一次性的 stderr 提示。
    ///
    /// **必须调真函数 `login_result`**：第一版在测试里自己写了一遍 `json!`，
    /// 于是「把 token 塞进结果」的变异抓不到——那是空断言。
    #[test]
    fn token_never_appears_in_tool_result() {
        let secret = "SECRET-TOKEN-SHOULD-NOT-LEAK";
        let login = weixin::WeixinLogin {
            token: secret.to_string(),
            base_url: "https://example".to_string(),
            user_id: "u1".to_string(),
        };
        let body = login_result(&login);
        let text = body.to_string();
        assert!(
            !text.contains(secret),
            "工具结果里出现了 token 本身：{text}"
        );
        assert!(text.contains("token_len"), "应当只暴露长度，不暴露值");
        assert_eq!(body["user_id"], "u1", "user_id 可以明文（它不是凭证）");
    }

    /// base64 解码要和 ABB 的编码对称（含 padding 的三种情形）。
    #[test]
    fn base64_roundtrip() {
        for s in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
            let enc = {
                // 用与 ABB 相同的标准表编码，再解码回来
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
        // 非法输入返回 None（不是 panic、不是空 Vec）
        assert_eq!(base64_decode("!!!!"), None);
        assert_eq!(base64_decode("abc"), None, "长度不是 4 的倍数");
    }

    /// 非法 base64 ⇒ 明确报错，不是静默发空内容。
    #[test]
    fn invalid_base64_errors() {
        let e = call_tool(
            &fake_channel(),
            &json!({"name":"send_attachment","arguments":{
                "chat_id":"c","name":"x.png","data_b64":"!!!!"}}),
        )
        .unwrap_err();
        assert!(e.contains("合法 base64"), "报因要说清是编码问题：{e}");
    }

    /// 缺 data_b64 ⇒ 报错（不是发一个空文件）。
    #[test]
    fn missing_b64_errors() {
        let e = call_tool(
            &fake_channel(),
            &json!({"name":"send_attachment","arguments":{"chat_id":"c","name":"x"}}),
        )
        .unwrap_err();
        assert!(e.contains("data_b64"), "报因要点名缺哪个字段：{e}");
    }

    /// 未实现的方法要报错（不是静默成功）。
    #[test]
    fn unknown_method_returns_error() {
        let r = handle(
            &fake_channel(),
            &json!({"jsonrpc":"2.0","id":9,"method":"nope"}),
        )
        .expect("要回包");
        assert!(r.get("error").is_some(), "未知方法必须回 error");
    }
}
