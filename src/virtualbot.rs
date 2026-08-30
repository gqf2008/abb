//! 虚拟 Bot（#75）—— 平台真实群 + ABB 登记，群名=角色名、群介绍=system prompt。
//!
//! 三个职责：
//! 1. **登记表** `~/.agent-bridge/virtual-bots.json`：`[{bot_key, chat_id, role_name, created_at}]`。
//!    进程间共享：service 注入判定（bridge 每次群消息查快照）、deliver CLI @角色名寻址、
//!    GUI 管理都读它。并发模型：**只读 + 整文件原子重写**（`atomic_write_sensitive` 唯一
//!    tmp + rename）。写方 = GUI 进程（创建/取消登记/解散）+ **service 事件驱动**
//!    （im.chat.deleted_v1 群被解散自动移除，见 bridge.rs on_chat_deleted）；deliver CLI
//!    只读。整文件重写没有锁，并发写（极罕见、用户驱动 + 事件驱动）会丢更新
//!    （last-writer-wins）——与 deliveries.json 的 CAS 不同，这里不追求：登记增删是低频
//!    人工操作，原子 rename 保证读侧永远读到完整文件（无半截）。
//! 2. **角色模板库**：内置 10+ 角色（虚拟团队场景：一次建整套角色群）+ 自定义模板
//!    （存 Config.custom_roles，见 config.rs）。模板 = 群名 + 提示词（≤100 字符，
//!    对齐飞书群描述限制；钉钉暂同）。
//! 3. **群资料缓存**（5 分钟 TTL）：注入前查缓存，「改群介绍即时生效」的载体——
//!    缓存过期自然刷新，不做变更推送。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 一条虚拟 Bot 登记：平台真实群 + 角色名（=群名）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualBot {
    /// 归属 bot key（config bots[].key()；注入判定与 deliver 寻址都按它过滤）。
    pub bot_key: String,
    /// 平台群会话 id（飞书 oc_… / 钉钉 cid…）。
    pub chat_id: String,
    /// 角色名（= 平台群名；deliver 用 `--chat @角色名` 寻址）。
    pub role_name: String,
    /// 登记时间（unix 秒，GUI 列表展示用）。
    pub created_at: u64,
}

/// 角色模板：群名 + 提示词（=群介绍）。群名=角色名。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoleTemplate {
    pub name: String,
    pub prompt: String,
}

/// 群名长度上限（飞书群名限制；钉钉暂同——UI 与 Rust 两侧共用）。
pub const ROLE_NAME_MAX: usize = 60;
/// 群介绍长度上限（飞书群描述限制；钉钉暂同——钉钉群其实没有群介绍字段，
/// 见 dingtalk.rs create_chat 注释，长度限制只是提前对齐）。
pub const ROLE_PROMPT_MAX: usize = 100;

/// 登记表存取。见模块注释的并发模型。
pub struct VirtualBotStore {
    path: PathBuf,
}

impl VirtualBotStore {
    pub fn new() -> VirtualBotStore {
        VirtualBotStore::new_at(crate::bridge_dir().join("virtual-bots.json"))
    }

    /// 测试/自建路径注入（单测用临时目录，不碰真实登记表）。
    pub fn new_at(path: PathBuf) -> VirtualBotStore {
        VirtualBotStore { path }
    }

    /// 读全部登记。文件缺失/损坏 → 空列表（不 panic，损坏时日志留痕）。
    pub fn load(&self) -> Vec<VirtualBot> {
        match std::fs::read_to_string(&self.path) {
            Ok(t) => match serde_json::from_str(&t) {
                Ok(v) => v,
                Err(e) => {
                    crate::log!("[virtualbot] 登记表解析失败（按空处理，重新登记会覆盖）: {e:#}");
                    Vec::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                crate::log!("[virtualbot] 读登记表失败: {e:#}");
                Vec::new()
            }
        }
    }

    /// 某 bot 的登记（注入判定 / GUI 列表共用）。
    pub fn load_for(&self, bot_key: &str) -> Vec<VirtualBot> {
        self.load()
            .into_iter()
            .filter(|v| v.bot_key == bot_key)
            .collect()
    }

    /// 新增登记。Err(String)：
    /// - 同 bot 同 chat_id 已登记（重复建群/登记）；
    /// - 同 bot 同角色名已存在（群名=角色名，重名角色无法区分——群 @ 时该注入哪份
    ///   prompt？deliver 寻址该投给谁？都歧义，直接拒绝）。
    pub fn add(&self, bot: VirtualBot) -> Result<(), String> {
        let mut cur = self.load();
        if cur
            .iter()
            .any(|v| v.bot_key == bot.bot_key && v.chat_id == bot.chat_id)
        {
            return Err("该群已登记过（chat_id 重复）".to_string());
        }
        if cur
            .iter()
            .any(|v| v.bot_key == bot.bot_key && v.role_name == bot.role_name)
        {
            return Err(format!(
                "角色「{}」已登记（群名=角色名，请先取消登记或换名）",
                bot.role_name
            ));
        }
        cur.push(bot);
        self.write(&cur)
    }

    /// 取消登记（平台群保留，只删 ABB 登记）。返回 true=确有删除。
    pub fn remove(&self, bot_key: &str, chat_id: &str) -> bool {
        let cur = self.load();
        let before = cur.len();
        let next: Vec<VirtualBot> = cur
            .into_iter()
            .filter(|v| !(v.bot_key == bot_key && v.chat_id == chat_id))
            .collect();
        if next.len() == before {
            return false;
        }
        self.write(&next).is_ok()
    }

    /// 群解散后归档会话历史（8-20 用户决策）：把该 chat 的 history jsonl（+迁移/导入
    /// 标记文件）从 `workspaces/<bot>/history/<key>.jsonl` 移到
    /// `workspaces/<bot>/archive/<key>-<ts>.jsonl`——不删除（可追溯），移出活跃区。
    /// #194：独立工作区 vb/<uuid>/history/ 的文件一并归档（审查 F4：迁移后 bot 级
    /// 已空，只扫 bot 级会恒 0 文件）。
    /// 返回归档的文件数。调用方：im.chat.deleted_v1 事件 / GUI 手动刷新发现群不存在。
    pub fn archive_chat_history(bot_key: &str, chat_id: &str) -> usize {
        let ts = crate::chrono_lite::unix_secs();
        let prefix = crate::config::sanitize(chat_id);
        let archive = |hist_dir: &std::path::Path, archive_dir: &std::path::Path| -> usize {
            let mut moved = 0;
            let entries = match std::fs::read_dir(hist_dir) {
                Ok(e) => e.flatten().collect::<Vec<_>>(),
                Err(_) => return 0,
            };
            for e in entries {
                let name = e.file_name().to_string_lossy().to_string();
                if !(name.starts_with(&prefix)
                    && (name.ends_with(".jsonl") || name.ends_with(".json")))
                {
                    continue;
                }
                let _ = std::fs::create_dir_all(archive_dir);
                let dst = archive_dir.join(format!("{name}.archived-{ts}"));
                if std::fs::rename(e.path(), dst).is_ok() {
                    moved += 1;
                }
            }
            moved
        };
        let mut moved = archive(
            &crate::workspace_dir(bot_key).join("history"),
            &crate::workspace_dir(bot_key).join("archive"),
        );
        // #194：独立工作区侧（登记存在才扫）
        if let Some(vb_dir) = vb_dir_for(bot_key, chat_id) {
            moved += archive(&vb_dir.join("history"), &vb_dir.join("archive"));
        }
        if moved > 0 {
            crate::log!("[virtualbot] 群 {chat_id} 已归档 {moved} 个历史文件 → archive/");
        }
        moved
    }

    /// 更新登记的角色名（GUI 编辑改名：平台群改名后同步登记；chat_id 不变）。
    /// 重名校验同 add：同 bot 下角色名唯一。
    pub fn update_role(&self, bot_key: &str, chat_id: &str, new_role: &str) -> Result<(), String> {
        let cur = self.load();
        if !cur
            .iter()
            .any(|v| v.bot_key == bot_key && v.chat_id == chat_id)
        {
            return Err("该群未登记".to_string());
        }
        if cur
            .iter()
            .any(|v| v.bot_key == bot_key && v.chat_id != chat_id && v.role_name == new_role)
        {
            return Err(format!("角色「{new_role}」已被其它群占用"));
        }
        let next: Vec<VirtualBot> = cur
            .into_iter()
            .map(|mut v| {
                if v.bot_key == bot_key && v.chat_id == chat_id {
                    v.role_name = new_role.to_string();
                }
                v
            })
            .collect();
        // 未变（角色名相同）也照写：幂等
        self.write(&next)
    }

    /// deliver 寻址：@角色名 → chat_id（同 bot 上下文；第一个匹配）。
    pub fn resolve(&self, bot_key: &str, role_name: &str) -> Option<String> {
        self.load()
            .into_iter()
            .find(|v| v.bot_key == bot_key && v.role_name == role_name)
            .map(|v| v.chat_id)
    }

    /// 该 bot 已登记的角色名列表（deliver 寻址失败时的可用角色提示）。
    pub fn roles_for(&self, bot_key: &str) -> Vec<String> {
        self.load_for(bot_key)
            .into_iter()
            .map(|v| v.role_name)
            .collect()
    }

    /// chat 是否已登记为虚拟 Bot 群（本 bot 名下）。
    pub fn is_registered(&self, bot_key: &str, chat_id: &str) -> bool {
        self.load_for(bot_key).iter().any(|v| v.chat_id == chat_id)
    }

    /// 整文件原子重写（唯一 tmp + rename；`atomic_write_sensitive` 语义同 config）。
    fn write(&self, entries: &[VirtualBot]) -> Result<(), String> {
        let text = serde_json::to_string_pretty(entries).map_err(|e| e.to_string())?;
        crate::atomic_write_sensitive(&self.path, &text).map_err(|e| format!("写登记表失败: {e}"))
    }
}

impl Default for VirtualBotStore {
    fn default() -> Self {
        Self::new()
    }
}

// ══════════ #194 虚拟 Bot 独立工作区 ══════════
//
// 隔离粒度从会话级升级到目录级（老板需求 2026-08-30）：
// - 每个虚拟 Bot 群独立目录 `workspaces/<bot_key>/vb/<uuid>/`（自有 AGENTS.md、
//   会话记录、历史，agent cwd 也在这里）；
// - 可读 bot 工作区（读不受限），可写仅限自己目录（codex 靠 cwd+roots、claude 靠
//   guard 双区判定），不可写其他虚拟 Bot 目录与 bot 工作区根；
// - 目录名用确定性 UUID 而非角色名——角色可改名/重名/含特殊字符；(bot_key, chat_id)
//   派生（fnv128，uuid v5 同思路），跨进程/重启稳定，无需存储与迁移字段。

/// 虚拟 Bot 目录名的确定性 UUID（fnv128 → 8-4-4-4-12 十六进制）。
/// 输入含 bot_key：同群换绑 bot（理论上不发生）视为新身份。
pub fn vb_uuid(bot_key: &str, chat_id: &str) -> String {
    fn fnv64(seed: u64, s: &str) -> u64 {
        let mut h = seed;
        for b in s.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h
    }
    let ns = format!("abb-vb:{bot_key}:{chat_id}");
    let hi = fnv64(0xcbf2_9ce4_8422_2325, &ns);
    let lo = fnv64(0x9e37_79b9_7f4a_7c15, &ns);
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&hi.to_be_bytes());
    bytes[8..].copy_from_slice(&lo.to_be_bytes());
    let hex = |r: std::ops::Range<usize>| {
        bytes[r]
            .iter()
            .map(|x| format!("{x:02x}"))
            .collect::<String>()
    };
    format!(
        "{}-{}-{}-{}-{}",
        hex(0..4),
        hex(4..6),
        hex(6..8),
        hex(8..10),
        hex(10..16)
    )
}

/// chat 的虚拟 Bot 独立工作目录（只解析不落盘）。None = 非虚拟 Bot 群。
pub fn vb_dir_for(bot_key: &str, chat_id: &str) -> Option<PathBuf> {
    if VirtualBotStore::new().is_registered(bot_key, chat_id) {
        Some(
            crate::workspace_dir(bot_key)
                .join("vb")
                .join(vb_uuid(bot_key, chat_id)),
        )
    } else {
        None
    }
}

/// 解析并确保虚拟 Bot 工作目录存在；同时把该群的存量数据一次性迁入（#194）：
/// bot 级 sessions.json 中本 chat 的槽位迁到 vb/<uuid>/sessions.json（迁移后从 bot
/// 级移除，两处不双写）；bot 级 history/ 中本 chat 前缀的历史文件迁到 vb/<uuid>/
/// history/。逐项搬移幂等（源不存在即 no-op），service/GUI 谁先解析都收敛。
/// None = 非虚拟群。
pub fn ensure_vb_dir(bot_key: &str, chat_id: &str) -> Option<PathBuf> {
    let dir = vb_dir_for(bot_key, chat_id)?;
    let _ = std::fs::create_dir_all(&dir);
    migrate_legacy_vb_data(&crate::bridge_dir(), bot_key, chat_id, &dir);
    Some(dir)
}

/// 存量迁移（#194）：bot 级会话槽位 + 前缀匹配的历史文件搬入 vb 目录。幂等。
fn migrate_legacy_vb_data(
    base: &std::path::Path,
    bot_key: &str,
    chat_id: &str,
    vb_dir: &std::path::Path,
) {
    // 已迁移 marker：短路重复扫描/解析（每消息多次触达本函数）。部分失败不落
    // marker，下次重试（独立审查 F8）。
    let marker = vb_dir.join(".migrated");
    if marker.exists() {
        return;
    }
    // 1) 会话槽位：bot 级 sessions.json → vb/sessions.json（backend 无关，整 chat 搬，
    //    含话题键；独立审查 F3）
    let bot_sessions = base.join("workspaces").join(bot_key).join("sessions.json");
    let bot_store = crate::sessions::SessionStore::at("codex", bot_sessions);
    let vb_store = crate::sessions::SessionStore::at("codex", vb_dir.join("sessions.json"));
    let _ = bot_store.extract_chat_to(chat_id, &vb_store);
    // 2) 历史文件族：bot history/ 下 escape(chat). 前缀的**全部**文件（.jsonl/.json/
    //    .ctxsum/.migrated.json/.imported.json——独立审查 F9；前缀用 escape_key 与
    //    文件命名同源，sanitize 会漏大写/特殊字符 chat——独立审查 F8）
    let esc = crate::history::escape_key(chat_id);
    let prefix = format!("{esc}.");
    let bot_hist = base.join("workspaces").join(bot_key).join("history");
    let vb_hist = vb_dir.join("history");
    let Ok(entries) = std::fs::read_dir(&bot_hist) else {
        return;
    };
    let mut moved = 0usize;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with(&prefix) {
            let _ = std::fs::create_dir_all(&vb_hist);
            let dst = vb_hist.join(&name);
            if !dst.exists() && std::fs::rename(e.path(), dst).is_ok() {
                moved += 1;
            }
        }
    }
    // 3) 会话级指令：bot sessions/<esc>.AGENTS.md → vb/sessions/（独立审查 F9）
    let bot_ses = base.join("workspaces").join(bot_key).join("sessions");
    let vb_ses = vb_dir.join("sessions");
    let agents_md = format!("{esc}.AGENTS.md");
    if bot_ses.join(&agents_md).exists() {
        let _ = std::fs::create_dir_all(&vb_ses);
        let _ = std::fs::rename(bot_ses.join(&agents_md), vb_ses.join(&agents_md));
    }
    if moved > 0 {
        crate::log!("[virtualbot] 群 {chat_id} 存量历史 {moved} 个文件已迁入独立工作区");
    }
    let _ = std::fs::write(&marker, "1");
}

/// 测试辅助：迁移函数暴露给同 crate 单测（ensure_vb_dir 依赖真实登记表路径，
/// 单测直接驱动迁移核心）。
#[cfg(test)]
pub(crate) fn migrate_legacy_vb_data_pub(
    base: &std::path::Path,
    bot_key: &str,
    chat_id: &str,
    vb_dir: &std::path::Path,
) {
    migrate_legacy_vb_data(base, bot_key, chat_id, vb_dir);
}

/// 内置角色模板库（虚拟团队场景：一次建整套角色群：后端/前端/UIUX/产品/…）。
/// 提示词 ≤100 字符（飞书群描述上限）——测试里统一断言，防止新增模板悄悄超限。
pub fn builtin_templates() -> Vec<RoleTemplate> {
    vec![
        RoleTemplate {
            name: "后端开发".into(),
            prompt: "你是后端开发工程师：负责 API 设计、数据建模与性能优化。输出可运行代码并附测试要点，动手前先确认需求边界。".into(),
        },
        RoleTemplate {
            name: "前端开发".into(),
            prompt: "你是前端开发工程师：负责界面实现与交互体验优化。遵循项目既有技术栈与风格，先说明改动影响范围，交付可运行的代码。".into(),
        },
        RoleTemplate {
            name: "UIUX 设计师".into(),
            prompt: "你是 UI/UX 设计师：负责界面视觉与交互体验。先理解用户场景再给方案，输出包含布局、配色与组件说明。".into(),
        },
        RoleTemplate {
            name: "产品经理".into(),
            prompt: "你是产品经理：负责需求分析与方案设计。先澄清目标用户与核心场景，再输出需求清单、优先级与验收标准。".into(),
        },
        RoleTemplate {
            name: "需求系统设计师".into(),
            prompt: "你是需求系统设计师：把业务需求转化为系统设计。输出模块划分、数据流与接口契约，先对齐边界再出方案。".into(),
        },
        RoleTemplate {
            name: "市场调查".into(),
            prompt: "你是市场调查分析师：负责竞品与市场研究。结论附数据来源与置信度，先给执行计划再产出报告。".into(),
        },
        RoleTemplate {
            name: "需求功能迭代".into(),
            prompt: "你是需求迭代负责人：把新需求排进迭代计划。评估工作量与依赖，给出分期方案与验收标准。".into(),
        },
        RoleTemplate {
            name: "事项管理".into(),
            prompt: "你是事项管理员：负责任务分派与进度跟踪。把需求拆成可执行事项，标注负责人、截止时间与阻塞项。".into(),
        },
        RoleTemplate {
            name: "营销策划师".into(),
            prompt: "你是营销策划师：负责活动策划与文案输出。先定目标人群与渠道，再产出可执行的策划方案。".into(),
        },
        RoleTemplate {
            name: "运营".into(),
            prompt: "你是运营专员：负责用户增长与日常运营。方案需可量化目标，先给指标口径再给动作清单。".into(),
        },
        RoleTemplate {
            name: "测试工程师".into(),
            prompt: "你是测试工程师：负责质量保障与用例设计。覆盖边界与异常路径，先列测试计划再执行并输出缺陷报告。".into(),
        },
    ]
}

/// 群资料内存缓存（#75）——「改群介绍即时生效」的载体：每次注入前查缓存，
/// 缓存过期自然刷新（5 分钟 TTL），不做变更推送。
///
/// 两个来源**分开记时**：
/// - `event_names`：事件自带群名（飞书 message.chat_name / 钉钉 conversationTitle），
///   每条群消息刷新——平台改群名，下一条消息注入的就是新名；
/// - `api`：`Messenger::get_chat_info` 拉取的 (群名, 群介绍)，5 分钟过期。
///
/// 分开的关键：事件名刷新**不能**顺带延长群介绍的有效期——否则活跃群里改了群介绍
/// 永远不生效（TTL 被消息频率不断续期），「改群介绍=改角色，即时生效」就名存实亡。
pub struct ChatInfoCache {
    api: Mutex<HashMap<String, (String, String, Instant)>>,
    event_names: Mutex<HashMap<String, (String, Instant)>>,
    ttl: Duration,
}

/// 默认缓存有效期：5 分钟（对齐需求设计）。
const CACHE_TTL: Duration = Duration::from_secs(300);

impl ChatInfoCache {
    pub fn new() -> ChatInfoCache {
        ChatInfoCache::with_ttl(CACHE_TTL)
    }

    /// 测试用：注入更短的 TTL，免等 5 分钟验证过期刷新。
    fn with_ttl(ttl: Duration) -> ChatInfoCache {
        ChatInfoCache {
            api: Mutex::new(HashMap::new()),
            event_names: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// 事件自带群名入库（on_payload / on_dingtalk 每条群消息调一次；空名忽略）。
    pub fn note_event_name(&self, chat_id: &str, name: &str) {
        if name.is_empty() {
            return;
        }
        self.event_names
            .lock()
            .unwrap()
            .insert(chat_id.to_string(), (name.to_string(), Instant::now()));
    }

    /// 注入用取数：返回 (群名, 群介绍)。群名优先事件名（最新），回落 API 名；
    /// 群介绍缓存过期/缺失时经 messenger 拉一次（best-effort，失败不阻塞）。
    /// 两者都拿不到 → None（调用方跳过注入）。
    pub async fn get(
        &self,
        chat_id: &str,
        msgr: &dyn crate::messenger::Messenger,
    ) -> Option<(String, String)> {
        let now = Instant::now();
        let event_name: Option<String> = self
            .event_names
            .lock()
            .unwrap()
            .get(chat_id)
            .filter(|(_, ts)| now.duration_since(*ts) <= self.ttl)
            .map(|(n, _)| n.clone());
        let api_entry = self
            .api
            .lock()
            .unwrap()
            .get(chat_id)
            .filter(|(_, _, ts)| now.duration_since(*ts) <= self.ttl)
            .cloned();
        let (name, desc): (Option<String>, Option<String>) = match api_entry {
            Some((n, d, _)) => (event_name.or(Some(n)), Some(d)),
            None => match msgr.get_chat_info(chat_id).await {
                Some((n, d)) => {
                    // 拉取成功入库（重新计时）；事件名仍优先（可能比 API 更新）
                    self.api
                        .lock()
                        .unwrap()
                        .insert(chat_id.to_string(), (n.clone(), d.clone(), Instant::now()));
                    (event_name.or(Some(n)), Some(d))
                }
                // 拉取失败：事件名还在 → 只有名；都不在 → None（跳过注入）
                None => match event_name {
                    Some(n) => (Some(n), None),
                    None => (None, None),
                },
            },
        };
        match (name, desc) {
            (Some(n), Some(d)) => Some((n, d)),
            (Some(n), None) => Some((n, String::new())),
            (None, Some(d)) => Some((String::new(), d)),
            (None, None) => None,
        }
    }
}

impl Default for ChatInfoCache {
    fn default() -> Self {
        Self::new()
    }
}

/// 拼注入块：[群角色]\n群名：{name}\n群介绍：{desc}\n\n（前置进 agent prompt）。
/// 群介绍是 system prompt（平台群资料为准）；空介绍只注入群名。
pub fn role_block(name: &str, desc: &str) -> String {
    format!("[群角色]\n群名：{name}\n群介绍：{desc}\n\n")
}

/// unix 秒 → "YYYY-MM-DD HH:MM"（本地时区，与 chrono_lite::now() 同为简化 UTC+8）。
/// GUI 登记列表展示用——虚拟 Bot 的创建时间。
pub fn format_created(secs: u64) -> String {
    let local = secs + 8 * 3600;
    let days = local / 86400;
    let rem = local % 86400;
    let (y, m, d) = days_to_ymd(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// 天数（自纪元）→ (年, 月, 日)：Howard Hinnant 的 civil_from_days 算法。
/// 注意算法尾部 `y + (m <= 2)`：1/2 月要并入上一日历年（12/1 月的日期算在
/// 下一年之前）——漏掉这步会整体差一年（审查时踩过，测试锁了 1970/2026 两个锚点）。
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    let z = days as i64 + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = (yoe as i64 + era * 400) + if m <= 2 { 1 } else { 0 };
    (y as u64, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TmpDir(std::path::PathBuf);
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp_store(name: &str) -> (TmpDir, VirtualBotStore) {
        let dir = std::env::temp_dir().join(format!(
            "abb-virtualbot-{name}-{}",
            crate::chrono_lite::unix_secs()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let store = VirtualBotStore::new_at(dir.join("virtual-bots.json"));
        (TmpDir(dir), store)
    }

    fn vb(bot_key: &str, chat_id: &str, role: &str) -> VirtualBot {
        VirtualBot {
            bot_key: bot_key.to_string(),
            chat_id: chat_id.to_string(),
            role_name: role.to_string(),
            created_at: 1,
        }
    }

    #[test]
    fn store_add_remove_roundtrip() {
        let (_d, store) = tmp_store("roundtrip");
        assert!(store.add(vb("feishu", "oc_1", "后端开发")).is_ok());
        assert!(store.add(vb("feishu", "oc_2", "前端开发")).is_ok());
        assert!(store.add(vb("dingtalk", "cid3", "测试工程师")).is_ok());
        assert_eq!(store.load().len(), 3);
        // 按 bot 过滤
        assert_eq!(store.load_for("feishu").len(), 2);
        assert_eq!(store.load_for("dingtalk").len(), 1);
        // 取消登记：只删指定条目，群保留（登记表层面没有群概念，就是删登记）
        assert!(store.remove("feishu", "oc_1"));
        assert!(!store.remove("feishu", "oc_1"), "重复删除应返回 false");
        assert_eq!(store.load().len(), 2);
    }

    #[test]
    fn store_rejects_duplicate_chat_or_role() {
        let (_d, store) = tmp_store("dup");
        assert!(store.add(vb("feishu", "oc_1", "后端开发")).is_ok());
        // 同 chat_id 重复登记 → 拒绝
        assert!(store.add(vb("feishu", "oc_1", "产品经理")).is_err());
        // 同角色名（不同群）→ 拒绝：群名=角色名，重名无法区分
        assert!(store.add(vb("feishu", "oc_2", "后端开发")).is_err());
        // 不同 bot 同名角色 → 允许（隔离）
        assert!(store.add(vb("dingtalk", "cid2", "后端开发")).is_ok());
    }

    #[test]
    fn store_resolve_scoped_by_bot() {
        let (_d, store) = tmp_store("resolve");
        store.add(vb("feishu", "oc_1", "后端开发")).unwrap();
        store.add(vb("dingtalk", "cid1", "后端开发")).unwrap();
        assert_eq!(
            store.resolve("feishu", "后端开发"),
            Some("oc_1".to_string())
        );
        assert_eq!(
            store.resolve("dingtalk", "后端开发"),
            Some("cid1".to_string())
        );
        assert_eq!(store.resolve("feishu", "不存在"), None);
        // 角色列表（寻址失败提示用）
        assert_eq!(store.roles_for("feishu"), vec!["后端开发".to_string()]);
        assert_eq!(store.roles_for("wechat"), Vec::<String>::new());
    }

    #[test]
    fn store_update_role_keeps_chat_id() {
        let (_d, store) = tmp_store("update");
        store.add(vb("feishu", "oc_1", "后端开发")).unwrap();
        store.add(vb("feishu", "oc_2", "前端开发")).unwrap();
        assert!(store.update_role("feishu", "oc_1", "后端开发·主").is_ok());
        // 改名与其它登记撞名 → 拒绝
        assert!(store.update_role("feishu", "oc_1", "前端开发").is_err());
        // 未登记 chat_id → 报错
        assert!(store.update_role("feishu", "oc_99", "任意").is_err());
        assert_eq!(
            store.resolve("feishu", "后端开发·主"),
            Some("oc_1".to_string())
        );
        assert_eq!(store.resolve("feishu", "后端开发"), None);
    }

    #[test]
    fn store_missing_file_loads_empty() {
        let dir = std::env::temp_dir().join(format!(
            "abb-virtualbot-missing-{}",
            crate::chrono_lite::unix_secs()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let store = VirtualBotStore::new_at(dir.join("nope.json"));
        assert!(store.load().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtin_templates_meet_platform_limits() {
        let t = builtin_templates();
        assert!(t.len() >= 10, "内置模板应 ≥10 个，实际 {}", t.len());
        for item in &t {
            assert!(!item.name.is_empty(), "模板名不能为空");
            assert!(
                item.name.chars().count() <= ROLE_NAME_MAX,
                "模板名超长（>{}）: {}",
                ROLE_NAME_MAX,
                item.name
            );
            assert!(!item.prompt.is_empty(), "模板提示词不能为空: {}", item.name);
            assert!(
                item.prompt.chars().count() <= ROLE_PROMPT_MAX,
                "模板提示词超长（>{}）: {}（{} 字符）",
                ROLE_PROMPT_MAX,
                item.name,
                item.prompt.chars().count()
            );
        }
        // 内置模板名不重复（模板列表勾选/校验依赖唯一名）
        let mut names: Vec<String> = t.iter().map(|x| x.name.clone()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), t.len(), "内置模板名应唯一");
    }

    #[test]
    fn role_block_format() {
        assert_eq!(
            role_block("后端开发", "你是后端工程师。"),
            "[群角色]\n群名：后端开发\n群介绍：你是后端工程师。\n\n"
        );
        assert_eq!(role_block("运营", ""), "[群角色]\n群名：运营\n群介绍：\n\n");
    }

    /// 假 messenger：get_chat_info 可编程（返回固定资料或失败），记录调用次数。
    struct FakeMsgr {
        info: std::sync::Mutex<Option<(String, String)>>,
        calls: AtomicUsize,
    }
    impl FakeMsgr {
        fn new(info: Option<(String, String)>) -> FakeMsgr {
            FakeMsgr {
                info: std::sync::Mutex::new(info),
                calls: AtomicUsize::new(0),
            }
        }
    }
    #[async_trait]
    impl crate::messenger::Messenger for FakeMsgr {
        async fn send_text(&self, _chat_id: &str, _text: &str) -> Result<()> {
            Ok(())
        }
        async fn get_chat_info(&self, _chat_id: &str) -> Option<(String, String)> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.info.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn cache_uses_event_name_over_api_and_caches_desc() {
        let cache = ChatInfoCache::new();
        let msgr = FakeMsgr::new(Some(("群旧名".to_string(), "介绍A".to_string())));
        // 事件名更权威（可能比 API 新）
        cache.note_event_name("oc_1", "后端开发");
        let (name, desc) = cache.get("oc_1", &msgr).await.unwrap();
        assert_eq!(name, "后端开发");
        assert_eq!(desc, "介绍A");
        // 第二次：缓存命中，不再拉 API
        cache.get("oc_1", &msgr).await.unwrap();
        assert_eq!(msgr.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn cache_falls_back_to_api_name_without_event() {
        let cache = ChatInfoCache::new();
        let msgr = FakeMsgr::new(Some(("产品经理".to_string(), "介绍B".to_string())));
        let (name, desc) = cache.get("oc_2", &msgr).await.unwrap();
        assert_eq!(name, "产品经理");
        assert_eq!(desc, "介绍B");
    }

    #[tokio::test]
    async fn cache_returns_none_when_nothing_known() {
        let cache = ChatInfoCache::new();
        let msgr = FakeMsgr::new(None); // API 也查不到
        assert!(cache.get("oc_3", &msgr).await.is_none());
    }

    #[tokio::test]
    async fn cache_returns_event_name_when_api_fails() {
        let cache = ChatInfoCache::new();
        let msgr = FakeMsgr::new(None);
        cache.note_event_name("oc_4", "运营");
        let (name, desc) = cache.get("oc_4", &msgr).await.unwrap();
        assert_eq!(name, "运营");
        assert_eq!(desc, "");
    }

    #[tokio::test]
    async fn cache_refreshes_after_ttl_and_event_refresh_keeps_desc_fresh_check() {
        // TTL 过期后：事件名刷新能续期自己，但群介绍必须重新拉——否则「改群介绍
        // 即时生效」在活跃群里永不生效（这正是事件名与 API 分开记时的原因）。
        let cache = ChatInfoCache::with_ttl(Duration::from_millis(200));
        let msgr = FakeMsgr::new(Some(("后端开发".to_string(), "旧介绍".to_string())));
        cache.note_event_name("oc_5", "后端开发");
        let (_, desc) = cache.get("oc_5", &msgr).await.unwrap();
        assert_eq!(desc, "旧介绍");
        assert_eq!(msgr.calls.load(Ordering::Relaxed), 1);
        // 改平台群介绍（API 侧变化）……
        *msgr.info.lock().unwrap() = Some(("后端开发".to_string(), "新介绍".to_string()));
        // ……TTL 内事件名刷新（又来了一条群消息）→ 只续事件名
        tokio::time::sleep(Duration::from_millis(100)).await;
        cache.note_event_name("oc_5", "后端开发");
        // TTL 到期后再取：事件名与群介绍都过期 → 重新拉（拿到新介绍；事件名
        // 仍优先，此处与 API 同名所以结果一致）
        tokio::time::sleep(Duration::from_millis(250)).await;
        let (name, desc) = cache.get("oc_5", &msgr).await.unwrap();
        assert_eq!(name, "后端开发");
        assert_eq!(
            desc, "新介绍",
            "群介绍应在缓存过期后重新拉取（改群介绍即时生效）"
        );
        assert_eq!(msgr.calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn format_created_epoch_and_known_date() {
        // 纪元 = 1970-01-01 08:00（UTC+8）
        assert_eq!(format_created(0), "1970-01-01 08:00");
        // 2026-08-20T00:00Z = 2026-08-20 08:00 本地
        assert_eq!(format_created(1_787_184_000), "2026-08-20 08:00");
    }

    /// #194：确定性 UUID——同 (bot_key, chat_id) 恒同值、uuid 形态、不同群不同值、
    /// 与角色名无关（改名不变）。
    #[test]
    fn vb_uuid_is_deterministic_and_name_independent() {
        let a1 = vb_uuid("bot_a", "oc_1");
        let a2 = vb_uuid("bot_a", "oc_1");
        assert_eq!(a1, a2, "同群必须同 uuid（跨进程/重启稳定）");
        assert_ne!(vb_uuid("bot_a", "oc_2"), a1, "不同群不同 uuid");
        assert_ne!(vb_uuid("bot_b", "oc_1"), a1, "不同 bot 不同 uuid");
        // uuid 形态：8-4-4-4-12 十六进制
        let parts: Vec<&str> = a1.split('-').collect();
        assert_eq!(parts.len(), 5, "uuid 形态 8-4-4-4-12: {a1}");
        assert_eq!(
            [
                parts[0].len(),
                parts[1].len(),
                parts[2].len(),
                parts[3].len(),
                parts[4].len()
            ],
            [8, 4, 4, 4, 12],
            "uuid 段长: {a1}"
        );
        assert!(
            a1.chars().all(|c| c == '-' || c.is_ascii_hexdigit()),
            "仅十六进制字符: {a1}"
        );
    }

    /// #194：ensure_vb_dir 存量迁移——bot 级 sessions 槽位与 history 文件一次性
    /// 迁入 vb/<uuid>/，bot 级不再保留（不双写）；幂等（二次调用无变化）。
    #[test]
    fn ensure_vb_dir_migrates_legacy_sessions_and_history() {
        let base = std::env::temp_dir().join(format!("abb-vb-mig-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let bot_key = "bot_x";
        let chat = "oc_vb1";
        // 真实登记表路径写入临时登记（VirtualBotStore::new 读 crate::bridge_dir()——
        // 迁移函数经 vb_dir_for 查登记，故这里直接以vb_dir_for 依赖的登记表为
        // 桥接点：改用 HOME 注入不可行（crate::bridge_dir 固定），改测内部组合：
        // 先手工构造登记表于真实位置不可接受 → 本测试改为验证「登记存在」路径的
        // 组合件 migrate_legacy_vb_data（pub(crate) 供测试）。
        let vb_dir = base.join("workspaces").join(bot_key).join("vb").join("u1");
        std::fs::create_dir_all(vb_dir.join("history")).unwrap();
        // bot 级存量：sessions.json 有该 chat 槽位；history 有前缀匹配文件
        let bot_sessions = base.join("workspaces").join(bot_key).join("sessions.json");
        std::fs::create_dir_all(bot_sessions.parent().unwrap()).unwrap();
        std::fs::create_dir_all(base.join("workspaces").join(bot_key).join("history")).unwrap();
        std::fs::write(
            &bot_sessions,
            r#"{"oc_vb1": {"codex": {"session_id": "tid-legacy", "started": true}}}"#,
        )
        .unwrap();
        std::fs::write(
            base.join("workspaces")
                .join(bot_key)
                .join("history")
                .join(format!("{}.jsonl", crate::config::sanitize(chat))),
            "user: 旧消息\n",
        )
        .unwrap();

        crate::virtualbot::migrate_legacy_vb_data_pub(&base, bot_key, chat, &vb_dir);

        // 会话槽位迁入 vb 且 bot 级移除（不双写）
        let vb_store = crate::sessions::SessionStore::at("codex", vb_dir.join("sessions.json"));
        assert_eq!(
            vb_store.chat_entry(chat).unwrap().codex.session_id,
            "tid-legacy",
            "槽位必须迁入 vb"
        );
        let bot_store = crate::sessions::SessionStore::new("codex", bot_key);
        assert!(
            bot_store.chat_entry(chat).is_none(),
            "bot 级槽位必须移除（不双写）"
        );
        // 历史文件迁入
        assert!(
            vb_dir
                .join("history")
                .join(format!("{}.jsonl", crate::config::sanitize(chat)))
                .exists(),
            "历史文件必须迁入 vb"
        );
        assert!(!base
            .join("workspaces")
            .join(bot_key)
            .join("history")
            .join(format!("{}.jsonl", crate::config::sanitize(chat)))
            .exists());
        // 幂等：再跑一次无变化不报错
        crate::virtualbot::migrate_legacy_vb_data_pub(&base, bot_key, chat, &vb_dir);
        assert_eq!(
            vb_store.chat_entry(chat).unwrap().codex.session_id,
            "tid-legacy"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
