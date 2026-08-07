//! 桥 —— 事件过滤 + 粘性后端路由 + per-chat 串行 + 表情生命周期。
//! 通道无关：通过 `Messenger` trait 收发（飞书 / 微信 / 钉钉），飞书事件解析在 on_payload，
//! 微信消息在 on_weixin，钉钉消息在 on_dingtalk。零 regex（路由/@_user_ 手工解析）。

use crate::agent::{self, Backend};
use crate::config::{BotConfig, Config};
use crate::messenger::Messenger;
use crate::schedule::JobStore;
use crate::sessions::SessionStore;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub struct Bridge {
    pub msgr: Arc<dyn Messenger>,
    pub sessions: SessionStore,
    pub jobs: JobStore,
    /// 本 bot 的配置（app_id/bot_name/bot_open_id/primary_chat_id/wx_*…）
    pub bot: BotConfig,
    /// 全局：只响应这个 owner（飞书 open_id）
    pub owner_open_id: String,
    /// 全局：默认后端（SessionStore 已按它初始化；字段保留以便将来逐 bot 覆盖）
    #[allow(dead_code)]
    pub default_backend: String,
    seen: Mutex<HashSet<String>>,
    /// per-chat 串行锁：同一 chat 的并发消息在此**排队**等前一条处理完再跑（而非丢弃）。
    /// 每个 chat_id 一把 tokio 异步锁；锁 Arc 从 std Mutex 的 HashMap 取出后再 await，
    /// 不跨 await 持有 std 锁。
    chat_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// 在跑任务的打断标志：chat_id → AtomicBool。「停止词」到达时置 true 叫停该 chat 正在跑的任务。
    /// （per-chat 同一时刻只有一个在跑任务，故每 chat 至多一个标志。）
    cancel_flags: Mutex<HashMap<String, Arc<std::sync::atomic::AtomicBool>>>,
}

#[derive(Debug)]
pub struct Ev {
    pub mid: String,
    pub chat_id: String,
    /// 会话类型：飞书 p2p/group，微信 dm（主会话候选判定用）
    pub chat_type: String,
    pub text: String,
}

impl Bridge {
    pub fn new(msgr: Arc<dyn Messenger>, bot: BotConfig, cfg: &Config) -> Bridge {
        // 后端跟着 bot 走：用该 bot 的生效后端（自身 backend 非空优先，否则回落全局默认）。
        let effective = bot.effective_backend(&cfg.default_backend).to_string();
        let sessions = SessionStore::new(&effective, &bot.key());
        Bridge {
            msgr,
            sessions,
            jobs: JobStore::new(&bot.key()),
            // owner 也是 per-bot（飞书 bot 各自配，微信用 wx_user_id 不走这）；空则回落全局 owner_open_id。
            owner_open_id: bot.effective_owner(&cfg.owner_open_id).to_string(),
            default_backend: effective,
            bot,
            seen: Mutex::new(HashSet::new()),
            chat_locks: Mutex::new(HashMap::new()),
            cancel_flags: Mutex::new(HashMap::new()),
        }
    }

    /// 取（或新建）某 chat 的串行锁 Arc。同一 chat 的并发任务拿同一把锁 → 排队等前一条处理完。
    fn chat_lock(&self, chat_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.chat_locks.lock().unwrap();
        locks
            .entry(chat_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// 群消息 mentions 里是否 @了本机器人（name/app_id/open_id 三重冗余）。
    fn bot_is_mentioned(&self, mentions: &[serde_json::Value]) -> bool {
        for m in mentions {
            // name 命中
            if m.get("name").and_then(|x| x.as_str()) == Some(self.bot.bot_name.as_str())
                && !self.bot.bot_name.is_empty()
            {
                return true;
            }
            // id.app_id / id.open_id 命中
            if let Some(id) = m.get("id") {
                if id.get("app_id").and_then(|x| x.as_str()) == Some(self.bot.app_id.as_str())
                    && !self.bot.app_id.is_empty()
                {
                    return true;
                }
                if id.get("open_id").and_then(|x| x.as_str()) == Some(self.bot.bot_open_id.as_str())
                    && !self.bot.bot_open_id.is_empty()
                {
                    return true;
                }
            }
            // 顶层 open_id 命中
            if m.get("open_id").and_then(|x| x.as_str()) == Some(self.bot.bot_open_id.as_str())
                && !self.bot.bot_open_id.is_empty()
            {
                return true;
            }
        }
        false
    }

    /// DATA 帧 payload 入口：解析 v2 事件 → 过滤 → handle。
    pub async fn on_payload(&self, payload: &[u8]) {
        let body: serde_json::Value = match serde_json::from_slice(payload) {
            Ok(v) => v,
            Err(_) => return,
        };
        let event_type = body["header"]["event_type"].as_str().unwrap_or("");
        if event_type != "im.message.receive_v1" {
            return; // 只处理消息接收事件，其它（reaction/task…）忽略
        }
        let event = &body["event"];
        let sender = &event["sender"];
        let message = &event["message"];

        // 忽略机器人自己发的（防自我回复死循环）
        if sender["sender_type"].as_str() == Some("bot") {
            return;
        }
        let sender_id = sender["sender_id"]["open_id"].as_str().unwrap_or("");
        let chat_type = message["chat_type"].as_str().unwrap_or("");
        let mentions: Vec<serde_json::Value> =
            message["mentions"].as_array().cloned().unwrap_or_default();

        if chat_type == "group" {
            crate::log!("[群] bot@={}", self.bot_is_mentioned(&mentions));
        }

        // should_respond
        if sender_id != self.owner_open_id {
            return;
        }
        if chat_type == "group" && !self.bot_is_mentioned(&mentions) {
            return;
        }

        // content 是 JSON 字符串，取 .text（失败回退原文）
        let raw = message["content"].as_str().unwrap_or("");
        let text = serde_json::from_str::<serde_json::Value>(raw)
            .ok()
            .and_then(|v| v["text"].as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| raw.to_string());
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }

        let ev = Ev {
            mid: message["message_id"].as_str().unwrap_or("").to_string(),
            chat_id: message["chat_id"].as_str().unwrap_or("").to_string(),
            chat_type: chat_type.to_string(),
            text,
        };
        if ev.mid.is_empty() || ev.chat_id.is_empty() {
            return;
        }
        self.handle(ev).await;
    }

    async fn handle(&self, ev: Ev) {
        // mid 去重
        {
            let mut seen = self.seen.lock().unwrap();
            if seen.contains(&ev.mid) {
                return;
            }
            seen.insert(ev.mid.clone());
            if seen.len() > 5000 {
                let keep: Vec<String> = seen.iter().skip(2500).cloned().collect();
                *seen = keep.into_iter().collect();
            }
        }

        // 剥群聊 @_user_N 提及标签
        let text = strip_mentions(&ev.text).trim().to_string();
        if text.is_empty() {
            crate::log!(
                "[bridge] chat {} 跳过空/非文本消息",
                &ev.chat_id[..ev.chat_id.len().min(10)]
            );
            return;
        }

        // 打断拦截：停止词 → 叫停该 chat 正在跑的任务。必须在拿串行锁**之前**判断，
        // 否则会被排到运行中任务之后，等任务跑完才处理（那时打断就没意义了）。
        if is_cancel_keyword(&text) {
            if let Some(flag) = self.cancel_flags.lock().unwrap().get(&ev.chat_id).cloned() {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
                crate::log!(
                    "[bridge] 收到停止指令 chat={}",
                    &ev.chat_id[..ev.chat_id.len().min(10)]
                );
                // 「⏹ 已停止」由被叫停的任务自己发（它确认真停了才发）；这里不回话避免重复。
                return;
            }
            // 无在跑任务 → 停止词当普通消息透传给 agent
        }

        // 记录本 bot 主会话（私聊）：定时任务会话失效时的回落目标 + job CLI 缺省回发处
        // 飞书私聊 chat_type="p2p"；微信私聊用 "dm"
        if ev.chat_type == "p2p" || ev.chat_type == "dm" {
            crate::config::Config::save_primary_chat(&self.bot.key(), &ev.chat_id);
        }

        // 后端只认 per-bot 配置（app 里改），聊天里不再有 /codex /claude 切换——
        // 斜杠前缀原样透传给 agent（claude/codex 有自己的 slash 命令，不该被桥拦截）。
        let backend = Backend::parse(self.bot.effective_backend(&self.default_backend));
        let prompt = text;

        // per-chat 串行：同一 chat 的并发消息排队等前一条处理完（不丢弃）。
        // 先从 std Mutex 取出该 chat 的锁 Arc（短持 std 锁），再 await 异步锁。
        let chat_lock = self.chat_lock(&ev.chat_id);
        let _serial_guard = chat_lock.lock().await;

        // 会话快照必须在**拿到锁之后**取：锁外取的话，首轮 agent 还在跑时到达的第二条消息
        // 会读到过期的 started=false —— claude 侧对同一 UUID 再 --session-id 报「already in use」，
        // codex 侧新建 thread 覆盖掉首轮的 → 首轮上下文永久丢失。锁内取则前一轮必已 mark_started。
        let session_id = self.sessions.ensure_session(&ev.chat_id);
        let resume = self.sessions.is_started(&ev.chat_id);

        let typing_rid = self.msgr.typing(&ev.mid).await;

        // 流式执行：agent 边跑边把中途完整消息推进 progress 通道，这里即时转发到聊天（不等跑完）；
        // cancel flag 注册进 cancel_flags，供该 chat 后续「停止词」消息叫停。
        let (ptx, mut prx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.cancel_flags
            .lock()
            .unwrap()
            .insert(ev.chat_id.clone(), cancel_flag.clone());

        let bot_key = self.bot.key();
        let run_fut = agent::run(
            backend,
            &prompt,
            &session_id,
            resume,
            &ev.chat_id,
            &bot_key,
            Some(&self.sessions),
            Some(ptx),
            Some(cancel_flag.clone()),
        );
        tokio::pin!(run_fut);
        let result = loop {
            tokio::select! {
                Some(p) = prx.recv() => {
                    if let Err(e) = self.msgr.send_text(&ev.chat_id, &p).await {
                        crate::log!(
                            "[bridge] ⚠️ 中途进度发送失败 chat={}: {e:#}",
                            &ev.chat_id[..ev.chat_id.len().min(10)]
                        );
                    }
                }
                r = &mut run_fut => { break r; }
            }
        };
        // 任务结束 → 摘掉打断标志（后续停止词将按普通消息处理）
        self.cancel_flags.lock().unwrap().remove(&ev.chat_id);

        match result {
            Ok(agent::RunOutcome::Reply(reply)) => {
                // agent 成功即标记 started（会话状态只跟 agent 跑没跑成有关，与投递无关）
                self.sessions.mark_started(&ev.chat_id);
                // 发送结果必须留痕：回复丢了（token 失效/会话失效等）时不能谎报成功。
                match self.msgr.send_text(&ev.chat_id, &reply).await {
                    Ok(()) => crate::log!(
                        "[bridge] 已回复 chat={} 长度={}",
                        &ev.chat_id[..ev.chat_id.len().min(10)],
                        reply.chars().count()
                    ),
                    Err(e) => crate::log!(
                        "[bridge] ⚠️ 回复发送失败 chat={}: {e:#}",
                        &ev.chat_id[..ev.chat_id.len().min(10)]
                    ),
                }
            }
            Ok(agent::RunOutcome::Cancelled) => {
                crate::log!(
                    "[bridge] 任务被打断 chat={}",
                    &ev.chat_id[..ev.chat_id.len().min(10)]
                );
                let _ = self.msgr.send_text(&ev.chat_id, "⏹ 已停止").await;
                // 不 mark_started：被打断的轮次不算完成
            }
            Err(e) => {
                // 错误文案作为回复发出（用户可见原因），同样留痕
                match self.msgr.send_text(&ev.chat_id, &e).await {
                    Ok(()) => crate::log!(
                        "[bridge] 已回复错误 chat={} 长度={}",
                        &ev.chat_id[..ev.chat_id.len().min(10)],
                        e.chars().count()
                    ),
                    Err(se) => crate::log!(
                        "[bridge] ⚠️ 错误回复发送失败 chat={}: {se:#}",
                        &ev.chat_id[..ev.chat_id.len().min(10)]
                    ),
                }
            }
        }

        self.msgr.del_typing(&ev.mid, typing_rid).await;
        self.msgr.done(&ev.mid).await;
        // _serial_guard 在此函数末尾 drop，释放 per-chat 锁，排队的下一条开始处理。
    }

    /// 微信入站消息入口（service 的微信长轮询循环调用）。
    /// msg=入站微信消息；先记 context_token，过滤后走统一 handle。
    pub async fn on_weixin(&self, msg: crate::wechat::WeixinMessage) {
        // 只处理用户消息（message_type==1 是 USER；2 是 BOT 自己）
        if msg.message_type != 1 {
            return;
        }
        let from = msg.from_user_id.trim().to_string();
        if from.is_empty() {
            return;
        }
        // should_respond：微信侧 owner 判据是登录拿到的 ilink_user_id（不是飞书 open_id）
        let owner = self.bot.wx_owner();
        if !owner.is_empty() && from != owner {
            crate::log!(
                "[weixin] 忽略非 owner 消息 from={}",
                &from[..from.len().min(10)]
            );
            return;
        }
        // 回复必须回显该用户最新 context_token
        self.msgr.note_context(&from, &msg.context_token);
        let text = msg.text().trim().to_string();
        if text.is_empty() {
            crate::log!("[weixin] 丢弃：text 为空（非文本消息）");
            return; // 非文本（图片/语音/文件暂未实现）
        }
        let mid = if msg.message_id.is_empty() {
            // 微信 message_id 可能为空；用 session_id+时间戳凑一个去重键
            format!("{}:{}", msg.session_id, msg.create_time_ms)
        } else {
            msg.message_id.clone()
        };
        let ev = Ev {
            mid,
            chat_id: from,               // 微信会话标识 = 对方 ilink_user_id
            chat_type: "dm".to_string(), // 微信私聊当 dm（主会话候选）
            text,
        };
        self.handle(ev).await;
    }

    /// 钉钉入站消息入口（service 的钉钉 Stream 循环调用）。
    /// msg=解析好的机器人消息；先记群聊最近发送者（回复时 @），过滤后走统一 handle。
    pub async fn on_dingtalk(&self, msg: crate::dingtalk::DingtalkMessage) {
        // should_respond：钉钉 owner 判据是允许的 staffId（ding_user_id；空=不设限）
        let owner = self.bot.ding_owner();
        if !owner.is_empty() && msg.sender_staff_id != owner {
            crate::log!(
                "[dingtalk] 忽略非 owner 消息 from={}",
                &msg.sender_staff_id[..msg.sender_staff_id.len().min(10)]
            );
            return;
        }
        // 群聊只有 @ 了本机器人（或配置了「@ 才推送」）的消息才处理；单聊直接处理
        if msg.is_group() && !msg.mentioned {
            crate::log!(
                "[dingtalk] 忽略群聊未 @ 机器人的消息 chat={}",
                &msg.chat_id()[..msg.chat_id().len().min(10)]
            );
            return;
        }
        let chat_id = msg.chat_id();
        if chat_id.is_empty() || msg.mid.is_empty() {
            return;
        }
        // 群聊回复需要 @ 提问者 → 记最近 sender（单聊 chat_id==sender，无意义但无害）
        self.msgr.note_sender(&chat_id, &msg.sender_staff_id);

        // 剥群聊文本里的 "@机器人名" 前缀（钉钉推给机器人的内容会带上），只剥一次
        let is_group = msg.is_group();
        let mut text = msg.text;
        if is_group {
            text = strip_bot_mention(&text, &self.bot.bot_name);
        }
        let ev = Ev {
            mid: msg.mid,
            chat_id,
            chat_type: if is_group { "group".to_string() } else { "dm".to_string() },
            text,
        };
        self.handle(ev).await;
    }
}

/// 剥掉 "@_user_<数字>" 提及标签（等价 Python 的 re.sub(r"@_user_\d+", "", ...)）。
/// 注意：只有后面跟了至少一个数字才算提及标签；`@_user_`（无数字）原样保留（与 \d+ 一致）。
pub fn strip_mentions(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if s[i..].starts_with("@_user_") {
            let mut j = i + "@_user_".len();
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + "@_user_".len() {
                i = j; // 后面确有数字：跳过整个 @_user_N
                continue;
            }
            // 无数字：不是有效提及，原样保留 "@_user_"
            out.push_str("@_user_");
            i += "@_user_".len();
        } else {
            // 按 char 推进（UTF-8 安全）
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// 剥掉钉钉群聊文本开头的 "@机器人名"（钉钉推给机器人的群消息 content 会带 @ 前缀；
/// 机器人名未知/没匹配到则原样返回）。只剥一次，避免误伤正文里的同名 @。
pub fn strip_bot_mention(text: &str, bot_name: &str) -> String {
    if bot_name.is_empty() {
        return text.to_string();
    }
    let t = text.trim_start();
    let mention = format!("@{bot_name}");
    if let Some(rest) = t.strip_prefix(mention.as_str()) {
        // 剥掉后面可能跟的空格（钉钉常见 " @机器人名 内容"）
        return rest.trim_start().to_string();
    }
    text.to_string()
}

/// 识别「打断」关键词（整句精确匹配，大小写不敏感）。仅当该 chat 有任务在跑时才生效
/// （由 handle 判断）；否则原样透传给 agent，避免误吞用户正常词汇。
fn is_cancel_keyword(text: &str) -> bool {
    const KEYWORDS: &[&str] = &["停", "停止", "停下", "取消", "stop", "cancel", "/stop", "/cancel"];
    let t = text.trim().to_ascii_lowercase();
    KEYWORDS.iter().any(|k| *k == t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_user_mentions() {
        assert_eq!(strip_mentions("@_user_1 你好"), " 你好");
        assert_eq!(strip_mentions("@_user_123@_user_456 hi"), " hi");
        assert_eq!(strip_mentions("没有提及"), "没有提及");
        assert_eq!(strip_mentions("@_user_ 不完整"), "@_user_ 不完整");
    }

    #[test]
    fn strip_bot_mention_works() {
        assert_eq!(strip_bot_mention("@庆小丰 你好", "庆小丰"), "你好");
        assert_eq!(strip_bot_mention("  @庆小丰  你好", "庆小丰"), "你好");
        // 名字未知/没匹配 → 原样
        assert_eq!(strip_bot_mention("@别人 你好", "庆小丰"), "@别人 你好");
        assert_eq!(strip_bot_mention("@庆小丰 你好", ""), "@庆小丰 你好");
        // 正文中间的同名 @ 不剥
        assert_eq!(strip_bot_mention("你好 @庆小丰", "庆小丰"), "你好 @庆小丰");
    }

    #[test]
    fn cancel_keywords_match() {
        for k in ["停", "停止", "取消", "stop", "Stop", "STOP", "/stop", "cancel", "/cancel", " 停 "] {
            assert!(is_cancel_keyword(k), "应为停止词: {k:?}");
        }
        for k in ["停下来聊聊", "stop it", "别停", "/stopit", "取消订阅这个服务", ""] {
            assert!(!is_cancel_keyword(k), "不应为停止词: {k:?}");
        }
    }
}
