//! 渠道 MCP 服务 → [`Messenger`]（总线 S1c）。
//!
//! 目标拓扑见 `docs/mcp-bus-architecture.md`。本模块把 [`crate::mcp_client`] 的
//! stdio client 包成 [`Messenger`] 实现，于是**内置渠道与 MCP 渠道在上层完全同构**——
//! `bridge/`、`deliver`、`schedule` 都不需要知道消息从哪来。
//!
//! ## 能力由服务自报，不信配置声明
//!
//! 建连时先 `tools/list` + `resources/list`，用 [`crate::mcp_bus::capabilities_from`]
//! 归纳出 [`ChannelCapabilities`] 再 `validate()`。缺失必需能力 ⇒ **建连失败**，
//! 而不是「先连上、发消息时才发现发不出去」：
//! 后者在托盘里的症状是「渠道显示在线、消息发不出且没人知道为什么」。
//!
//! ## 本步骤只做出站
//!
//! `send_text`（必需）+ `send_thread_reply`（可选，缺失回落 `send_text`）。
//! 附件、引用、群资料、建群、表情等**一律显式失败/返回 None**——不静默假装成功
//! （本仓在 `send_attachment` 的默认实现上踩过：默认把路径当文本发出去还返回 Ok）。
//!
//! 入站（`resources/subscribe` 或 `poll`）不在本模块：它在 `mcp_inbound.rs`
//! （协议）与 `mcp_channel_loop.rs`（驱动）。

// 这里曾有模块级 `#![allow(dead_code)]`，注释写「还没接进 service.rs 的 bot 装配（S1d）」。
//
// **那个 allow 已经移除**（S1d 完成）：`messenger::build` 的 `is_mcp()` 分支真的在用本类型。
// 只剩两个访问器单点 allow（见下），留给 S2 入站与 GUI 状态展示。
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::mcp_bus::{
    capabilities_from, ChannelCapabilities, TOOL_SEND_TEXT, TOOL_SEND_THREAD_REPLY,
};
use crate::mcp_client::{ClientError, StdioChannelClient};
use crate::messenger::Messenger;
use anyhow::Result;

/// MCP 渠道服务的 [`Messenger`] 实现。
///
/// 内部是 `Mutex<client>`：`Messenger` 的异步方法可能被并发调用，而 stdio 是
/// **单条流**——两条请求交错写会把 server 搞乱。用锁串行化最省事且正确
/// （渠道发消息本来也不是高并发场景；真要并发该换多连接，不是放开锁）。
pub struct McpChannelMessenger {
    name: String,
    client: Mutex<StdioChannelClient>,
    caps: ChannelCapabilities,
}

impl McpChannelMessenger {
    /// 建连并**校验能力**：拿到 `tools/list` 与 `resources/list` 后归纳，
    /// 缺 `send_text` 或完全没有入站能力 ⇒ 直接失败。
    ///
    /// 为什么在建连时就校验：等到第一条消息要发时才失败，运维只会看到
    /// 「ABB 显示这个渠道在跑，但消息发不出去」。
    pub fn connect(
        name: &str,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
    ) -> std::result::Result<Self, String> {
        let mut client = StdioChannelClient::spawn(name, program, args, envs)
            .map_err(|e| format!("连不上渠道 MCP 服务 `{name}`：{e}"))?;

        // `initialize` 是 MCP 握手第一步。有的 server 不要求也能调工具，
        // 但规范要求先握手；失败不致命（下面 tools/list 才是真判据），
        // 所以这里**记下但不因此判定不可用**——避免对宽松实现的 server 误杀。
        let _ = client.call(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "abb", "version": env!("CARGO_PKG_VERSION")},
            }),
        );

        let tools = client
            .call("tools/list", json!({}))
            .map_err(|e| format!("`{name}` 的 tools/list 失败：{e}"))?;
        // resources/list 很多 server 不实现 ⇒ 失败当成「服务里没有资源」，
        // 而不是整条渠道不可用（入站还可能走 poll）。
        let resources = client
            .call("resources/list", json!({}))
            .unwrap_or(Value::Null);

        let caps = capabilities_from(&tools, &resources)
            .map_err(|e| format!("`{name}` 的能力清单形状不对：{e:?}"))?;
        caps.validate()
            .map_err(|e| format!("`{name}` 不满足渠道契约：{e}"))?;

        Ok(Self {
            name: name.to_string(),
            client: Mutex::new(client),
            caps,
        })
    }

    /// 服务名（日志与报因用）。
    ///
    /// 模块级 `allow(dead_code)` 已移除——`messenger::build` 真用上了本类型。
    /// 这两个访问器留给 S2 入站与 GUI 状态展示，**逐项**标注而不是把整个模块放开：
    /// 逐项标注会让「哪些还没接」一目了然，模块级放开会把将来新加的死代码一起藏住。
    #[allow(dead_code)]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[allow(dead_code)]
    pub fn capabilities(&self) -> &ChannelCapabilities {
        &self.caps
    }

    /// 用一个**已经连好**的 client 组装（端到端测试用）。
    ///
    /// 为什么不复用 [`Self::connect`]：那条会自己 spawn 进程并做能力校验，
    /// 而测试需要「用同一个已握手的 client 既走协议层又走 Messenger 层」——
    /// 否则验的是两个不同进程，说明不了链路一致。
    #[cfg(test)]
    pub fn from_client_for_test(
        client: crate::mcp_client::StdioChannelClient,
        name: &str,
        caps: ChannelCapabilities,
    ) -> Self {
        Self {
            name: name.to_string(),
            client: Mutex::new(client),
            caps,
        }
    }

    /// 取回内部 client（端到端测试用：同一个进程继续问统计）。
    #[cfg(test)]
    pub fn into_client_for_test(self) -> crate::mcp_client::StdioChannelClient {
        self.client.into_inner().expect("测试里不该中毒")
    }

    /// 调一个工具，返回文本结果。
    ///
    /// MCP 的工具结果形状是 `{content: [{type:"text", text:"…"}, …]}`。
    /// 这里**把所有 text 段拼起来**：有的 server 用多段表示分段消息，
    /// 丢掉后面的段就是静默丢内容。非 text 段（图片等）跳过——本步骤不支持附件。
    fn call_tool(client: &mut StdioChannelClient, tool: &str, args: Value) -> Result<String> {
        let resp = client
            .call("tools/call", json!({ "name": tool, "arguments": args }))
            .map_err(|e| map_err(tool, e))?;
        Ok(text_of_tool_result(&resp))
    }

    /// 调一个**可选**工具（契约 §3.4）。
    ///
    /// 三条回落规则，与 trait 的默认实现对齐：
    /// - 服务没声明该工具 ⇒ `None`，调用方回落 trait 默认；
    /// - 调用失败 ⇒ 记日志后 `None`（**不向上报错**：可选能力失败不该让消息处理失败）；
    /// - 返回内容不是合法 JSON ⇒ 也 `None`（同上）。
    ///
    /// 为什么失败只记日志不报错：这些能力都是「锦上添花」——
    /// 拿不到引用消息，消息仍要处理；查不到人名，用 id 兜底也行。
    /// 让它们阻断主流程，等于一个次要功能坏掉就吞掉整条消息。
    fn call_optional(&self, tool: &str, args: Value) -> Option<Value> {
        if !self.caps.has_tool(tool) {
            return None;
        }
        let mut c = match self.client.lock() {
            Ok(c) => c,
            Err(_) => {
                crate::log!("[mcp:{}] 可选工具 {tool} 跳过：client 锁中毒", self.name);
                return None;
            }
        };
        match Self::call_tool(&mut c, tool, args) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(v) => Some(v),
                Err(e) => {
                    crate::log!(
                        "[mcp:{}] 可选工具 {tool} 返回的不是 JSON（{e}），按不支持处理",
                        self.name
                    );
                    None
                }
            },
            Err(e) => {
                crate::log!("[mcp:{}] 可选工具 {tool} 失败：{e:#}", self.name);
                None
            }
        }
    }
}

/// 从 `tools/call` 的结果里取出文本。
///
/// **形状不对要报错而不是返回空串**：返回空串会让上层以为「发出去了，内容为空」，
/// 而实际是服务实现不符合契约。
fn text_of_tool_result(resp: &Value) -> String {
    let Some(arr) = resp.get("content").and_then(|v| v.as_array()) else {
        return String::new();
    };
    arr.iter()
        .filter(|seg| seg.get("type").and_then(|v| v.as_str()) == Some("text"))
        .filter_map(|seg| seg.get("text").and_then(|v| v.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 体积闸。抽成函数是因为**要在两处用**（读前看 metadata、读后再看实际字节数）。
///
/// 报因必须带具体大小与上限：只说"太大"让人不知道差多少、要不要压缩。
fn reject_if_too_big(len: u64) -> Result<()> {
    let max = crate::mcp_client::MAX_ATTACHMENT_BYTES as u64;
    if len > max {
        return Err(anyhow::anyhow!(
            "附件太大：{:.1} MB，上限 {:.0} MB（base64 后需留在单帧上限内）",
            len as f64 / 1_048_576.0,
            max as f64 / 1_048_576.0
        ));
    }
    Ok(())
}

/// 标准 base64（含 padding）。手写不引 crate——与 ABB 既有风格一致
/// （`wechat.rs` 里的 base64 也是手写的，避免为几行代码拉依赖树）。
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// 把 client 错误翻成「哪个工具、为什么」。带工具名是因为一个渠道可能有多个工具，
/// 只说「写不进去」运维没法判断是哪一步。
fn map_err(tool: &str, e: ClientError) -> anyhow::Error {
    anyhow::anyhow!("渠道工具 `{tool}` 调用失败：{e}")
}

#[async_trait::async_trait]
impl Messenger for McpChannelMessenger {
    async fn send_text(&self, chat_id: &str, text: &str) -> Result<()> {
        let mut c = self
            .client
            .lock()
            .map_err(|_| anyhow::anyhow!("渠道 client 锁中毒"))?;
        Self::call_tool(
            &mut c,
            TOOL_SEND_TEXT,
            json!({ "chat_id": chat_id, "text": text }),
        )?;
        Ok(())
    }

    async fn send_thread_reply(&self, chat_id: &str, message_id: &str, text: &str) -> Result<()> {
        // 服务没声明 `send_thread_reply` ⇒ 回落普通发送（契约 §3.1）。
        // **不能静默忽略这条消息**：回落是明确行为，不是「不支持就算了」。
        if !self.caps.has_send_thread_reply {
            return self.send_text(chat_id, text).await;
        }
        let mut c = self
            .client
            .lock()
            .map_err(|_| anyhow::anyhow!("渠道 client 锁中毒"))?;
        Self::call_tool(
            &mut c,
            TOOL_SEND_THREAD_REPLY,
            json!({ "chat_id": chat_id, "message_id": message_id, "text": text }),
        )?;
        Ok(())
    }

    /// 发附件（契约 §3.5）。
    ///
    /// 三道闸，顺序是刻意的：
    ///
    /// 1. **服务没声明工具** ⇒ 明确失败并说清「该渠道只能发文本」。
    ///    不假装成功——历史教训：旧默认实现把路径当文本发出去还返回 `Ok`。
    /// 2. **文件读不出来** ⇒ 报 IO 错误（路径错、权限不足）。
    /// 3. **体积超限** ⇒ 报「附件太大（X MB，上限 Y MB）」。
    ///
    /// 第 3 道必须在**发送之前**：否则超限会一路走到 `mcp_client` 的单行上限，
    /// 变成一个笼统的 `LineTooLong`，与「渠道不支持附件」难以区分。
    async fn send_attachment(
        &self,
        chat_id: &str,
        meta: &crate::attachments::AttachmentMeta,
    ) -> Result<()> {
        if !self.caps.has_send_attachment {
            return Err(anyhow::anyhow!(
                "渠道 `{}` 的 MCP 服务未声明 `{}` 工具 ⇒ 该渠道只能发文本",
                self.name,
                crate::mcp_bus::TOOL_SEND_ATTACHMENT
            ));
        }
        let path = std::path::Path::new(&meta.path);
        // **先看大小，再读内容**（评审 7f57d7a8 的 non-blocking）。
        //
        // 第一版把体积闸放在 `fs::read` 之后：功能上对（会拒绝），但「防内存爆炸」
        // 这个目的**完全没达到**——20 MB 的超限文件照样先被整份读进内存才被拒。
        // metadata 是 O(1)，读文件是 O(size)，顺序反了等于没防。
        let meta_len = std::fs::metadata(path)
            .map_err(|e| anyhow::anyhow!("读附件失败（{}）：{e}", path.display()))?
            .len();
        reject_if_too_big(meta_len)?;
        let bytes = std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("读附件失败（{}）：{e}", path.display()))?;
        // **读后还要再判一次**：metadata 与实际读到的大小可能不一致（文件在两次调用
        // 之间被改、或路径指向特殊文件如 /proc、FIFO）。只信 metadata 会漏掉那种。
        reject_if_too_big(bytes.len() as u64)?;
        // base64 内联：Jev 裁定 `base64_inline`（p=0.78/conf=0.67），
        // 理由是 ABB 与渠道 server 通常同机（都是本地子进程），
        // 两段式上传对这个场景过重；path_ref 会把 ABB 的文件系统暴露给 server。
        let data = base64_encode(&bytes);
        let mut c = self
            .client
            .lock()
            .map_err(|_| anyhow::anyhow!("渠道 client 锁中毒"))?;
        Self::call_tool(
            &mut c,
            crate::mcp_bus::TOOL_SEND_ATTACHMENT,
            serde_json::json!({
                "chat_id": chat_id,
                "name": meta.file_name,
                "mime": meta.mime,
                "kind": meta.kind,
                "data_b64": data,
            }),
        )?;
        Ok(())
    }

    // ── 契约 §3.4 的可选能力（缺失即回落 trait 默认实现）──────────────
    //
    // 全部走 `call_optional`：服务没声明 / 调用失败 / 返回非 JSON ⇒ 一律 `None`，
    // 由 trait 默认值兜底。**不向上报错**——次要能力失败不该吞掉整条消息。

    /// A：取被引用消息的内容。缺它 ⇒ agent 看不到引用上文。
    async fn get_quoted_message(
        &self,
        message_id: &str,
    ) -> Option<crate::messenger::QuotedMessage> {
        let v = self.call_optional(
            crate::mcp_bus::TOOL_GET_QUOTED_MESSAGE,
            json!({ "message_id": message_id }),
        )?;
        Some(crate::messenger::QuotedMessage {
            text: v.get("text").and_then(|x| x.as_str())?.to_string(),
            // 附件描述由**调用方**在入站时已经拿到（`Ev` 里），这里不重复传——
            // 契约只要求回文本；server 想给附件信息时放 `text` 里即可。
            attachments: vec![],
            msg_type: v
                .get("msg_type")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            unparsed_tags: vec![],
        })
    }

    /// A：下载入站附件。
    ///
    /// **参数只有 `{message_id, seq}`**：怎么下载是渠道的事（契约 §4「ABB 不负责
    /// 协议细节」）。server 自己反查那条消息的第 `seq` 个附件、下载到它自己的目录，
    /// 把最终路径告诉 ABB。
    async fn download_attachment(
        &self,
        _bot_key: &str,
        mid: &str,
        seq: usize,
        _desc: &crate::attachments::AttachmentDesc,
    ) -> Option<crate::attachments::AttachmentMeta> {
        let v = self.call_optional(
            crate::mcp_bus::TOOL_DOWNLOAD_ATTACHMENT,
            json!({ "message_id": mid, "seq": seq }),
        )?;
        // 形状不对 ⇒ 返回带 note 的占位（**不静默丢消息**，与内置渠道同口径）。
        let path = v.get("path").and_then(|x| x.as_str()).unwrap_or("");
        if path.is_empty() {
            return Some(crate::attachments::AttachmentMeta {
                kind: v
                    .get("kind")
                    .and_then(|x| x.as_str())
                    .unwrap_or("file")
                    .to_string(),
                source: "mcp".to_string(),
                file_name: v
                    .get("file_name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                mime: String::new(),
                size: 0,
                path: String::new(),
                sha256: String::new(),
                note: format!(
                    "渠道 `{}` 说下载了但没给路径：{}",
                    self.name,
                    v.get("note")
                        .and_then(|x| x.as_str())
                        .unwrap_or("（无说明）")
                ),
            });
        }
        Some(crate::attachments::AttachmentMeta {
            kind: v
                .get("kind")
                .and_then(|x| x.as_str())
                .unwrap_or("file")
                .to_string(),
            source: "mcp".to_string(),
            file_name: v
                .get("file_name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            mime: v
                .get("mime")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            size: v.get("size").and_then(|x| x.as_u64()).unwrap_or(0),
            path: path.to_string(),
            sha256: v
                .get("sha256")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            note: v
                .get("note")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        })
    }

    /// A：建群。缺它 ⇒ 只能手动建（聊天侧回落登记制指引）。
    async fn create_chat(
        &self,
        name: &str,
        description: &str,
        owner_user_id: &str,
    ) -> Result<String, String> {
        let v = self
            .call_optional(
                crate::mcp_bus::TOOL_CREATE_CHAT,
                json!({
                    "name": name,
                    "description": description,
                    "owner_user_id": owner_user_id,
                }),
            )
            .ok_or_else(|| {
                format!(
                    "渠道 `{}` 的 MCP 服务未声明 `{}` ⇒ 只能手动建群后登记",
                    self.name,
                    crate::mcp_bus::TOOL_CREATE_CHAT
                )
            })?;
        v.get("chat_id")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| format!("渠道 `{}` 建了群但没给 chat_id", self.name))
    }

    /// A：反查展示名。缺它 ⇒ 授权记录里只有 id。
    async fn user_display_name(&self, user_id: &str) -> Option<String> {
        self.call_optional(
            crate::mcp_bus::TOOL_USER_DISPLAY_NAME,
            json!({ "user_id": user_id }),
        )?
        .get("name")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
    }

    /// B：处理中表情。
    async fn typing(&self, message_id: &str) -> Option<String> {
        self.call_optional(
            crate::mcp_bus::TOOL_TYPING,
            json!({ "message_id": message_id }),
        )?
        .get("reaction_id")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
    }

    /// B：撤销处理中表情。
    async fn del_typing(&self, message_id: &str, reaction_id: Option<String>) {
        let _ = self.call_optional(
            crate::mcp_bus::TOOL_DEL_TYPING,
            json!({ "message_id": message_id, "reaction_id": reaction_id }),
        );
    }

    /// B：完成表情。
    async fn done(&self, message_id: &str) {
        let _ = self.call_optional(
            crate::mcp_bus::TOOL_DONE,
            json!({ "message_id": message_id }),
        );
    }

    /// B：记回复上下文。**同步方法**（trait 如此），且无返回。
    ///
    /// 失败静默：这是状态同步，调用方（`bridge`）本来也不看返回值。
    fn note_context(&self, chat_id: &str, context_token: &str) {
        let _ = self.call_optional(
            crate::mcp_bus::TOOL_NOTE_CONTEXT,
            json!({ "chat_id": chat_id, "context_token": context_token }),
        );
    }

    /// B：记最近发送者（钉钉群聊 @ 用）。**同步方法**，无返回。
    fn note_sender(&self, chat_id: &str, sender_id: &str) {
        let _ = self.call_optional(
            crate::mcp_bus::TOOL_NOTE_SENDER,
            json!({ "chat_id": chat_id, "sender_id": sender_id }),
        );
    }

    /// B：会话类型 + 会话名。缺它 ⇒ 历史回填跳过该会话。
    async fn chat_brief(&self, chat_id: &str) -> Option<(String, String)> {
        let v = self.call_optional(
            crate::mcp_bus::TOOL_CHAT_BRIEF,
            json!({ "chat_id": chat_id }),
        )?;
        Some((
            v.get("chat_type").and_then(|x| x.as_str())?.to_string(),
            v.get("chat_name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        ))
    }

    /// B：群资料（群名 + 群介绍）。
    async fn get_chat_info(&self, chat_id: &str) -> Option<(String, String)> {
        let v = self.call_optional(
            crate::mcp_bus::TOOL_GET_CHAT_INFO,
            json!({ "chat_id": chat_id }),
        )?;
        Some((
            v.get("name").and_then(|x| x.as_str())?.to_string(),
            v.get("description")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_result_text_joins_all_segments() {
        // 多段是合法的（server 用它表示分段消息）；只取第一段就是静默丢内容。
        let resp = json!({"content":[
            {"type":"text","text":"第一段"},
            {"type":"image","data":"…"},
            {"type":"text","text":"第二段"}
        ]});
        assert_eq!(text_of_tool_result(&resp), "第一段\n第二段");
    }

    #[test]
    fn tool_result_missing_content_is_empty_not_panic() {
        assert_eq!(text_of_tool_result(&json!({})), "");
        assert_eq!(text_of_tool_result(&json!({"content":"nope"})), "");
    }

    /// 用 shell 脚本冒充一个**能力齐备**的渠道服务。
    fn spawn_fake(body: &str) -> std::result::Result<McpChannelMessenger, String> {
        McpChannelMessenger::connect("fake", "/bin/sh", &["-c".into(), body.into()], &[])
    }

    #[test]
    fn connect_fails_loudly_when_send_text_is_missing() {
        // 有入站能力但没有 send_text ⇒ 必须建连失败，而不是「连上了但发不出去」。
        let err = spawn_fake(
            r#"while IFS= read -r line; do
                 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                 case "$line" in
                   *tools/list*)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"poll"}]}}\n' "$id" ;;
                   *resources/list*)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"resources":[{"uri":"channel://inbox"}]}}\n' "$id" ;;
                   *)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
                 esac
               done"#,
        )
        .err()
        .unwrap_or_else(|| panic!("缺 send_text 时不该连上"));
        assert!(err.contains("send_text"), "报因要指名缺哪个工具：{err}");
    }

    #[test]
    fn connect_fails_loudly_when_there_is_no_inbound() {
        let err = spawn_fake(
            r#"while IFS= read -r line; do
                 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                 case "$line" in
                   *tools/list*)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"send_text"}]}}\n' "$id" ;;
                   *resources/list*)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"resources":[]}}\n' "$id" ;;
                   *)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
                 esac
               done"#,
        )
        .err()
        .unwrap_or_else(|| panic!("无入站能力时不该连上"));
        assert!(err.contains("入站"), "报因要指明缺入站能力：{err}");
    }

    /// 声明了工具但**文件读不出来** ⇒ 报 IO 错（不是静默成功）。
    #[tokio::test]
    async fn send_attachment_reports_unreadable_file() {
        let m = spawn_fake(
            r#"while IFS= read -r line; do
                 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                 case "$line" in
                   *tools/list*)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"send_text"},{"name":"send_attachment"}]}}\n' "$id" ;;
                   *resources/list*)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"resources":[{"uri":"channel://inbox"}]}}\n' "$id" ;;
                   *) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
                 esac
               done"#,
        )
        .unwrap_or_else(|e| panic!("应当连上：{e}"));
        assert!(m.capabilities().has_send_attachment, "该假服务声明了工具");
        let meta = crate::attachments::AttachmentMeta {
            kind: "file".into(),
            source: "test".into(),
            file_name: "nope.bin".into(),
            mime: "application/octet-stream".into(),
            size: 1,
            path: "/definitely/not/here/nope.bin".into(),
            sha256: String::new(),
            note: String::new(),
        };
        let err = m.send_attachment("c", &meta).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("读附件失败"),
            "读不出来要说清是 IO 问题：{msg}"
        );
    }

    /// **体积超限必须在发送前拒绝**并说清大小。
    ///
    /// 不能让它一路走到 `mcp_client` 的单行上限——那里的报错是笼统的
    /// `LineTooLong`，与「渠道不支持附件」难以区分。
    #[tokio::test]
    async fn send_attachment_rejects_oversized_before_sending() {
        let m = spawn_fake(
            r#"while IFS= read -r line; do
                 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                 case "$line" in
                   *tools/list*)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"send_text"},{"name":"send_attachment"}]}}\n' "$id" ;;
                   *resources/list*)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"resources":[{"uri":"channel://inbox"}]}}\n' "$id" ;;
                   *) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
                 esac
               done"#,
        )
        .unwrap_or_else(|e| panic!("应当连上：{e}"));

        // 造一个刚超上限的文件（稀疏写，别真占内存）
        let path = std::env::temp_dir().join(format!(
            "abb-attach-big-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        {
            let f = std::fs::File::create(&path).expect("建临时文件");
            f.set_len((crate::mcp_client::MAX_ATTACHMENT_BYTES + 1) as u64)
                .expect("设长度");
        }
        let meta = crate::attachments::AttachmentMeta {
            kind: "file".into(),
            source: "test".into(),
            file_name: "big.bin".into(),
            mime: "application/octet-stream".into(),
            size: 0,
            path: path.display().to_string(),
            sha256: String::new(),
            note: String::new(),
        };
        let err = m.send_attachment("c", &meta).await.unwrap_err();
        let _ = std::fs::remove_file(&path);
        let msg = err.to_string();
        assert!(
            msg.contains("附件太大"),
            "超限要在发送前拒绝并说清大小：{msg}"
        );
        assert!(msg.contains("MB"), "报因要带单位：{msg}");
    }

    /// **体积闸必须在读文件之前生效**（评审 7f57d7a8 的 non-blocking）。
    ///
    /// ## 判据为什么不用耗时
    ///
    /// 第一版用「耗时应 < 20ms」——**变异逃逸了**：20 MB 稀疏文件读起来太快，
    /// 先读后判也只花十几毫秒。耗时依赖机器速度，不是可靠判据。
    ///
    /// ## 改用「读会阻塞」的路径
    ///
    /// 用 **FIFO**（命名管道）：`metadata().len()` 立即返回 0（不阻塞），
    /// 而 `fs::read` 会**一直阻塞**直到有人写。于是：
    /// - 正确的实现（先看 metadata）：立刻返回「附件太大」或走到 read 才阻塞
    /// - 先读后判的实现在这里会**挂住**
    ///
    /// 但 FIFO 的 metadata 大小是 0，不会触发超限……所以这条路也不通。
    ///
    /// ## 最终判据：源码守卫
    ///
    /// 耗时不可靠、FIFO 触发不了体积闸，那就直接钉**顺序**：
    /// 断言 `reject_if_too_big(meta_len)` 在 `std::fs::read` **之前**出现。
    /// 这是纯静态判据，不受机器快慢影响。
    /// 源码守卫：体积闸必须出现在 `fs::read` **之前**。
    ///
    /// 见上方注释解释为什么不用耗时/不用 FIFO。这条判据是静态的，
    /// 不受机器快慢与文件系统行为影响。
    #[test]
    fn oversized_gate_precedes_file_read_in_source() {
        let src = include_str!("mcp_channel.rs");
        let gate = src
            .find("reject_if_too_big(meta_len)?")
            .expect("找不到读前的体积闸（被删了？）");
        let read = src.find("std::fs::read(path)").expect("找不到 fs::read");
        assert!(
            gate < read,
            "体积闸在 fs::read 之后 ⇒ 超限文件仍会先被整份读进内存（第 {} 行 vs 第 {} 行）",
            src[..gate].lines().count(),
            src[..read].lines().count()
        );
        // 读后也要再判一次（metadata 与实际可能不一致）
        let after = src[read..]
            .find("reject_if_too_big(bytes.len()")
            .expect("找不到读后的体积闸（metadata 可能不可信，读后必须再判）");
        assert!(after < src[read..].len(), "读后那道闸在错误的位置");
    }

    // ── 契约 §3.4 可选能力：三条回落规则 ─────────────────────────────
    //
    // 11 个可选能力共用同一套回落逻辑（`call_optional`），所以这里按
    // **三种情形**覆盖，而不是逐个能力写一遍——那只会重复同一段断言。

    /// 假 server 的脚本：`tools` 是要暴露的工具名列表（空格分隔）。
    ///
    /// 每个可选工具返回**写死的完整 JSON 行**，不用 `sed` 拼转义——
    /// 第一版用了 `printf ... | sed 's/"/\\"/g'` 双层转义，结果 JSON 被拼坏、
    /// 服务连不上（`tools/list` 都失败）。写死虽然重复，但**一眼能看出返回什么**。
    fn fake_server(tools: &str, break_all: bool) -> String {
        let list = tools
            .split_whitespace()
            .map(|t| format!(r#"{{"name":"{t}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let call_arm = if break_all {
            // **只让可选工具失败，`send_text` 保持正常**。
            //
            // 第一版让所有 `tools/call` 都返回 error，于是测试里那句
            // 「失败没有污染 client，仍然能发文本」也失败了——那不是代码的问题，
            // 是我把「可选能力失败」和「主能力失败」混在一起测了。
            r#"*send_text*)
                     B='ok'
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$B" ;;
                   *tools/call*)
                     printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32603,"message":"boom"}}\n' "$id" ;;
            "#
            .to_string()
        } else {
            // **关键：JSON 放在 shell 单引号变量里、用 `%s` 插入。**
            //
            // 第一版把 JSON 直接写进 printf 的格式串，结果 `printf` 把 `\"` 解释成 `"`
            // （吃掉反斜杠），拼出非法 JSON、server 连都不上。`%s` 不做转义处理，
            // 所以把转义好的内容放进变量再插入是可靠的。
            r#"*get_quoted_message*)
                     B='{\"text\":\"引用原文\",\"msg_type\":\"text\"}'
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$B" ;;
                   *download_attachment*)
                     B='{\"kind\":\"image\",\"file_name\":\"a.png\",\"mime\":\"image/png\",\"size\":42,\"path\":\"/tmp/a.png\",\"sha256\":\"ab\"}'
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$B" ;;
                   *create_chat*)
                     B='{\"chat_id\":\"oc_new\"}'
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$B" ;;
                   *user_display_name*)
                     B='{\"name\":\"张三\"}'
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$B" ;;
                   *typing*)
                     B='{\"reaction_id\":\"re_1\"}'
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$B" ;;
                   *chat_brief*)
                     B='{\"chat_type\":\"group\",\"chat_name\":\"研发群\"}'
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$B" ;;
                   *get_chat_info*)
                     B='{\"name\":\"研发群\",\"description\":\"干啥的\"}'
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$B" ;;
                   *tools/call*)
                     B='{}'
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$B" ;;
            "#
            .to_string()
        };
        format!(
            r#"while IFS= read -r line; do
                 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                 case "$line" in
                   *tools/list*)
                     printf '{{"jsonrpc":"2.0","id":%s,"result":{{"tools":[{list}]}}}}\n' "$id" ;;
                   *resources/list*)
                     printf '{{"jsonrpc":"2.0","id":%s,"result":{{"resources":[{{"uri":"channel://inbox"}}]}}}}\n' "$id" ;;
                   {call_arm}
                   *) printf '{{"jsonrpc":"2.0","id":%s,"result":{{}}}}\n' "$id" ;;
                 esac
               done"#
        )
    }

    fn messenger_with(tools: &str, break_all: bool) -> McpChannelMessenger {
        let body = fake_server(tools, break_all);
        McpChannelMessenger::connect("fake", "/bin/sh", &["-c".to_string(), body], &[])
            .unwrap_or_else(|e| panic!("应当连上（tools={tools}）：{e}"))
    }

    /// **情形 1**：服务只声明 `send_text` ⇒ 11 个可选能力全部回落 trait 默认。
    ///
    /// 这条守的是「缺能力不等于坏渠道」：一个只发文本的服务仍然可用，
    /// 不该因为没声明 `create_chat` 就被判不可用。
    #[tokio::test]
    async fn absent_optional_tools_fall_back_to_trait_defaults() {
        let m = messenger_with("send_text", false);

        assert!(
            m.get_quoted_message("mid").await.is_none(),
            "缺工具应返回 None"
        );
        assert!(
            m.download_attachment(
                "k",
                "mid",
                0,
                &crate::attachments::AttachmentDesc::Feishu {
                    message_id: "mid".into(),
                    file_key: "fk".into(),
                    kind: "image".into(),
                    file_name: "a.png".into(),
                }
            )
            .await
            .is_none(),
            "缺工具应返回 None"
        );
        // create_chat 的默认是 **Err**（不是 None）——它要告诉调用方「只能手动建群」。
        let err = m
            .create_chat("群", "描述", "ou_1")
            .await
            .expect_err("缺 create_chat 应报错而不是静默");
        assert!(
            err.contains("未声明") && err.contains("create_chat"),
            "报因要点名缺哪个工具：{err}"
        );
        assert!(m.user_display_name("u1").await.is_none());
        assert!(m.typing("mid").await.is_none());
        assert!(m.chat_brief("c1").await.is_none());
        assert!(m.get_chat_info("c1").await.is_none());

        // 无返回的三个：只要不 panic 就算过（它们本来就不上报）。
        m.del_typing("mid", Some("re".into())).await;
        m.done("mid").await;
        m.note_context("c1", "ctx");
        m.note_sender("c1", "u1");
    }

    /// **情形 2**：服务声明的工具都要被真的调用并正确解析。
    ///
    /// 这条守的是「解析对不对」——11 个能力各有一个字段映射，
    /// 任何一个写错（取错 key、漏 unwrap）都会在这里暴露。
    #[tokio::test]
    async fn present_optional_tools_are_called_and_parsed() {
        let m = messenger_with(
            "send_text get_quoted_message download_attachment create_chat              user_display_name typing del_typing done note_context note_sender              chat_brief get_chat_info",
            false,
        );

        let q = m.get_quoted_message("mid").await.expect("应当拿到引用");
        assert_eq!(q.text, "引用原文");
        assert_eq!(q.msg_type, "text");

        let a = m
            .download_attachment(
                "k",
                "mid",
                0,
                &crate::attachments::AttachmentDesc::Feishu {
                    message_id: "mid".into(),
                    file_key: "fk".into(),
                    kind: "image".into(),
                    file_name: "a.png".into(),
                },
            )
            .await
            .expect("应当拿到附件元数据");
        assert_eq!(a.path, "/tmp/a.png");
        assert_eq!(a.size, 42);
        assert_eq!(a.mime, "image/png");
        assert!(a.note.is_empty(), "正常下载不该有 note：{:?}", a.note);

        assert_eq!(m.create_chat("群", "描述", "ou_1").await.unwrap(), "oc_new");
        assert_eq!(m.user_display_name("u1").await.as_deref(), Some("张三"));
        assert_eq!(m.typing("mid").await.as_deref(), Some("re_1"));
        assert_eq!(
            m.chat_brief("c1").await,
            Some(("group".to_string(), "研发群".to_string()))
        );
        assert_eq!(
            m.get_chat_info("c1").await,
            Some(("研发群".to_string(), "干啥的".to_string()))
        );
    }

    /// **情形 3**：可选能力调用失败 ⇒ `None`，**不阻断主流程**。
    ///
    /// 这条守的是「次要功能坏了不该吞掉消息」：如果 `get_quoted_message` 失败
    /// 就向上报错，一条带引用的消息会被整个丢弃——而它本来完全可以不带引用进 agent。
    #[tokio::test]
    async fn failing_optional_tool_is_swallowed_not_propagated() {
        let m = messenger_with(
            "send_text get_quoted_message create_chat chat_brief",
            true, // 所有 tools/call 都返回 error
        );

        // 全部返回 None / Err，且**不 panic**
        assert!(
            m.get_quoted_message("mid").await.is_none(),
            "调用失败应回落 None"
        );
        assert!(m.chat_brief("c1").await.is_none(), "调用失败应回落 None");
        assert!(
            m.create_chat("群", "", "").await.is_err(),
            "create_chat 失败要报错（它没有 None 可回落）"
        );

        // **关键**：同一个 messenger 仍然能发文本——失败没有污染 client。
        m.send_text("c1", "仍然能发")
            .await
            .expect("发文本不该受影响");
    }

    #[test]
    fn capabilities_are_recorded_from_what_the_server_exposes() {
        let m = spawn_fake(
            r#"while IFS= read -r line; do
                 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                 case "$line" in
                   *tools/list*)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"send_text"},{"name":"send_thread_reply"}]}}\n' "$id" ;;
                   *resources/list*)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"resources":[{"uri":"channel://a/inbox"}]}}\n' "$id" ;;
                   *)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
                 esac
               done"#,
        )
        .unwrap_or_else(|e| panic!("能力齐备应当连上：{e}"));
        assert!(m.capabilities().has_send_text);
        assert!(m.capabilities().has_send_thread_reply);
        assert_eq!(m.capabilities().inbox_uris, vec!["channel://a/inbox"]);
    }

    #[tokio::test]
    async fn send_text_passes_chat_id_and_text() {
        // server 把收到的 arguments 原样回显 ⇒ 断言我们真的传对了字段名。
        let m = spawn_fake(
            r#"while IFS= read -r line; do
                 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                 case "$line" in
                   *tools/list*)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"send_text"}]}}\n' "$id" ;;
                   *resources/list*)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"resources":[{"uri":"channel://inbox"}]}}\n' "$id" ;;
                   *tools/call*)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"echo-ok"}]}}\n' "$id" ;;
                   *)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
                 esac
               done"#,
        )
        .unwrap_or_else(|e| panic!("应当连上：{e}"));
        m.send_text("chat-1", "你好").await.expect("发送应当成功");
    }

    /// 没有 `send_thread_reply` ⇒ 回落 `send_text`，**不能静默丢弃**。
    #[tokio::test]
    async fn thread_reply_falls_back_to_send_text_when_absent() {
        let m = spawn_fake(
            r#"while IFS= read -r line; do
                 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                 case "$line" in
                   *tools/list*)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"send_text"}]}}\n' "$id" ;;
                   *resources/list*)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"resources":[{"uri":"channel://inbox"}]}}\n' "$id" ;;
                   *tools/call*)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"ok"}]}}\n' "$id" ;;
                   *)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
                 esac
               done"#,
        )
        .unwrap_or_else(|e| panic!("应当连上：{e}"));
        assert!(!m.capabilities().has_send_thread_reply);
        m.send_thread_reply("c", "m", "文本")
            .await
            .expect("应回落到 send_text 并成功");
    }

    /// base64 编码正确（标准字母表 + padding）。附件走它内联传输。
    #[test]
    fn base64_encode_is_correct() {
        // 长度 % 3 的三种情形各验一次（padding 数量不同）
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        // 含高位字节（0x80..0xff）也要对，别只测 ASCII
        assert_eq!(base64_encode(&[0xff, 0xfe]), "//4=");
    }

    /// **服务没声明 `send_attachment` ⇒ 明确失败**，并且报因要说清是「没声明工具」，
    /// 不能笼统说"发送失败"（那样会让人以为是网络问题）。
    #[tokio::test]
    async fn send_attachment_without_tool_fails_with_reason() {
        let m = spawn_fake(
            r#"while IFS= read -r line; do
                 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                 case "$line" in
                   *tools/list*)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"send_text"}]}}\n' "$id" ;;
                   *resources/list*)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"resources":[{"uri":"channel://inbox"}]}}\n' "$id" ;;
                   *) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
                 esac
               done"#,
        )
        .unwrap_or_else(|e| panic!("应当连上：{e}"));
        let meta = crate::attachments::AttachmentMeta {
            kind: "file".into(),
            source: "test".into(),
            file_name: "x".into(),
            mime: "application/octet-stream".into(),
            size: 1,
            path: "/tmp/x".into(),
            sha256: String::new(),
            note: String::new(),
        };
        let err = m.send_attachment("c", &meta).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("未声明") && msg.contains("send_attachment"),
            "报因要点名缺哪个工具：{msg}"
        );
    }
}
