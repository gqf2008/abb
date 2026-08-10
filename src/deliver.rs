//! 跨会话投递（#21）—— agent 通过 `$ABB_BIN deliver` CLI 把消息投递到其它 bot 的会话。
//!
//! 进程模型：agent 是 bridge spawn 的子进程，拿不到 service 内存里的 Messenger；
//! 因此 CLI 把投递请求落盘到 `~/.agent-bridge/deliveries.json`，service 侧轮询消费
//! （mtime 热重载，与 JobStore 同一模式，避免 service 每次读盘）。投递目标 = bot_key + chat_id。
//!
//! 总开关：`Config.cross_delivery_enabled`（默认关）。CLI 提交前拒绝 + service 消费侧双保险，
//! 防止开关在提交后被关掉仍把队列里的项发出去。

use crate::config::BotConfig;
use crate::messenger::Messenger;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// 一条待投递消息。来源（source_bot/source_chat）用于投递失败时回源报错，不静默丢失。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryItem {
    /// 幂等键（uuid；同 id 不重复入队）。
    pub id: String,
    /// 目标 bot key（config bots[].key()）。
    pub target_bot: String,
    /// 目标会话 id（chat_id）。
    pub target_chat: String,
    /// 完整消息文本。
    pub text: String,
    /// 来源 bot key（agent 工作区所在 bot；人工 CLI 可为空）。
    #[serde(default)]
    pub source_bot: String,
    /// 来源会话 id（投递失败时回源报错用）。
    #[serde(default)]
    pub source_chat: String,
    #[serde(default)]
    pub created_at: u64,
}

/// 投递请求落盘队列（CLI 写、service 消费）。
pub struct DeliveryStore {
    path: PathBuf,
    data: Mutex<Vec<DeliveryItem>>,
    /// 上次加载时的文件 mtime。CLI（codex）在另一进程写 deliveries.json，service 内存里这份
    /// 不会自动看到 → 取队列前按 mtime 热重载，避免漏投（对齐 JobStore）。
    loaded_mtime: Mutex<Option<SystemTime>>,
}

impl DeliveryStore {
    pub fn new() -> DeliveryStore {
        DeliveryStore::new_at(crate::bridge_dir().join("deliveries.json"))
    }

    fn new_at(path: PathBuf) -> DeliveryStore {
        let data = fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let mtime = fs::metadata(&path).ok().and_then(|m| m.modified().ok());
        DeliveryStore {
            path,
            data: Mutex::new(data),
            loaded_mtime: Mutex::new(mtime),
        }
    }

    /// 若 deliveries.json 的 mtime 比上次加载新（CLI 在别的进程改了），重新读盘。
    fn refresh(&self) {
        let cur = fs::metadata(&self.path)
            .ok()
            .and_then(|m| m.modified().ok());
        let stale = { *self.loaded_mtime.lock().unwrap() != cur };
        if !stale {
            return;
        }
        if let Ok(text) = fs::read_to_string(&self.path) {
            if let Ok(data) = serde_json::from_str::<Vec<DeliveryItem>>(&text) {
                *self.data.lock().unwrap() = data;
            }
        }
        *self.loaded_mtime.lock().unwrap() = cur;
    }

    fn persist_locked(&self, data: &[DeliveryItem]) {
        if let Ok(text) = serde_json::to_string_pretty(data) {
            let _ = crate::atomic_write_text(&self.path, &text);
        }
    }

    /// 入队（幂等：同 id 已存在则跳过）。
    pub fn add(&self, item: DeliveryItem) {
        let mut d = self.data.lock().unwrap();
        if d.iter().any(|x| x.id == item.id) {
            return;
        }
        d.push(item);
        self.persist_locked(&d);
    }

    /// 取出全部待投递项并清空（service 消费循环用；失败项不回盘——回盘会死循环重投，
    /// 失败已回源报错 + 微信目标落 outbox，不静默丢失）。
    pub fn take_all(&self) -> Vec<DeliveryItem> {
        self.refresh();
        let mut d = self.data.lock().unwrap();
        let taken = std::mem::take(&mut *d);
        self.persist_locked(&d);
        taken
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.refresh();
        self.data.lock().unwrap().len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 跨会话投递路由：service 启动时按所有已启用 bot 构建，投递循环持引用消费队列。
pub struct Router {
    /// 总开关（Config.cross_delivery_enabled）。false 时任何投递直接拒绝（不发出）。
    pub enabled: bool,
    /// bot_key → Messenger（与各 Bridge 共享同一实例）。
    pub messengers: HashMap<String, Arc<dyn Messenger>>,
    /// bot_key → 配置（判断微信通道走 outbox 兜底；供日志/回源提示）。
    pub bots: HashMap<String, BotConfig>,
    /// outbox 落盘目录覆盖（测试注入用；生产 None = 按目标 bot 工作区默认路径）。
    pub outbox_dir: Option<PathBuf>,
}

impl Router {
    /// 投递一条消息。成功只留日志；失败：微信目标落其 outbox 等下次入站补发，
    /// 同时回源 bot/会话报错（best-effort，不静默丢失）。未知目标/开关关闭也回源提示。
    pub async fn deliver(&self, item: &DeliveryItem) {
        if !self.enabled {
            crate::log!(
                "[deliver] 跳过投递：跨会话投递未开启（bot={} chat={} id={}）",
                &item.target_bot[..item.target_bot.len().min(16)],
                &item.target_chat[..item.target_chat.len().min(16)],
                &item.id[..item.id.len().min(8)]
            );
            self.notify_source(
                item,
                "⚠️ 跨会话投递未开启：请在 ABB 设置里打开「跨会话投递」开关后重试。",
            )
            .await;
            return;
        }
        let Some(msgr) = self.messengers.get(&item.target_bot) else {
            crate::log!(
                "[deliver] 跳过投递：目标 bot 不存在或未启用（{}）",
                item.target_bot
            );
            self.notify_source(
                item,
                &format!(
                    "⚠️ 跨会话投递失败：目标 bot「{}」不存在或未启用。",
                    item.target_bot
                ),
            )
            .await;
            return;
        };
        match msgr.send_text(&item.target_chat, &item.text).await {
            Ok(()) => crate::log!(
                "[deliver] 已投递 bot={} chat={} id={} 长度={}",
                item.target_bot,
                &item.target_chat[..item.target_chat.len().min(16)],
                &item.id[..item.id.len().min(8)],
                item.text.chars().count()
            ),
            Err(e) => {
                crate::log!(
                    "[deliver] 投递失败 bot={} chat={} id={}: {e:#}",
                    item.target_bot,
                    &item.target_chat[..item.target_chat.len().min(16)],
                    &item.id[..item.id.len().min(8)]
                );
                // 微信目标：主动推送受 context_token 活跃度约束（#9），落目标 bot outbox，
                // 等目标 bot 下次入站刷新 token 后补发——与定时任务报告同一兜底路径。
                if self
                    .bots
                    .get(&item.target_bot)
                    .map(|b| b.is_wechat())
                    .unwrap_or(false)
                {
                    let store = match &self.outbox_dir {
                        Some(dir) => {
                            crate::outbox::OutboxStore::new_at(dir.join("pending_outbox.json"))
                        }
                        None => crate::outbox::OutboxStore::new(&item.target_bot),
                    };
                    store.add(crate::outbox::OutboxItem {
                        id: item.id.clone(),
                        chat_id: item.target_chat.clone(),
                        text: item.text.clone(),
                        created_at: item.created_at,
                        attempts: 0,
                        job_id: String::new(),
                    });
                    crate::log!(
                        "[deliver] 已落 bot={} 的 outbox 等待入站补发",
                        item.target_bot
                    );
                }
                self.notify_source(
                    item,
                    &format!("⚠️ 跨会话投递到「{}」失败：{e:#}", item.target_bot),
                )
                .await;
            }
        }
    }

    /// 回源报错：投递失败要让人知道，不能静默丢。来源 bot/会话空（人工 CLI）则只留日志。
    async fn notify_source(&self, item: &DeliveryItem, msg: &str) {
        if item.source_bot.is_empty() || item.source_chat.is_empty() {
            return;
        }
        let Some(msgr) = self.messengers.get(&item.source_bot) else {
            crate::log!(
                "[deliver] 回源失败：来源 bot {} 不存在或未启用",
                item.source_bot
            );
            return;
        };
        if let Err(e) = msgr.send_text(&item.source_chat, msg).await {
            crate::log!(
                "[deliver] 回源报错发送失败 bot={} chat={}: {e:#}",
                item.source_bot,
                &item.source_chat[..item.source_chat.len().min(16)]
            );
        }
    }
}

/// 解析 `deliver` CLI 参数（main.rs run_deliver_cli 用；拆出便于单测）。
/// env_bot / env_chat 是桥 spawn agent 时注入的 AGENT_BRIDGE_BOT_KEY / AGENT_BRIDGE_CHAT_ID，
/// 作为来源（回源报错）缺省值；人工 CLI 可显式 --source-bot/--source-chat 覆盖。
pub fn parse_deliver_args(
    args: &[String],
    env_bot: &str,
    env_chat: &str,
) -> Result<DeliveryItem, String> {
    let mut target_bot: Option<String> = None;
    let mut target_chat: Option<String> = None;
    let mut text: Option<String> = None;
    let mut source_bot: Option<String> = None;
    let mut source_chat: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let val = |i: &mut usize| -> Result<String, String> {
            *i += 1;
            args.get(*i)
                .map(|s| s.to_string())
                .ok_or_else(|| format!("参数 {} 缺少值", flag))
        };
        match flag {
            "--bot" => target_bot = Some(val(&mut i)?),
            "--chat" => target_chat = Some(val(&mut i)?),
            "--text" => text = Some(val(&mut i)?),
            "--source-bot" => source_bot = Some(val(&mut i)?),
            "--source-chat" => source_chat = Some(val(&mut i)?),
            _ => return Err(format!("未知参数：{flag}")),
        }
        i += 1;
    }
    let target_bot = target_bot.ok_or_else(|| "缺少 --bot <目标 bot key>".to_string())?;
    let target_chat = target_chat.ok_or_else(|| "缺少 --chat <目标 chat_id>".to_string())?;
    let text = text.ok_or_else(|| "缺少 --text <内容>".to_string())?;
    if target_bot.is_empty() {
        return Err("--bot 不能为空".to_string());
    }
    if target_chat.is_empty() {
        return Err("--chat 不能为空".to_string());
    }
    if text.trim().is_empty() {
        return Err("--text 不能为空".to_string());
    }
    Ok(DeliveryItem {
        id: uuid::Uuid::new_v4().to_string(),
        target_bot,
        target_chat,
        text,
        source_bot: source_bot
            .filter(|s| !s.is_empty())
            .or_else(|| (!env_bot.is_empty()).then(|| env_bot.to_string()))
            .unwrap_or_default(),
        source_chat: source_chat
            .filter(|s| !s.is_empty())
            .or_else(|| (!env_chat.is_empty()).then(|| env_chat.to_string()))
            .unwrap_or_default(),
        created_at: crate::chrono_lite::unix_secs(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// 测试用临时目录：返回守卫（Drop 时清理）+ store。
    struct TmpDir(std::path::PathBuf);
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp_store(name: &str) -> (TmpDir, DeliveryStore) {
        let dir = std::env::temp_dir().join(format!(
            "abb-deliver-{name}-{}",
            crate::chrono_lite::unix_secs()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let store = DeliveryStore::new_at(dir.join("deliveries.json"));
        (TmpDir(dir), store)
    }

    fn item(id: &str, target_bot: &str, target_chat: &str, text: &str) -> DeliveryItem {
        DeliveryItem {
            id: id.to_string(),
            target_bot: target_bot.to_string(),
            target_chat: target_chat.to_string(),
            text: text.to_string(),
            source_bot: String::new(),
            source_chat: String::new(),
            created_at: 1,
        }
    }

    #[test]
    fn store_add_dedupes_by_id() {
        let (_d, store) = tmp_store("dedupe");
        store.add(item("a", "wechat", "u1", "hi"));
        store.add(item("a", "wechat", "u1", "hi"));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn store_take_all_clears() {
        let (_d, store) = tmp_store("take");
        store.add(item("a", "wechat", "u1", "hi"));
        store.add(item("b", "feishu", "c2", "yo"));
        let taken = store.take_all();
        assert_eq!(taken.len(), 2);
        assert!(store.is_empty());
    }

    #[test]
    fn store_hot_reloads_external_write() {
        let (d, store) = tmp_store("reload");
        store.add(item("a", "wechat", "u1", "old"));
        // 模拟 CLI 进程在另一个进程直接覆盖文件（mtime 变化 → 热重载）
        let path = d.0.join("deliveries.json");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            &path,
            serde_json::to_string(&vec![item("b", "feishu", "c2", "new")]).unwrap(),
        )
        .unwrap();
        let taken = store.take_all();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].id, "b");
    }

    #[test]
    fn parse_uses_env_defaults_for_source() {
        let args = vec![
            "--bot".to_string(),
            "feishu".to_string(),
            "--chat".to_string(),
            "c1".to_string(),
            "--text".to_string(),
            "hello".to_string(),
        ];
        let d = parse_deliver_args(&args, "wechat", "u1").unwrap();
        assert_eq!(d.target_bot, "feishu");
        assert_eq!(d.target_chat, "c1");
        assert_eq!(d.text, "hello");
        assert_eq!(d.source_bot, "wechat");
        assert_eq!(d.source_chat, "u1");
        assert!(!d.id.is_empty());
    }

    #[test]
    fn parse_requires_target_and_text() {
        assert!(parse_deliver_args(&[], "wechat", "u1").is_err());
        let args = vec![
            "--bot".to_string(),
            "feishu".to_string(),
            "--chat".to_string(),
            "c1".to_string(),
        ];
        assert!(parse_deliver_args(&args, "wechat", "u1").is_err());
        let args = vec![
            "--bot".to_string(),
            "feishu".to_string(),
            "--chat".to_string(),
            "c1".to_string(),
            "--text".to_string(),
            "   ".to_string(),
        ];
        assert!(parse_deliver_args(&args, "wechat", "u1").is_err());
    }

    #[test]
    fn parse_rejects_unknown_flag() {
        let args = vec!["--nope".to_string()];
        assert!(parse_deliver_args(&args, "wechat", "u1").is_err());
    }

    /// 测试假 messenger：记录发送，可配置失败。
    struct FakeMsgr {
        sent: Mutex<Vec<(String, String)>>,
        fail: AtomicBool,
    }
    impl FakeMsgr {
        fn new() -> FakeMsgr {
            FakeMsgr {
                sent: Mutex::new(Vec::new()),
                fail: AtomicBool::new(false),
            }
        }
    }
    #[async_trait]
    impl Messenger for FakeMsgr {
        async fn send_text(&self, chat_id: &str, text: &str) -> Result<()> {
            if self.fail.load(Ordering::Relaxed) {
                anyhow::bail!("模拟发送失败");
            }
            self.sent
                .lock()
                .unwrap()
                .push((chat_id.to_string(), text.to_string()));
            Ok(())
        }
    }

    fn router_with(
        enabled: bool,
        outbox_dir: Option<std::path::PathBuf>,
    ) -> (Router, Arc<FakeMsgr>, Arc<FakeMsgr>) {
        let target = Arc::new(FakeMsgr::new());
        let source = Arc::new(FakeMsgr::new());
        let mut bots = HashMap::new();
        bots.insert(
            "wechat".to_string(),
            BotConfig {
                name: "wechat".into(),
                kind: "wechat".into(),
                ..Default::default()
            },
        );
        bots.insert(
            "feishu".to_string(),
            BotConfig {
                name: "feishu".into(),
                kind: "feishu".into(),
                ..Default::default()
            },
        );
        let router = Router {
            enabled,
            messengers: HashMap::from([
                ("wechat".to_string(), target.clone() as Arc<dyn Messenger>),
                ("feishu".to_string(), source.clone() as Arc<dyn Messenger>),
            ]),
            bots,
            outbox_dir,
        };
        (router, target, source)
    }

    #[tokio::test]
    async fn router_disabled_rejects_and_notifies_source() {
        let (router, target, source) = router_with(false, None);
        let mut d = item("a", "wechat", "u1", "hi");
        d.source_bot = "feishu".into();
        d.source_chat = "c1".into();
        router.deliver(&d).await;
        assert!(target.sent.lock().unwrap().is_empty());
        assert_eq!(source.sent.lock().unwrap().len(), 1);
        assert!(source.sent.lock().unwrap()[0].0 == "c1");
    }

    #[tokio::test]
    async fn router_sends_to_target() {
        let (router, target, source) = router_with(true, None);
        let mut d = item("a", "wechat", "u1", "hi");
        d.source_bot = "feishu".into();
        d.source_chat = "c1".into();
        router.deliver(&d).await;
        assert_eq!(target.sent.lock().unwrap().len(), 1);
        assert_eq!(
            target.sent.lock().unwrap()[0],
            ("u1".to_string(), "hi".to_string())
        );
        assert!(source.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn router_unknown_target_notifies_source() {
        let (router, target, source) = router_with(true, None);
        let mut d = item("a", "ghost", "u1", "hi");
        d.source_bot = "feishu".into();
        d.source_chat = "c1".into();
        router.deliver(&d).await;
        assert!(target.sent.lock().unwrap().is_empty());
        assert_eq!(source.sent.lock().unwrap().len(), 1);
        assert!(source.sent.lock().unwrap()[0].1.contains("ghost"));
    }

    #[tokio::test]
    async fn router_failure_queues_wechat_outbox_and_notifies_source() {
        let dir = std::env::temp_dir().join(format!(
            "abb-deliver-outbox-{}",
            crate::chrono_lite::unix_secs()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let (router, target, source) = router_with(true, Some(dir.clone()));
        target.fail.store(true, Ordering::Relaxed);
        let mut d = item("a", "wechat", "u1", "hi");
        d.source_bot = "feishu".into();
        d.source_chat = "c1".into();
        router.deliver(&d).await;
        assert!(target.sent.lock().unwrap().is_empty());
        // 回源报错
        assert_eq!(source.sent.lock().unwrap().len(), 1);
        assert!(source.sent.lock().unwrap()[0].1.contains("失败"));
        // 微信目标落 outbox（文件存在且可读出该项）
        let path = dir.join("pending_outbox.json");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"id\": \"a\""));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
