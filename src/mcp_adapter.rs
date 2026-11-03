//! 渠道入站 → `Bridge` 的适配层（总线 S2c）。
//!
//! ## 为什么需要这一层
//!
//! `Bridge` 的入站入口是 `handle(ev: Ev)`——`Ev` 是**渠道无关**的内部结构
//! （mid/chat_id/text/attachments/role/ts…），三个内置渠道各自把平台事件翻译成它。
//! MCP 渠道要做的是**同一件事**：把契约 §3.3 的 [`ChannelMessage`] 翻译成 `Ev`。
//!
//! 所以这里**不**去碰 `Bridge::on_payload`（那个入口吃飞书事件 JSON，
//! 硬塞 MCP 消息就得伪造飞书 JSON，等于把渠道差异重新引进 ABB）。
//!
//! ## 三个翻译上必须小心的地方
//!
//! 1. **`role` 不能照抄 `sender_id`**。`Ev::role` 决定 agent 跑全权限还是受限档
//!    （`SenderRole::Owner` / `Granted`）。MCP 渠道没有平台侧 owner 概念，
//!    所以**默认一律 `Granted`（受限）**——这是 fail-closed：搭错配置最多是能力受限，
//!    不会凭空给出全权限。要放宽必须在配置里显式声明 owner（`mcp_channel.owner_ids`），
//!    不是靠消息猜。本函数**只认白名单里的 `sender_id`**；空 `sender_id` 永不算 owner
//!    （哪怕名单里真写了一堆空串）。
//!
//! 2. **`chat_type` 不能瞎填**。它参与「主会话候选」判定（`p2p` 才算）。MCP 渠道
//!    没有 p2p/group 概念，填 `"mcp"` 而不是猜 `"p2p"`——猜 p2p 会让它被当成
//!    owner 私聊而参与主会话选举。
//!
//! 3. **附件只带定位信息**。`ChannelAttachment::url_or_path` 可能是 URL，
//!    而 `AttachmentMeta` 要求本地路径。S2c 阶段**不假装下载成功**：
//!    带 URL 的附件如实标成 `note` 已说明未下载，而不是编一个路径。

// 本步骤（S2c adapter）把翻译逻辑做成可单测的纯函数；**接进 `service::run_bot`
// 的循环（真 client → negotiate → 循环 → Bridge）是 S2d**。
// 与 `mcp_bus`/`mcp_inbound` 同口径：**移除必须是「真接上了」**，不是「先关警告」。
use crate::bridge::Ev;
use crate::config::SenderRole;
use crate::mcp_bus::ChannelMessage;

/// 翻译结果：`Ev` 或「这条不能进桥」的原因。
///
/// 返回 `Result` 而不是 `Option`：进不去桥的消息要能说清为什么
/// （日志里「收到了但没处理」比「什么都没有」难查得多）。
///
/// `owner_ids` 是**该 bot 的** `mcp_channel.owner_ids` 快照（`Bridge::bot` 里的那份）：
/// 名单只影响 `role`，不影响其它字段的翻译。传 `&[]` = 全部受限。
pub fn to_ev(msg: &ChannelMessage, owner_ids: &[String]) -> Result<Ev, String> {
    if msg.chat_id.trim().is_empty() {
        return Err("缺 chat_id".into());
    }
    if msg.message_id.trim().is_empty() {
        return Err("缺 message_id".into());
    }
    if msg.text.trim().is_empty() && msg.attachments.is_empty() {
        // 空消息不该进桥：它会占一轮 agent 交互却没有任何输入。
        // 注意「纯附件消息」是合法的（text 空但 attachments 非空），所以两个都要判。
        return Err("正文与附件都为空".into());
    }

    Ok(Ev {
        mid: msg.message_id.clone(),
        chat_id: msg.chat_id.clone(),
        // 见模块注释 2：不猜 p2p，填 "mcp"。
        chat_type: "mcp".to_string(),
        // 会话名留空：MCP 渠道没有「群名」概念，契约里也没有这个字段。
        // 不要拿 chat_id 顶替——那会让历史列表里显示一串 id 而不是空白。
        chat_name: String::new(),
        // MCP 渠道没有「话题」概念（契约 §3.3 无 thread 字段）⇒ 恒空。
        thread_id: String::new(),
        quoted: crate::messenger::QuotedContent::default(),
        text: msg.text.clone(),
        attachments: msg
            .attachments
            .iter()
            .enumerate()
            .map(|(i, a)| attachment_meta(msg, a, i))
            .collect(),
        // 见模块注释 1：默认受限，只有显式名单里的 sender_id 才是 owner。
        // 三条判据的顺序不能换：
        // - 空 sender_id 先挡：名单里就算有 `""` 也不能因此给全权限（否则
        //   「忘了带 sender_id」的渠道会静默全开）；
        // - 逐字节精确比较（不 `to_lowercase`、不 trim）：名单是运维手写的 id，
        //   做任何归一化都会把不相干的 id 认成 owner。
        role: if msg.sender_id.is_empty() {
            SenderRole::Granted
        } else if owner_ids.iter().any(|o| o == &msg.sender_id) {
            SenderRole::Owner
        } else {
            SenderRole::Granted
        },
        sender_id: msg.sender_id.clone(),
        ts: if msg.ts > 0 {
            msg.ts
        } else {
            // 契约允许 ts=0（未提供）。填当前时间而不是留 0：
            // 历史列表按 ts 排序，留 0 会让这类消息永远排在最前。
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        },
    })
}

/// 一条入站附件 → `AttachmentMeta`。
///
/// **S2c 不下载**：`url_or_path` 是 URL 时如实记成「未下载」，
/// 不编造本地路径（编造会让 agent 去读一个不存在的文件，且不报错）。
fn attachment_meta(
    msg: &ChannelMessage,
    a: &crate::mcp_bus::ChannelAttachment,
    idx: usize,
) -> crate::attachments::AttachmentMeta {
    let raw = a.url_or_path.trim();
    let is_local = !raw.is_empty() && !raw.starts_with("http://") && !raw.starts_with("https://");
    // 文件名：渠道给了就用，没给按 kind 编一个**带序号**的（避免同名覆盖）。
    let name = if a.name.trim().is_empty() {
        let kind = if a.kind.trim().is_empty() {
            "file"
        } else {
            a.kind.trim()
        };
        format!("{kind}-{}-{idx}", msg.message_id)
    } else {
        a.name.clone()
    };
    crate::attachments::AttachmentMeta {
        kind: if a.kind.trim().is_empty() {
            "file".to_string()
        } else {
            a.kind.clone()
        },
        source: "mcp".to_string(),
        file_name: name,
        mime: String::new(),
        size: 0,
        // 只有确实是本地路径时才写进 `path`；URL 留空并写进 note。
        path: if is_local {
            raw.to_string()
        } else {
            String::new()
        },
        sha256: String::new(),
        note: if raw.is_empty() {
            "渠道未提供附件位置".to_string()
        } else if is_local {
            String::new()
        } else {
            // 说清「有这个东西但没下载」，而不是让下游以为附件就在那儿。
            format!("渠道给了 URL，S2c 尚未实现下载：{raw}")
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_bus::ChannelAttachment;

    fn msg(text: &str) -> ChannelMessage {
        ChannelMessage {
            chat_id: "chat-1".into(),
            message_id: "mid-1".into(),
            sender_id: "user-1".into(),
            sender_name: String::new(),
            text: text.into(),
            ts: 1793600000,
            attachments: vec![],
        }
    }

    #[test]
    fn maps_core_fields() {
        let ev = to_ev(&msg("你好"), &[]).expect("应当能翻译");
        assert_eq!(ev.mid, "mid-1");
        assert_eq!(ev.chat_id, "chat-1");
        assert_eq!(ev.text, "你好");
        assert_eq!(ev.sender_id, "user-1");
        assert_eq!(ev.ts, 1793600000);
    }

    /// **fail-closed**：MCP 渠道没有平台 owner 概念，一律给受限档。
    /// 若这里给 Owner，等于「接一个渠道就自动给全权限」。
    #[test]
    fn role_is_granted_not_owner_by_default() {
        let ev = to_ev(&msg("hi"), &[]).unwrap();
        assert_eq!(
            ev.role,
            SenderRole::Granted,
            "MCP 渠道默认必须是受限档（fail-closed）"
        );
    }

    /// 验收 1：**空名单 ⇒ 任何 sender_id 都是 Granted**。
    ///
    /// 这条是整套 owner 声明的安全底线：没配置（或配成空数组）时不得因为
    /// 「渠道自报了 sender_id」而放宽档位。
    #[test]
    fn empty_owner_list_keeps_everyone_granted() {
        let owners: Vec<String> = Vec::new();
        for sender in ["alice", "bob", "", "OWNER", "user-1"] {
            let mut m = msg("hi");
            m.sender_id = sender.into();
            let ev = to_ev(&m, &owners).unwrap();
            assert_eq!(
                ev.role,
                SenderRole::Granted,
                "空名单下 sender_id={sender:?} 不得是 owner"
            );
        }
    }

    /// 验收 2：名单含 `"alice"` ⇒ alice 是 Owner，bob 仍是 Granted。
    ///
    /// 两侧都要断言：只证「alice 是 Owner」不够——把实现写成「非空名单全给 Owner」
    /// 也能让那一半通过。
    #[test]
    fn listed_sender_is_owner() {
        let owners = vec!["alice".to_string()];
        let mut alice = msg("hi");
        alice.sender_id = "alice".into();
        assert_eq!(
            to_ev(&alice, &owners).unwrap().role,
            SenderRole::Owner,
            "名单里的 sender_id 应是 owner"
        );

        let mut bob = msg("hi");
        bob.sender_id = "bob".into();
        assert_eq!(
            to_ev(&bob, &owners).unwrap().role,
            SenderRole::Granted,
            "名单外的 sender_id 必须保持受限"
        );
    }

    /// 验收 3：**大小写敏感**。名单含 `"Alice"` ⇒ `"alice"` 不是 Owner。
    ///
    /// 做 `to_lowercase` 归一化会把不相干的 id 认成 owner（渠道的 sender_id 是
    /// 对方自报字符串，没有任何平台侧归一化保证）。
    #[test]
    fn owner_match_is_case_sensitive() {
        let owners = vec!["Alice".to_string()];
        let mut lower = msg("hi");
        lower.sender_id = "alice".into();
        assert_eq!(
            to_ev(&lower, &owners).unwrap().role,
            SenderRole::Granted,
            "大小写不同不得认成同一个 owner"
        );
        // 同一名单下逐字节相等的那一个仍然是 owner（反向断言，防止实现恒 Granted）。
        let mut exact = msg("hi");
        exact.sender_id = "Alice".into();
        assert_eq!(to_ev(&exact, &owners).unwrap().role, SenderRole::Owner);
    }

    /// 验收 4：**空 sender_id 永不算 owner**——哪怕名单里写了空串。
    ///
    /// 「忘了带 sender_id」的渠道若因此拿到全权限，就是静默全开。
    #[test]
    fn empty_sender_id_is_never_owner() {
        let owners = vec![String::new()];
        let mut m = msg("hi");
        m.sender_id = String::new();
        assert_eq!(
            to_ev(&m, &owners).unwrap().role,
            SenderRole::Granted,
            "空 sender_id 不得因名单里有空串而变 owner"
        );
    }

    /// `chat_type` 不能填 p2p：那会让它参与「主会话候选」选举。
    #[test]
    fn chat_type_is_not_p2p() {
        let ev = to_ev(&msg("hi"), &[]).unwrap();
        assert_ne!(ev.chat_type, "p2p", "填 p2p 会被当成 owner 私聊");
        assert_eq!(ev.chat_type, "mcp");
    }

    #[test]
    fn empty_both_text_and_attachments_is_rejected_with_reason() {
        let mut m = msg("");
        m.text = "   ".into();
        let err = to_ev(&m, &[]).unwrap_err();
        assert!(err.contains("空"), "报因要说清是空消息：{err}");
    }

    /// 纯附件消息是合法的（text 空但 attachments 非空）。
    #[test]
    fn attachment_only_message_is_accepted() {
        let mut m = msg("");
        m.attachments = vec![ChannelAttachment {
            kind: "image".into(),
            name: "a.png".into(),
            url_or_path: "/tmp/a.png".into(),
        }];
        let ev = to_ev(&m, &[]).expect("纯附件消息应当接受");
        assert_eq!(ev.attachments.len(), 1);
    }

    #[test]
    fn missing_ids_are_rejected_with_field_name() {
        let mut m = msg("x");
        m.chat_id = "  ".into();
        assert!(to_ev(&m, &[]).unwrap_err().contains("chat_id"));
        let mut m = msg("x");
        m.message_id = String::new();
        assert!(to_ev(&m, &[]).unwrap_err().contains("message_id"));
    }

    /// ts=0（契约允许）要换成当前时间，否则历史列表里永远排最前。
    #[test]
    fn zero_ts_is_replaced_with_now() {
        let mut m = msg("x");
        m.ts = 0;
        let ev = to_ev(&m, &[]).unwrap();
        assert!(ev.ts > 1_600_000_000, "ts=0 应换成当前时间，实际 {}", ev.ts);
    }

    /// **本地路径照原样进 `path`**。
    #[test]
    fn local_attachment_path_is_kept() {
        let mut m = msg("x");
        m.attachments = vec![ChannelAttachment {
            kind: "file".into(),
            name: "报表.csv".into(),
            url_or_path: "/tmp/报表.csv".into(),
        }];
        let ev = to_ev(&m, &[]).unwrap();
        assert_eq!(ev.attachments[0].path, "/tmp/报表.csv");
        assert!(ev.attachments[0].note.is_empty(), "本地文件不该有 note");
    }

    /// **URL 不能编造成本地路径**：那会让 agent 去读一个不存在的文件且不报错。
    #[test]
    fn url_attachment_is_not_faked_as_local_path() {
        let mut m = msg("x");
        m.attachments = vec![ChannelAttachment {
            kind: "image".into(),
            name: "pic.jpg".into(),
            url_or_path: "https://example.com/pic.jpg".into(),
        }];
        let ev = to_ev(&m, &[]).unwrap();
        assert_eq!(ev.attachments[0].path, "", "URL 不该写进 path");
        assert!(
            ev.attachments[0].note.contains("尚未实现下载"),
            "note 要说明未下载：{:?}",
            ev.attachments[0].note
        );
    }

    /// 渠道没给文件名时按 kind + message_id + 序号编，**不能互相覆盖**。
    #[test]
    fn unnamed_attachments_get_distinct_names() {
        let mut m = msg("x");
        m.attachments = vec![
            ChannelAttachment {
                kind: "image".into(),
                name: String::new(),
                url_or_path: "/tmp/a".into(),
            },
            ChannelAttachment {
                kind: "image".into(),
                name: String::new(),
                url_or_path: "/tmp/b".into(),
            },
        ];
        let ev = to_ev(&m, &[]).unwrap();
        assert_ne!(
            ev.attachments[0].file_name, ev.attachments[1].file_name,
            "同名会互相覆盖"
        );
    }

    /// `chat_name` 留空而不是拿 chat_id 顶替——否则历史列表显示一串 id。
    #[test]
    fn chat_name_is_empty_not_chat_id() {
        let ev = to_ev(&msg("hi"), &[]).unwrap();
        assert!(ev.chat_name.is_empty());
    }
}
