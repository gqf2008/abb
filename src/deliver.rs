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
use std::collections::{HashMap, VecDeque};
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
    /// 附件元数据（#21 附件跨投递）：转发本地路径/元数据，接收端按能力处理。
    #[serde(default)]
    pub attachments: Vec<crate::attachments::AttachmentMeta>,
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
    /// 最近投递指纹（防循环 #21）：`(时间, 来源+目标+文本指纹)`，窗口内重复投递直接跳过。
    recent: Mutex<VecDeque<(u64, String)>>,
}

/// 防循环去重窗口（秒）：同一 (来源 bot/会话, 目标 bot/会话, 文本) 在窗口内只投一次。
const RECENT_WINDOW_SECS: u64 = 600;

impl Router {
    pub fn new(
        enabled: bool,
        messengers: HashMap<String, Arc<dyn Messenger>>,
        bots: HashMap<String, BotConfig>,
        outbox_dir: Option<PathBuf>,
    ) -> Router {
        Router {
            enabled,
            messengers,
            bots,
            outbox_dir,
            recent: Mutex::new(VecDeque::new()),
        }
    }

    /// 投递一条消息。成功只留日志；失败：微信目标落其 outbox 等下次入站补发，
    /// 同时回源 bot/会话报错（best-effort，不静默丢失）。未知目标/开关关闭也回源提示。
    /// 防循环：同一 (来源, 目标, 文本) 在窗口内重复投递会跳过并回源提示。
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
        // 防循环（消息循环防护 #21）：同指纹窗口内重复 → 跳过 + 回源说明，不无限转发。
        // 注意：MutexGuard 必须在任何 await 前 drop（std 锁不是 Send，跨 await 会编译失败）。
        let dup_key = format!(
            "{}|{}|{}|{}|{}",
            item.source_bot, item.source_chat, item.target_bot, item.target_chat, item.text
        );
        let is_dup = {
            let mut recent = self.recent.lock().unwrap();
            let now = crate::chrono_lite::unix_secs();
            while recent
                .front()
                .map(|(t, _)| *t + RECENT_WINDOW_SECS < now)
                .unwrap_or(false)
            {
                recent.pop_front();
            }
            let dup = recent.iter().any(|(_, k)| k == &dup_key);
            if !dup {
                recent.push_back((now, dup_key));
            }
            dup
        };
        if is_dup {
            crate::log!(
                "[deliver] 跳过重复投递（防循环）bot={} chat={} id={}",
                item.target_bot,
                &item.target_chat[..item.target_chat.len().min(16)],
                &item.id[..item.id.len().min(8)]
            );
            self.notify_source(
                item,
                "⚠️ 相同内容刚投递过，已跳过（防循环）。如需再次投递请稍后（10 分钟内抑制重复）。",
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
        // 先发文本，再逐个发附件（附件失败只回源报错，附件无 outbox 文本可补）。
        if !item.text.is_empty() {
            if let Err(e) = msgr.send_text(&item.target_chat, &item.text).await {
                self.fail_text(item, &e).await;
                return;
            }
        }
        for meta in &item.attachments {
            if let Err(e) = msgr.send_attachment(&item.target_chat, meta).await {
                crate::log!(
                    "[deliver] 附件投递失败 bot={} chat={} 文件名={}: {e:#}",
                    item.target_bot,
                    &item.target_chat[..item.target_chat.len().min(16)],
                    meta.file_name
                );
                self.notify_source(
                    item,
                    &format!(
                        "⚠️ 跨会话投递到「{}」的附件「{}」失败：{e:#}（元数据/本地路径已在前一条文本里）。",
                        item.target_bot, meta.file_name
                    ),
                )
                .await;
                return;
            }
        }
        crate::log!(
            "[deliver] 已投递 bot={} chat={} id={} 文本长度={} 附件数={}",
            item.target_bot,
            &item.target_chat[..item.target_chat.len().min(16)],
            &item.id[..item.id.len().min(8)],
            item.text.chars().count(),
            item.attachments.len()
        );
    }

    /// 文本投递失败：微信目标落其 outbox 等下次入站补发（#9 模式），同时回源报错。
    async fn fail_text(&self, item: &DeliveryItem, e: &anyhow::Error) {
        crate::log!(
            "[deliver] 投递失败 bot={} chat={} id={}: {e:#}",
            item.target_bot,
            &item.target_chat[..item.target_chat.len().min(16)],
            &item.id[..item.id.len().min(8)]
        );
        if self
            .bots
            .get(&item.target_bot)
            .map(|b| b.is_wechat())
            .unwrap_or(false)
        {
            let store = match &self.outbox_dir {
                Some(dir) => crate::outbox::OutboxStore::new_at(dir.join("pending_outbox.json")),
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
    let mut files: Vec<String> = Vec::new();
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
            "--file" => files.push(val(&mut i)?),
            "--source-bot" => source_bot = Some(val(&mut i)?),
            "--source-chat" => source_chat = Some(val(&mut i)?),
            _ => return Err(format!("未知参数：{flag}")),
        }
        i += 1;
    }
    let target_bot = target_bot.ok_or_else(|| "缺少 --bot <目标 bot key>".to_string())?;
    let target_chat = target_chat.ok_or_else(|| "缺少 --chat <目标 chat_id>".to_string())?;
    let text = text.unwrap_or_default();
    if target_bot.is_empty() {
        return Err("--bot 不能为空".to_string());
    }
    if target_chat.is_empty() {
        return Err("--chat 不能为空".to_string());
    }
    if text.trim().is_empty() && files.is_empty() {
        return Err("--text 和 --file 至少给一个".to_string());
    }
    // 纯附件投递：空白 text 归一为空（避免给目标发一条空白消息）
    let text = text.trim().to_string();
    let mut attachments = Vec::with_capacity(files.len());
    for f in &files {
        attachments.push(attachment_meta_from_path(f)?);
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
        attachments,
    })
}

/// 把本地文件转成附件元数据（deliver --file 用）。跨 bot 同机运行，接收端可直接读本地路径。
pub fn attachment_meta_from_path(path: &str) -> Result<crate::attachments::AttachmentMeta, String> {
    let p = std::path::Path::new(path);
    let file_name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let bytes = std::fs::read(p).map_err(|e| format!("读取附件失败 {path}: {e}"))?;
    if bytes.is_empty() {
        return Err(format!("附件为空：{path}"));
    }
    if bytes.len() > crate::attachments::MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "附件超过大小上限（{}MB）：{path}",
            crate::attachments::MAX_ATTACHMENT_BYTES / 1024 / 1024
        ));
    }
    let kind = crate::attachments::kind_from_name(&file_name);
    Ok(crate::attachments::AttachmentMeta {
        kind: kind.clone(),
        source: "deliver".to_string(),
        file_name: file_name.clone(),
        mime: crate::attachments::mime_from_name(&file_name, &kind),
        size: bytes.len() as u64,
        path: std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .display()
            .to_string(),
        sha256: crate::attachments::sha256_hex(&bytes),
        note: String::new(),
    })
}

/// 是否自环投递：来源 == 目标（同 bot 同会话转发给自己，无意义且是循环温床）。
pub fn is_self_loop(item: &DeliveryItem) -> bool {
    !item.source_bot.is_empty()
        && !item.source_chat.is_empty()
        && item.source_bot == item.target_bot
        && item.source_chat == item.target_chat
}

/// 定时任务多目标（#21）：把 Job.targets 展开成待投递项（bot_key 空 = 本 bot）。
/// 来源固定为任务所属 bot/原会话，投递失败由 Router 回源报错。
pub fn job_target_items(
    job_bot: &str,
    job_chat: &str,
    targets: &[crate::schedule::JobTarget],
    text: &str,
) -> Vec<DeliveryItem> {
    targets
        .iter()
        .map(|t| DeliveryItem {
            id: uuid::Uuid::new_v4().to_string(),
            target_bot: if t.bot_key.is_empty() {
                job_bot.to_string()
            } else {
                t.bot_key.clone()
            },
            target_chat: t.chat_id.clone(),
            text: text.to_string(),
            source_bot: job_bot.to_string(),
            source_chat: job_chat.to_string(),
            created_at: crate::chrono_lite::unix_secs(),
            attachments: Vec::new(),
        })
        .collect()
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
            attachments: Vec::new(),
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
        let router = Router::new(
            enabled,
            HashMap::from([
                ("wechat".to_string(), target.clone() as Arc<dyn Messenger>),
                ("feishu".to_string(), source.clone() as Arc<dyn Messenger>),
            ]),
            bots,
            outbox_dir,
        );
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

    #[test]
    fn parse_accepts_file_attachments() {
        let dir = std::env::temp_dir().join(format!(
            "abb-deliver-file-{}",
            crate::chrono_lite::unix_secs()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("pic.png");
        std::fs::write(&path, b"fake-png-bytes").unwrap();
        let args = vec![
            "--bot".to_string(),
            "feishu".to_string(),
            "--chat".to_string(),
            "c1".to_string(),
            "--file".to_string(),
            path.display().to_string(),
        ];
        let d = parse_deliver_args(&args, "wechat", "u1").unwrap();
        assert_eq!(d.attachments.len(), 1);
        assert_eq!(d.attachments[0].kind, "image");
        assert_eq!(d.attachments[0].mime, "image/png");
        assert_eq!(d.attachments[0].size, 14);
        assert_eq!(d.attachments[0].sha256.len(), 64);
        assert!(d.attachments[0].path.contains("pic.png"));
        // 不存在的文件 → 报错
        let bad = vec![
            "--bot".to_string(),
            "feishu".to_string(),
            "--chat".to_string(),
            "c1".to_string(),
            "--file".to_string(),
            dir.join("nope.png").display().to_string(),
        ];
        assert!(parse_deliver_args(&bad, "wechat", "u1").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_allows_file_only() {
        // --text 空 + --file 有 → 允许（纯附件投递）
        let dir = std::env::temp_dir().join(format!(
            "abb-deliver-fileonly-{}",
            crate::chrono_lite::unix_secs()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("a.txt");
        std::fs::write(&path, b"hi").unwrap();
        let args = vec![
            "--bot".to_string(),
            "feishu".to_string(),
            "--chat".to_string(),
            "c1".to_string(),
            "--text".to_string(),
            " ".to_string(),
            "--file".to_string(),
            path.display().to_string(),
        ];
        assert!(parse_deliver_args(&args, "wechat", "u1").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn self_loop_detected() {
        let mut d = item("a", "wechat", "u1", "hi");
        d.source_bot = "wechat".into();
        d.source_chat = "u1".into();
        assert!(is_self_loop(&d));
        d.source_chat = "u2".into();
        assert!(!is_self_loop(&d));
        // 来源为空（人工 CLI）不算自环
        let d2 = item("b", "wechat", "u1", "hi");
        assert!(!is_self_loop(&d2));
    }

    #[test]
    fn job_target_items_expands_bot_key() {
        let targets = vec![
            crate::schedule::JobTarget {
                bot_key: String::new(),
                chat_id: "oc_own".into(),
            },
            crate::schedule::JobTarget {
                bot_key: "feishu".into(),
                chat_id: "oc_feishu".into(),
            },
        ];
        let items = job_target_items("wechat", "oc_src", &targets, "结果");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].target_bot, "wechat"); // bot_key 空 → 本 bot
        assert_eq!(items[0].target_chat, "oc_own");
        assert_eq!(items[1].target_bot, "feishu");
        assert_eq!(items[1].source_bot, "wechat");
        assert_eq!(items[1].source_chat, "oc_src");
        assert_eq!(items[1].text, "结果");
    }

    #[tokio::test]
    async fn router_sends_attachment_meta() {
        let (router, target, _source) = router_with(true, None);
        let mut d = item("a", "wechat", "u1", "看附件");
        d.attachments.push(crate::attachments::AttachmentMeta {
            kind: "image".into(),
            source: "deliver".into(),
            file_name: "pic.png".into(),
            mime: "image/png".into(),
            size: 13,
            path: "/tmp/pic.png".into(),
            sha256: "ab".into(),
            note: String::new(),
        });
        router.deliver(&d).await;
        let sent = target.sent.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0], ("u1".to_string(), "看附件".to_string()));
        assert!(
            sent[1].1.contains("pic.png"),
            "附件元数据应随文本发出: {}",
            sent[1].1
        );
    }

    #[tokio::test]
    async fn router_skips_duplicate_within_window() {
        let (router, target, source) = router_with(true, None);
        let mut d = item("a", "wechat", "u1", "hi");
        d.source_bot = "feishu".into();
        d.source_chat = "c1".into();
        router.deliver(&d).await;
        router.deliver(&d).await;
        assert_eq!(target.sent.lock().unwrap().len(), 1);
        // 第二次被抑制并回源提示
        assert_eq!(source.sent.lock().unwrap().len(), 1);
        assert!(source.sent.lock().unwrap()[0].1.contains("防循环"));
    }

    #[tokio::test]
    async fn router_different_source_same_text_not_duplicate() {
        let (router, target, _source) = router_with(true, None);
        let mut d1 = item("a", "wechat", "u1", "hi");
        d1.source_bot = "feishu".into();
        d1.source_chat = "c1".into();
        let mut d2 = item("b", "wechat", "u1", "hi");
        d2.source_bot = "feishu".into();
        d2.source_chat = "c2".into();
        router.deliver(&d1).await;
        router.deliver(&d2).await;
        assert_eq!(target.sent.lock().unwrap().len(), 2); // 不同来源不算重复
    }
}
