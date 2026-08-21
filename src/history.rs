//! per-chat 对话历史日志（#49 后端切换上下文迁移）。
//!
//! ABB 自身不维护对话内容——上下文全靠各后端私有 session（claude `--resume <uuid>` /
//! codex thread_id / pi `--session-id`），三者互不通用。切后端（改配置 → 重启）后新槽位
//! 为空，历史全丢。本模块给每个会话 key 维护一份轻量对话日志（用户轮 + 助手轮），
//! 在「新会话首轮」（新后端/会话丢失）时把最近几轮注入 prompt，让新后端接续上下文。
//!
//! - 存储：`workspaces/<bot_key>/history/<escaped_key>.jsonl`（key 含 ':' 等字符需转义，
//!   Windows 文件名安全）；每行一个 [`HistoryEntry`]，全字段 serde default 向前兼容。
//! - 写入策略：读全文件 → (mid, user) 去重（pending 重放兜底）→ **存储保真**（事件派生：
//!   不截断正文，只设保险上限防异常巨型单条）→ 超条数上限丢最旧 → tmp+rename 原子写。
//!   从不原地追加，文件永无半行；读取端仍容忍坏行（手工编辑损坏不 panic）。
//!   注入时的精确切分在 inject_block（预算内完整、边界截断收编）——存储保真保证
//!   预算内取到的永远是**完整内容**而非 300 字摘要（长代码块/详细背景不丢尾）。
//! - 注入闸在 bridge（串行锁内）：`!resume && marker.session_id != 当前会话`，或
//!   `resume && marker.pending && marker.session_id == 当前会话`（#54：同 sid 自愈重建
//!   或 claude 换 UUID 自愈后 pending 标记放行一次注入），或 pi 会话文件丢失/损坏
//!   （#56：pi 对不可续聊文件同 sid 静默新建，无错误可检——run 前探针直接注入）——
//!   /new、CLI reset 使 marker 失效或失配，无需额外清理路径。CLI `session reset`
//!   不清历史（与 /new 不对称，有意：reset 后注入续命恰是「会话丢失自愈」的目标语义）。
//! - 定时任务（run_job）不经 handle、不走本日志（每次全新 session 是既定设计，
//!   见 service.rs run_job 注释）——跨后端迁移只覆盖聊天轮次。
//! - IO 失败一律只 log 警告：历史是增强能力，绝不阻塞聊天主链路。

use std::path::PathBuf;

/// 单条历史：一用户轮（agent 的输入）或一助手轮（agent 的最终回复）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HistoryEntry {
    /// 用户消息 mid；助手条目复用同一 mid（一消息一回复）。
    #[serde(default)]
    pub mid: String,
    /// true = 用户轮 / false = 助手轮。
    #[serde(default)]
    pub user: bool,
    /// 产生该轮的后端名（"claude"/"codex"/"pi"）。
    #[serde(default)]
    pub backend: String,
    #[serde(default)]
    pub text: String,
    /// 记录时间（unix 秒）。
    #[serde(default)]
    pub ts: u64,
}

/// 迁移标记：历史已注入到哪个会话。按 session_id 匹配——/new/reset/自愈换 UUID 后
/// 自动失效（新会话可再次注入）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MigratedMarker {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub backend: String,
    #[serde(default)]
    pub ts: u64,
    /// #54 待注入：会话在同 sid 下被自愈重建（claude No conversation found /
    /// codex no rollout found）——重建轮没有旧上下文，标记 pending 让**下一条消息**
    /// 注入历史（此时 resume=true 但 pending 命中，闸放行）。注入成功后桥回写
    /// pending=false。旧标记文件无此字段（serde default = false）。
    #[serde(default)]
    pub pending: bool,
}

/// 单条存储保险上限（字符）：事件派生后存储层**保真不截断**（注入时按预算精确切，
/// 见 inject_block）——此上限只防异常巨型单条（恶意/意外粘贴撑爆文件），正常消息
/// 全量落盘。50 条 × 2 万字符 = 上限约 100 万字符（CJK 下字节 ×3 ≈ 3MB），
/// 极端读重写约 25-75ms，per-chat 串行下不可感知。
pub const ENTRY_MAX: usize = 20_000;
/// 条数上限：50 条封顶丢最旧（最近优先）。
const ENTRIES_MAX: usize = 50;
/// 注入 prompt 的字符预算（bridge 生产路径传参的默认值）。
/// 200K 字符 ≈ 100K tokens：覆盖绝大多数会话的**全部历史**（存储上限 50 条 × 20K
/// = 1M 字符），1M 上下文窗口下仍留 80% 给当前任务；保留上限作防御（防异常巨型
/// history 撑爆首轮）。原 6000 是摘要日志时代的残余（300 字/条 ≈ 19 轮摘要），
/// 事件派生存储保真后只够 1-2 轮全文，与「接续全部历史」脱节——用户指出后上调。
pub const INJECT_CHARS_DEFAULT: usize = 200_000;

/// 一个会话 key 的历史日志。无内存态（stateless 文件 API）：每次操作读盘→改→原子写回。
/// 同一 key 的写入点全在 bridge per-chat 串行锁内，天然串行无并发写。
pub struct History {
    path: PathBuf,
    marker_path: PathBuf,
    /// #33 已导入来源标记（`<key>.imported.json`）。
    imported_path: PathBuf,
}

impl History {
    pub fn open(bot_key: &str, key: &str) -> History {
        Self::open_in(&crate::workspace_dir(bot_key).join("history"), key)
    }

    /// 按指定目录构造（生产 history/ 子目录 / 测试 temp dir / 会话归纳清理共用）。
    pub(crate) fn open_in(dir: &std::path::Path, key: &str) -> History {
        let esc = escape_key(key);
        let _ = std::fs::create_dir_all(dir);
        History {
            path: dir.join(format!("{esc}.jsonl")),
            marker_path: dir.join(format!("{esc}.migrated.json")),
            imported_path: dir.join(format!("{esc}.imported.json")),
        }
    }

    fn read_entries(&self) -> Vec<HistoryEntry> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(_) => return Vec::new(), // 不存在 = 空历史（常态，不算失败）
        };
        let mut out = Vec::new();
        let mut warned = false;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<HistoryEntry>(line) {
                Ok(e) => out.push(e),
                Err(e) => {
                    // 坏行只警告一次：容忍手工编辑损坏，不让单行坏数据废掉整份历史
                    if !warned {
                        crate::log!(
                            "[history] ⚠️ 历史文件存在坏行已跳过 path={}: {e}",
                            trunc_path(&self.path)
                        );
                        warned = true;
                    }
                }
            }
        }
        out
    }

    /// 原子写全量条目（uuid tmp + rename，与 sessions.rs save_locked 同款）。
    /// 0o600（unix）：历史是首个全量持久累积的对话内容工件，对齐 config.json 的
    /// 敏感文件权限（审查 M-2；Windows 无对应语义，忽略）。
    fn write_entries(&self, entries: &[HistoryEntry]) -> bool {
        let mut text = String::new();
        for e in entries {
            text.push_str(&serde_json::to_string(e).unwrap_or_default());
            text.push('\n');
        }
        // uuid tmp + rename + 0o600：与 sessions.rs save_locked 同款语义，收敛为共享
        // 实现 atomic_write_sensitive（审查：第四个手写副本不再新增）。
        match crate::atomic_write_sensitive(&self.path, &text) {
            Ok(()) => true,
            Err(_) => {
                crate::log!("[history] ⚠️ 历史写入失败 path={}", trunc_path(&self.path));
                false
            }
        }
    }

    fn append(&self, mid: &str, user: bool, backend: &str, text: &str) -> bool {
        let mut entries = self.read_entries();
        // (mid, user) 去重：崩溃重放 / at-least-once 下同一条不重复记录
        if entries.iter().any(|e| e.mid == mid && e.user == user) {
            return true; // 已记录视为成功
        }
        // 事件派生：存储层保真（超保险上限才截断——异常巨型单条防撑爆文件），
        // 注入时的精确切分交给 inject_block 的预算逻辑（预算内完整、边界截断收编）。
        entries.push(HistoryEntry {
            mid: mid.to_string(),
            user,
            backend: backend.to_string(),
            text: crate::agent::truncate(text, ENTRY_MAX),
            ts: crate::chrono_lite::unix_secs(),
        });
        // 超上限丢最旧（最近优先）
        if entries.len() > ENTRIES_MAX {
            let drop = entries.len() - ENTRIES_MAX;
            entries.drain(..drop);
        }
        self.write_entries(&entries)
    }

    /// 记录一用户轮（agent 的输入：文本 + 引用 + 附件元数据由 bridge 组装）。
    pub fn append_user(&self, mid: &str, backend: &str, text: &str) -> bool {
        self.append(mid, true, backend, text)
    }

    /// 记录一助手轮（agent 的最终回复；与用户轮共用 mid）。
    pub fn append_assistant(&self, mid: &str, backend: &str, text: &str) -> bool {
        self.append(mid, false, backend, text)
    }

    /// 全量条目（端到端测试断言用；生产读取走 inject_block）。
    #[allow(dead_code)] // 与 SessionStore::ensure_session 同款：仅测试使用
    pub fn entries(&self) -> Vec<HistoryEntry> {
        self.read_entries()
    }

    /// #33 历史迁移：批量导入（后端 session 提取的消息）。**绕过 ENTRIES_MAX 裁剪**——
    /// 导入即「填充 #49 之前的老历史」，裁剪丢最旧违背目的；条数由调用方（每来源
    /// MAX_PER_SOURCE）与单条 ENTRY_MAX 保险上限约束。保留 ts（原会话时间戳），
    /// 合并后按 ts 升序（导入的更早消息排前，文件顺序 = 时间序）。
    pub fn import_entries(&self, entries: Vec<HistoryEntry>) -> bool {
        let mut all = self.read_entries();
        // (mid, user) 去重（导入 mid 唯一，防御性）
        for e in entries {
            if all.iter().any(|x| x.mid == e.mid && x.user == e.user) {
                continue;
            }
            all.push(e);
        }
        // 按 ts 升序（导入的更早消息排前，文件顺序 = 时间序）；
        // ts=0（解析失败的消息）排最后——不污染时间序首条（审查 Minor）
        all.sort_by_key(|e| (e.ts == 0, e.ts));
        self.write_entries(&all)
    }

    /// #33 已导入来源（幂等标记）：`<key>.imported.json` = {"backend:sid": true, ...}。
    pub fn imported_sources(&self) -> std::collections::HashSet<String> {
        std::fs::read_to_string(&self.imported_path)
            .ok()
            .and_then(|t| serde_json::from_str::<std::collections::HashSet<String>>(&t).ok())
            .unwrap_or_default()
    }

    /// #33 标记某来源已导入（合并写回，原子）。
    pub fn mark_imported(&self, source: &str) -> bool {
        let mut set = self.imported_sources();
        set.insert(source.to_string());
        match serde_json::to_string(&set) {
            Ok(t) => crate::atomic_write_sensitive(&self.imported_path, &t).is_ok(),
            Err(_) => false,
        }
    }

    /// 清空历史与迁移标记（/new：用户明确要求全新会话）。
    /// 任一文件「仍存在却没删掉」才算失败（半删状态会泄漏旧历史进新会话，审查 M-3）。
    pub fn clear(&self) -> bool {
        let a = std::fs::remove_file(&self.path);
        let b = std::fs::remove_file(&self.marker_path);
        let ok = (a.is_ok() || !self.path.exists()) && (b.is_ok() || !self.marker_path.exists());
        if !ok {
            crate::log!("[history] ⚠️ 清空失败 path={}", trunc_path(&self.path));
        }
        ok
    }

    pub fn marker(&self) -> Option<MigratedMarker> {
        std::fs::read_to_string(&self.marker_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
    }

    /// 记录「历史已注入到该会话」（bridge 在首轮成功后写，与 assistant 条目同点）。
    /// pending=true = 自愈重建会话，标记「下一条消息待注入」（#54）。
    pub fn set_marker(&self, session_id: &str, backend: &str, pending: bool) -> bool {
        let m = MigratedMarker {
            session_id: session_id.to_string(),
            backend: backend.to_string(),
            ts: crate::chrono_lite::unix_secs(),
            pending,
        };
        match serde_json::to_string(&m) {
            Ok(t) => match crate::atomic_write_sensitive(&self.marker_path, &t) {
                Ok(()) => true,
                Err(_) => {
                    crate::log!(
                        "[history] ⚠️ 迁移标记写入失败 path={}",
                        trunc_path(&self.marker_path)
                    );
                    false
                }
            },
            Err(_) => false,
        }
    }

    /// 该会话历史的最后活跃时间（最新一条条目的内容 ts）。空/无文件 → None。
    /// 用内容 ts 而非文件 mtime——jsonl 每行自带 ts，文件 mtime 会因原子重写而失真
    /// （会话归纳清理的过期判定依赖它，见 session_gc）。
    pub fn last_ts(&self) -> Option<u64> {
        self.read_entries().iter().map(|e| e.ts).max()
    }

    /// 组装注入 prompt 的 [历史上下文] 段。
    ///
    /// - `exclude_mid`：排除该 mid 的条目（重放场景防当前消息自我重复——它已在历史里）。
    /// - 预算从最新往旧收集，装不下的更旧条目丢弃；窗口内输出旧 → 新。
    /// - 返回 (段文本, 注入的用户轮数 N)；历史空返回 (空串, 0)。
    pub fn inject_block(&self, exclude_mid: &str, max_chars: usize) -> (String, usize) {
        let all = self.read_entries();
        let entries: Vec<&HistoryEntry> = all.iter().filter(|e| e.mid != exclude_mid).collect();
        if entries.is_empty() {
            return (String::new(), 0);
        }
        // 从最新往旧收，直到预算耗尽；最旧一条装不下时若剩余 ≥80 字符则截断收编
        let mut window: Vec<(&HistoryEntry, String)> = Vec::new();
        let mut used = 0usize;
        for e in entries.iter().rev() {
            let need = e.text.chars().count() + 8; // 「用户: 」/「助手: 」前缀 + 换行
            if used + need <= max_chars {
                window.push((e, e.text.clone()));
                used += need;
            } else if e.user {
                let remain = max_chars.saturating_sub(used);
                if remain >= 80 {
                    // 用户轮超预算：截断收编进剩余预算，收完即止（break）
                    let cut = crate::agent::truncate(&e.text, remain - 8);
                    window.push((e, cut));
                }
                break;
            } else {
                // 助手轮超预算：跳过（continue），预算流向更旧条目——否则「最新一条
                // 超长助手轮 + 孤立弹出」会得到空窗口（审查 I-1：去存储截断后长 reply
                // 可达，空注入 = 新后端零上下文）。
                continue;
            }
        }
        if window.is_empty() {
            return (String::new(), 0);
        }
        window.reverse(); // 输出旧 → 新
                          // 窗口不能以孤立的助手轮开头（其用户轮已被预算边界/条目淘汰切掉）——
                          // 模型看到「无问之答」会困惑，宁可少一轮；配合上面助手轮不截断收编，
                          // 「最新一条孤立超长助手轮」不会触发空窗（预算流向其配对的用户轮）。
                          // 审查 B1 兜底：剥光只剩孤立助手轮时（最新助手轮恰好装进预算、
                          // 配对用户轮被预算切掉——去存储截断后单轮即可跨预算边界）——
                          // 空注入 = 新后端零上下文（I-1 的症状），此时保留该轮截断收编，
                          // 宁可有答无问，不可零上下文。
        let mut stripped: Option<(&HistoryEntry, String)> = None;
        while let Some((e, _)) = window.first() {
            if e.user {
                break;
            }
            stripped = Some(window.remove(0));
        }
        if window.is_empty() {
            if let Some((e, _)) = stripped {
                let cut = crate::agent::truncate(&e.text, max_chars.saturating_sub(8));
                window.push((e, cut));
            } else {
                return (String::new(), 0);
            }
        }
        let rounds = window.iter().filter(|(e, _)| e.user).count();
        // 剥离兜底保留的孤立助手轮无配对用户轮（rounds 会数成 0 → bridge 的 n>0 闸
        // 会静默跳过注入）——计为 1 轮，确保注入发生（其文本已截断收编进预算）。
        let rounds = if rounds > 0 { rounds } else { 1 };
        let mut block = String::from(
            "[历史上下文]\n（以下是本会话切换前/丢失前的最近对话记录，供衔接背景；请基于最新消息继续）\n\n",
        );
        for (e, text) in &window {
            block.push_str(if e.user { "用户: " } else { "助手: " });
            block.push_str(text);
            block.push('\n');
        }
        block.push('\n'); // 与后续段（[引用消息]/用户文本）隔一空行
        (block, rounds)
    }
}

/// 截断超期历史（每日工作目录整理用，见 tidy）：枚举 history/ 下全部 `<escaped>.jsonl`
/// （escape_key 不产生 '.'，`*.jsonl` 精确命中 chat 文件，`.migrated.json`/`.imported.json`
/// 天然排除），删除 `ts < cutoff_ts` 的条目并原子重写；某文件全部过期则整体删除文件
/// （保留 `.migrated.json` 标记——那是会话级迁移状态，会话归纳清理才负责删它）。
/// 返回删除的条目总数。坏行保留（不主动丢数据）；IO 失败只 log 警告（历史是增强能力）。
pub fn truncate_stale(history_dir: &std::path::Path, cutoff_ts: u64) -> usize {
    let Ok(rd) = std::fs::read_dir(history_dir) else {
        return 0;
    };
    let mut removed_total = 0usize;
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_file()
            || path
                .extension()
                .map(|e| !e.eq_ignore_ascii_case("jsonl"))
                .unwrap_or(true)
        {
            continue;
        }
        // 读前快照 (mtime, size)：与桥的 append（读-改-写）并发时，直接写回会把
        // 读后新追加的行覆盖掉（lost update）。写回/删除前复核快照，变了 → 跳过
        // 本文件（宁留不删，下轮每日整理再截断）。
        let snap = std::fs::metadata(&path)
            .ok()
            .map(|m| (m.modified().ok(), m.len()));
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue, // 读不到跳过（宁留不删）
        };
        let mut kept_lines: Vec<&str> = Vec::new();
        let mut dropped = 0usize;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<HistoryEntry>(line) {
                Ok(e) if e.ts >= cutoff_ts => kept_lines.push(line),
                Ok(_) => dropped += 1,
                Err(_) => kept_lines.push(line), // 坏行保留
            }
        }
        if dropped == 0 {
            continue; // 无超期条目，不动文件
        }
        // 复核快照：读后文件被改动（append/其它写/删除）→ 放弃写回，宁留不删
        let changed = std::fs::metadata(&path)
            .ok()
            .map(|m| Some((m.modified().ok(), m.len())) != snap)
            .unwrap_or(true); // 元数据读不到 → 视为已变，保守跳过
        if changed {
            continue;
        }
        removed_total += dropped;
        if kept_lines.is_empty() {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) => {
                    removed_total -= dropped; // 删除失败 → 行还在盘上，不计数（审查修复）
                    crate::log!(
                        "[history] ⚠️ 超期截断删除失败 path={}: {e}",
                        trunc_path(&path)
                    );
                }
            }
        } else {
            let mut out = String::new();
            for l in kept_lines {
                out.push_str(l);
                out.push('\n');
            }
            if let Err(e) = crate::atomic_write_sensitive(&path, &out) {
                removed_total -= dropped; // 重写失败 → 超期行仍在，不算截断（审查修复）
                crate::log!(
                    "[history] ⚠️ 超期截断重写失败 path={}: {e}",
                    trunc_path(&path)
                );
            }
        }
    }
    removed_total
}

/// 文件名转义：仅保留 [a-z0-9_-]（字母统一小写——APFS/NTFS 默认大小写不敏感，统一小写
/// 使跨平台行为一致：仅大小写不同的 key 在这些卷上本来就会坍缩为同一文件），其余字节
/// 按 %XX（大写十六进制）。':' → "%3A"。可逆且单射，无路径分隔符风险（Windows 安全）。
/// Windows 保留设备名（CON/NUL/COM1-9/LPT1-9/AUX/PRN）加 "%5F"（'_' 的转义形态）前缀
/// 防吞文件（审查 M-1）——前缀必须取转义形态：若直接加 '_'，自然键 "_con"（'_' 在允许
/// 集内原样保留）会与保留键 "con" 坍缩到同一文件，破坏单射（#49 审查）。
pub(crate) fn escape_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for b in key.bytes() {
        if b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-' {
            out.push(b as char);
        } else if b.is_ascii_uppercase() {
            out.push((b + 32) as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    // 转义输出不含 '.'（被转成 %2E），整个名字即第一段——直接整串比对保留名
    const RESERVED: [&str; 22] = [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    if RESERVED.contains(&out.as_str()) {
        out.insert_str(0, "%5F");
    }
    out
}

fn trunc_path(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    s.chars()
        .rev()
        .take(40)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_history(name: &str, key: &str) -> History {
        let dir = std::env::temp_dir().join(format!("abb-hist-{}-{}", name, uuid::Uuid::new_v4()));
        let h = History::open_in(&dir, key);
        h.clear();
        h
    }

    #[test]
    fn append_read_roundtrip_and_escape() {
        // key 含 ':'（话题隔离形态）→ 文件名 %3A 转义
        let h = temp_history("rt", "oc_1:omt_2");
        assert!(h.path.to_string_lossy().contains("%3A"));
        assert!(h.append_user("m1", "claude", "第一轮问题"));
        assert!(h.append_assistant("m1", "claude", "第一轮回答"));
        let e = h.read_entries();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].text, "第一轮问题");
        assert!(e[0].user);
        assert_eq!(e[0].backend, "claude");
        assert!(!e[1].user);
        h.clear();
    }

    #[test]
    fn dedup_by_mid_and_role() {
        let h = temp_history("dedup", "oc_x");
        h.append_user("m1", "pi", "问题");
        h.append_user("m1", "pi", "问题（重放）"); // 同 mid 同角色 → 去重
        h.append_assistant("m1", "pi", "回答"); // 同 mid 助手轮 → 允许（一消息一回复）
        let e = h.read_entries();
        assert_eq!(e.len(), 2, "用户去重，助手另记");
        assert_eq!(e[0].text, "问题");
        h.clear();
    }

    #[test]
    fn evicts_oldest_beyond_cap() {
        let h = temp_history("cap", "oc_x");
        for i in 0..=ENTRIES_MAX as u64 {
            h.append_user(&format!("m{i}"), "claude", &format!("消息{i}"));
        }
        let e = h.read_entries();
        assert_eq!(e.len(), ENTRIES_MAX);
        assert_eq!(e[0].text, "消息1", "最旧被丢");
        assert_eq!(e.last().unwrap().text, format!("消息{ENTRIES_MAX}"));
        h.clear();
    }

    #[test]
    fn single_entry_stored_verbatim_and_capped() {
        // 事件派生：普通长消息**存储保真**（注入按预算切，存储不截断）
        let h = temp_history("trunc", "oc_x");
        let long = "长".repeat(400);
        h.append_user("m1", "claude", &long);
        assert_eq!(
            h.read_entries()[0].text.chars().count(),
            400,
            "400 字消息全量存储（原 300 截断已去除）"
        );
        // 超保险上限（异常巨型单条）才截断
        let huge = "大".repeat(ENTRY_MAX + 100);
        h.append_user("m2", "claude", &huge);
        assert_eq!(
            h.read_entries()[1].text.chars().count(),
            ENTRY_MAX,
            "超保险上限截断防撑爆文件"
        );
        h.clear();
    }

    #[test]
    fn last_ts_returns_newest_entry_ts() {
        // 会话归纳清理的过期判定：用内容 ts（非文件 mtime——原子重写会失真）
        let h = temp_history("lastts", "oc_x");
        assert_eq!(h.last_ts(), None, "空历史 → None");
        h.append_user("m1", "claude", "问");
        // append 的 ts 是写入时刻，两条之间必然递增（unix 秒）
        std::thread::sleep(std::time::Duration::from_millis(1100));
        h.append_assistant("m1", "claude", "答");
        let ts = h.last_ts().expect("有历史应有 last_ts");
        assert!(ts > 0);
        // 手工拨一条更旧的 ts（内容 ts 语义：取最大）
        let dir = std::env::temp_dir().join(format!("abb-hist-lastts2-{}", uuid::Uuid::new_v4()));
        let h2 = History::open_in(&dir, "oc_x");
        std::fs::write(
            &h2.path,
            "{\"mid\":\"a\",\"user\":true,\"backend\":\"claude\",\"text\":\"旧\",\"ts\":100}\n\
             {\"mid\":\"b\",\"user\":true,\"backend\":\"claude\",\"text\":\"新\",\"ts\":200}\n\
             {坏json\n",
        )
        .unwrap();
        assert_eq!(h2.last_ts(), Some(200), "坏行容忍，取最新 ts");
        std::fs::remove_dir_all(&dir).ok();
        h.clear();
    }

    #[test]
    fn truncate_stale_keeps_new_drops_old() {
        // 工作目录整理（tidy）：按内容 ts 截断超期行，坏行保留，全过期删文件
        let dir = std::env::temp_dir().join(format!("abb-hist-trunc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("oc_x.jsonl");
        let mk = |mid: &str, ts: u64| {
            format!("{{\"mid\":\"{mid}\",\"user\":true,\"backend\":\"claude\",\"text\":\"{mid}\",\"ts\":{ts}}}\n")
        };
        std::fs::write(
            &p,
            format!(
                "{}{}{}{}",
                mk("old1", 100),
                mk("old2", 200),
                mk("new1", 300),
                "{坏json\n"
            ),
        )
        .unwrap();
        // cutoff=250：old1/old2 删，new1 与坏行留
        assert_eq!(super::truncate_stale(&dir, 250), 2, "删除 2 条超期");
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("new1"), "新行保留");
        assert!(text.contains("坏json"), "坏行保留（不主动丢数据）");
        assert!(
            !text.contains("old1") && !text.contains("old2"),
            "超期行删除"
        );
        // 无超期 → 文件不动（截断幂等）
        assert_eq!(super::truncate_stale(&dir, 300), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncate_stale_removes_fully_stale_file_keeps_marker() {
        // 全过期 → 删 jsonl 文件，但 .migrated.json 标记保留（会话级状态归 session_gc 管）
        let dir = std::env::temp_dir().join(format!("abb-hist-trunc2-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("oc_y.jsonl");
        std::fs::write(
            &p,
            "{\"mid\":\"a\",\"user\":true,\"backend\":\"claude\",\"text\":\"旧\",\"ts\":100}\n",
        )
        .unwrap();
        std::fs::write(dir.join("oc_y.migrated.json"), r#"{"session_id":"s1"}"#).unwrap();
        assert_eq!(super::truncate_stale(&dir, 500), 1);
        assert!(!p.exists(), "全过期文件删除");
        assert!(
            dir.join("oc_y.migrated.json").exists(),
            "迁移标记保留（非本层职责）"
        );
        // 非 jsonl / 标记文件不被扫
        std::fs::write(dir.join("oc_z.imported.json"), "{}").unwrap();
        std::fs::write(dir.join("readme.txt"), "x").unwrap();
        assert_eq!(super::truncate_stale(&dir, 0), 0, "只扫 *.jsonl");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn inject_keeps_long_entry_verbatim_within_budget() {
        // 事件派生注入：预算内长条目**完整注入**（非 300 摘要）——历史接续不丢尾
        let h = temp_history("verbatim", "oc_x");
        let long = "详细背景".repeat(120); // 480 字，超旧 300 截断
        h.append_user("u1", "claude", &long);
        h.append_assistant("u1", "claude", "结论");
        let (block, n) = h.inject_block("", 6000);
        assert_eq!(n, 1, "一轮完整注入");
        assert!(
            block.contains(&long),
            "预算内完整内容（原 300 截断会丢尾）: {}",
            &block[..block.len().min(80)]
        );
        h.clear();
    }

    #[test]
    fn inject_long_latest_entry_consumes_budget() {
        // 最新条目超预算 → 预算内截断收编该条，更旧条目不注入（精确优先）
        let h = temp_history("longfirst", "oc_x");
        h.append_user("u1", "claude", "旧消息");
        let huge = "大".repeat(8000); // 超 6000 预算
        h.append_user("u2", "claude", &huge);
        let (block, n) = h.inject_block("", 6000);
        assert_eq!(n, 1, "只收最新一轮");
        assert!(block.contains("大"), "最新长条目截断收编进预算");
        assert!(!block.contains("旧消息"), "更旧条目不注入");
        assert!(block.chars().count() <= 6100, "预算约束（含前缀/换行）");
        h.clear();
    }

    #[test]
    fn inject_oversized_latest_assistant_does_not_empty_window() {
        // 审查 I-1 回归：最新一条助手轮超预算（去存储截断后长 reply 可达）——
        // 截断收编对助手轮跳过，预算流向其配对的用户轮，窗口不为空
        // （旧行为：单条孤立助手轮 → 弹出 → 空注入 = 新后端零上下文）
        let h = temp_history("longassist", "oc_x");
        h.append_user("u1", "claude", "问题");
        let huge = "答".repeat(8000); // 超 6000 预算的最新助手轮
        h.append_assistant("u1", "claude", &huge);
        let (block, n) = h.inject_block("", 6000);
        assert!(!block.is_empty(), "窗口不得为空（预算流向用户轮）");
        assert!(block.contains("问题"), "配对的用户轮注入");
        assert_eq!(n, 1);
        h.clear();
    }

    #[test]
    fn inject_just_fits_assistant_does_not_empty_window() {
        // 审查 B1：镜像方向——最新助手轮恰好装进预算（need=5998 ≤ 6000），其配对
        // 用户轮被预算切掉（remain=2 < 80 → break）→ 窗口剥成只剩孤立助手轮。
        // 旧行为剥光返回空窗（空注入 = 新后端零上下文，I-1 的症状在镜像方向仍可达）；
        // 兜底：保留该轮截断收编，宁可有答无问，不可零上下文。
        let h = temp_history("justfits", "oc_x");
        h.append_user("u1", "claude", "问题");
        let a = "答".repeat(5990); // need = 5990+8 = 5998 ≤ 6000 恰好装下；remain=2 < 80
        h.append_assistant("u1", "claude", &a);
        let (block, n) = h.inject_block("", 6000);
        assert!(!block.is_empty(), "窗口不得为空（B1 空注入回归）");
        assert!(block.contains("答"), "孤立助手轮截断收编");
        assert_eq!(n, 1, "计为 1 轮，bridge 的 n>0 闸放行注入");
        h.clear();
    }

    #[test]
    fn inject_block_order_budget_exclude() {
        let h = temp_history("inject", "oc_x");
        h.append_user("u1", "claude", "问一");
        h.append_assistant("u1", "claude", "答一");
        h.append_user("u2", "claude", "问二");
        // 全量注入：旧 → 新，N=用户轮数
        let (block, n) = h.inject_block("", 6000);
        assert_eq!(n, 2);
        assert!(block.contains("[历史上下文]"));
        let p1 = block.find("问一").unwrap();
        let p2 = block.find("答一").unwrap();
        let p3 = block.find("问二").unwrap();
        assert!(p1 < p2 && p2 < p3, "输出旧→新");
        assert!(block.contains("用户: 问一") && block.contains("助手: 答一"));
        // exclude_mid：重放防自我重复
        let (b2, _) = h.inject_block("u2", 6000);
        assert!(b2.contains("问一") && !b2.contains("问二"), "排除当前 mid");
        // 预算：只装得下最新的
        let (b3, n3) = h.inject_block("", 20);
        assert!(
            b3.contains("问二") && !b3.contains("问一"),
            "预算从新往旧收"
        );
        assert_eq!(n3, 1);
        h.clear();
    }

    #[test]
    fn import_entries_keeps_ts_and_bypasses_cap() {
        // #33：导入保留原时间戳 + 绕过 50 条裁剪（填充老历史，裁剪丢最旧违背目的）
        let h = temp_history("import", "oc_x");
        // 预置 50 条（触顶）
        for i in 0..50 {
            h.append_user(&format!("m{i}"), "claude", &format!("消息{i}"));
        }
        assert_eq!(h.entries().len(), 50);
        // 导入 60 条更早的消息（ts=1000 起）→ 全部保留（不被 50 条裁剪丢最旧）
        let imported: Vec<HistoryEntry> = (0..60)
            .map(|i| HistoryEntry {
                mid: format!("imp-test-{i}"),
                user: true,
                backend: "claude".into(),
                text: format!("老消息{i}"),
                ts: 1000 + i,
            })
            .collect();
        assert!(h.import_entries(imported));
        let all = h.entries();
        assert_eq!(all.len(), 110, "导入绕过 50 条裁剪");
        assert_eq!(all[0].ts, 1000, "保留原时间戳");
        assert!(all[0].text.starts_with("老消息"));
        h.clear();
    }

    #[test]
    fn imported_sources_roundtrip_and_idempotent() {
        // #33：幂等标记往返
        let h = temp_history("imported", "oc_x");
        assert!(h.imported_sources().is_empty());
        assert!(h.mark_imported("claude:abc123"));
        assert!(h.mark_imported("pi:def456"));
        let set = h.imported_sources();
        assert!(set.contains("claude:abc123") && set.contains("pi:def456"));
        // 合并写回：不覆盖已有
        assert!(h.mark_imported("codex:xyz"));
        let set2 = h.imported_sources();
        assert_eq!(set2.len(), 3);
        h.clear();
    }

    #[test]
    fn marker_roundtrip_and_clear_removes_both() {
        let h = temp_history("marker", "oc_x");
        assert!(h.marker().is_none());
        h.append_user("m1", "claude", "问");
        assert!(h.set_marker("sid-1", "pi", false));
        let m = h.marker().unwrap();
        assert_eq!(m.session_id, "sid-1");
        assert_eq!(m.backend, "pi");
        assert!(!m.pending, "普通注入轮标记非 pending");
        // #54：pending 往返（自愈重建轮标记）
        assert!(h.set_marker("sid-1", "pi", true));
        assert!(h.marker().unwrap().pending);
        // 旧标记文件无 pending 字段 → serde default = false（向前兼容）
        std::fs::write(
            &h.marker_path,
            r#"{"session_id":"old","backend":"claude","ts":1}"#,
        )
        .unwrap();
        assert!(!h.marker().unwrap().pending);
        h.clear();
        assert!(h.marker().is_none());
        assert!(!h.path.exists() && !h.marker_path.exists());
    }

    #[test]
    fn corrupt_line_tolerated() {
        let h = temp_history("corrupt", "oc_x");
        h.append_user("m1", "claude", "好行1");
        h.append_user("m2", "claude", "好行2");
        // 手工插一行坏数据
        let text = std::fs::read_to_string(&h.path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let poisoned = format!("{}\n{{坏json\n{}\n", lines[0], lines[1]);
        std::fs::write(&h.path, poisoned).unwrap();
        let e = h.read_entries();
        assert_eq!(e.len(), 2, "坏行跳过不 panic，其余完整");
        h.clear();
    }

    #[test]
    fn escape_key_unique_and_safe() {
        assert_eq!(escape_key("oc_1:omt_2"), "oc_1%3Aomt_2");
        assert_ne!(escape_key("a:b"), escape_key("ab"), "单射：不同 key 不同名");
        assert_eq!(escape_key("ab/cd"), "ab%2Fcd", "'/' 被转义无路径风险");
        assert!(
            escape_key("..").starts_with("%2E"),
            "'.' 也转义（防 .. 穿越）"
        );
        // 大小写折叠：大小写不敏感卷（APFS/NTFS）行为跨平台一致（审查 M-1）
        assert_eq!(escape_key("OC_x"), "oc_x");
        // Windows 保留设备名加转义前缀防吞文件（%5F = '_' 的转义形态，不破坏单射）
        assert_eq!(escape_key("CON"), "%5Fcon");
        assert_eq!(escape_key("com1"), "%5Fcom1");
        assert_eq!(escape_key("aux"), "%5Faux");
        // 单射：保留名前缀不与自然键冲突（"_con" 的 '_' 原样保留，二者必须不同文件）
        assert_ne!(
            escape_key("con"),
            escape_key("_con"),
            "保留键与自然键不坍缩"
        );
        assert_ne!(escape_key("aux"), escape_key("_aux"));
        assert_ne!(escape_key("CON"), escape_key("_con"));
    }

    #[test]
    fn inject_window_never_starts_with_assistant() {
        let h = temp_history("orphan", "oc_x");
        // u1 超长（600 字存储保真）、a1 中长，u2/a2 短。预算 220：装得下 a2+u2+a1
        // （10+10+188），u1（608）装不下且剩余 <80 走 break → 窗口（旧→新）原会以
        // 孤立的「助手: 答一」开头（其用户轮被预算切掉）——fix 后应弹出 a1。
        h.append_user("u1", "claude", &"问一".repeat(300));
        h.append_assistant("u1", "claude", &"答一".repeat(90));
        h.append_user("u2", "claude", "问二");
        h.append_assistant("u2", "claude", "答二");
        let (block, n) = h.inject_block("", 220);
        assert!(
            !block.starts_with("助手: "),
            "窗口不以孤立助手轮开头: {block}"
        );
        assert!(!block.contains("答一"), "孤立助手轮被弹出: {block}");
        assert!(block.contains("问二"), "较新的用户轮保留: {block}");
        assert!(block.contains("答二"), "配对助手轮保留: {block}");
        assert_eq!(n, 1, "N=窗口内用户轮数");
        h.clear();
    }
}
