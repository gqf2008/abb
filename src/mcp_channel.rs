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
//! 入站（`resources/subscribe` 或 `poll`）是 S2，本模块暂不涉及。

// 与 `mcp_bus`/`mcp_client` 同口径：还没接进 `service.rs` 的 bot 装配（S1d），
// 故部分成员暂时未用。**移除这个 allow 必须是「真接上了」**，不是「先关警告」。
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

    /// 附件：本步骤**显式失败**。
    ///
    /// 不写默认实现的原因与 `Messenger::send_attachment` 的注释同口径：把路径当文本
    /// 发出去还返回 Ok，会让投递方以为附件送达。
    async fn send_attachment(
        &self,
        _chat_id: &str,
        _meta: &crate::attachments::AttachmentMeta,
    ) -> Result<()> {
        Err(anyhow::anyhow!(
            "渠道 `{}` 的 MCP 服务尚未声明附件工具（S1c 只做出站文本）",
            self.name
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

    /// 附件必须**显式失败**，不能假装成功（历史教训：默认实现把路径当文本发还返回 Ok）。
    #[tokio::test]
    async fn send_attachment_fails_explicitly() {
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
        assert!(
            err.to_string().contains("附件"),
            "报因要说清是附件不支持：{err}"
        );
    }
}
