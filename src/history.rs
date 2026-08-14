//! per-chat 对话历史日志（#49 后端切换上下文迁移）。
//!
//! ABB 自身不维护对话内容——上下文全靠各后端私有 session（claude `--resume <uuid>` /
//! codex thread_id / pi `--session-id`），三者互不通用。切后端（改配置 → 重启）后新槽位
//! 为空，历史全丢。本模块给每个会话 key 维护一份轻量对话日志（用户轮 + 助手轮），
//! 在「新会话首轮」（新后端/会话丢失）时把最近几轮注入 prompt，让新后端接续上下文。
//!
//! - 存储：`workspaces/<bot_key>/history/<escaped_key>.jsonl`（key 含 ':' 等字符需转义，
//!   Windows 文件名安全）；每行一个 [`HistoryEntry`]，全字段 serde default 向前兼容。
//! - 写入策略：读全文件 → (mid, user) 去重（pending 重放兜底）→ 单条截断 → 超上限丢
//!   最旧 → tmp+rename 原子写。从不原地追加，文件永无半行；读取端仍容忍坏行（手工
//!   编辑损坏不 panic）。
//! - 注入闸在 bridge（串行锁内）：`!resume && marker.session_id != 当前会话`——
//!   marker 按 session_id 判定，/new、CLI reset、agent 自愈换 UUID 都自动使 marker 失效，
//!   无需额外清理路径。CLI `session reset` 不清历史（与 /new 不对称，有意：reset 后
//!   注入续命恰是「会话丢失自愈」的目标语义）。
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
}

/// 单条截断（agent::truncate 同款语义，历史是摘要非全文）。
const ENTRY_MAX: usize = 300;
/// 条数上限（唯一容量闸：50 条 × 300 字 = 上限约 1.5 万字符，单一闸完全可测；
/// 原设计草案的「条数 + 字符」双闸在 300 字截断下数学冗余，收敛为单闸）。
const ENTRIES_MAX: usize = 50;
/// 注入 prompt 的字符预算（bridge 调 inject_block 时传参，这里只作文档默认值）。
pub const INJECT_CHARS_DEFAULT: usize = 6_000;

/// 一个会话 key 的历史日志。无内存态（stateless 文件 API）：每次操作读盘→改→原子写回。
/// 同一 key 的写入点全在 bridge per-chat 串行锁内，天然串行无并发写。
pub struct History {
    path: PathBuf,
    marker_path: PathBuf,
}

impl History {
    pub fn open(bot_key: &str, key: &str) -> History {
        Self::open_in(&crate::workspace_dir(bot_key).join("history"), key)
    }

    /// 按指定目录构造（生产 history/ 子目录 / 测试 temp dir 共用）。
    fn open_in(dir: &std::path::Path, key: &str) -> History {
        let esc = escape_key(key);
        let _ = std::fs::create_dir_all(dir);
        History {
            path: dir.join(format!("{esc}.jsonl")),
            marker_path: dir.join(format!("{esc}.migrated.json")),
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
        let tmp = self
            .path
            .with_extension(format!("jsonl.tmp.{}", uuid::Uuid::new_v4()));
        let ok = std::fs::write(&tmp, text).is_ok() && {
            set_private(&tmp);
            std::fs::rename(&tmp, &self.path).is_ok()
        };
        if !ok {
            let _ = std::fs::remove_file(&tmp);
            crate::log!("[history] ⚠️ 历史写入失败 path={}", trunc_path(&self.path));
        }
        ok
    }

    fn append(&self, mid: &str, user: bool, backend: &str, text: &str) -> bool {
        let mut entries = self.read_entries();
        // (mid, user) 去重：崩溃重放 / at-least-once 下同一条不重复记录
        if entries.iter().any(|e| e.mid == mid && e.user == user) {
            return true; // 已记录视为成功
        }
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

    pub fn has_entries(&self) -> bool {
        !self.read_entries().is_empty()
    }

    /// 全量条目（端到端测试断言用；生产读取走 inject_block/has_entries）。
    #[allow(dead_code)] // 与 SessionStore::ensure_session 同款：仅测试使用
    pub fn entries(&self) -> Vec<HistoryEntry> {
        self.read_entries()
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
    pub fn set_marker(&self, session_id: &str, backend: &str) -> bool {
        let m = MigratedMarker {
            session_id: session_id.to_string(),
            backend: backend.to_string(),
            ts: crate::chrono_lite::unix_secs(),
        };
        match serde_json::to_string(&m) {
            Ok(t) => {
                let tmp = self
                    .marker_path
                    .with_extension(format!("json.tmp.{}", uuid::Uuid::new_v4()));
                let ok = std::fs::write(&tmp, t).is_ok() && {
                    set_private(&tmp);
                    std::fs::rename(&tmp, &self.marker_path).is_ok()
                };
                if !ok {
                    let _ = std::fs::remove_file(&tmp);
                    crate::log!(
                        "[history] ⚠️ 迁移标记写入失败 path={}",
                        trunc_path(&self.marker_path)
                    );
                }
                ok
            }
            Err(_) => false,
        }
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
            } else {
                let remain = max_chars.saturating_sub(used);
                if remain >= 80 {
                    let cut = crate::agent::truncate(&e.text, remain - 8);
                    window.push((e, cut));
                }
                break;
            }
        }
        if window.is_empty() {
            return (String::new(), 0);
        }
        window.reverse(); // 输出旧 → 新
        let rounds = window.iter().filter(|(e, _)| e.user).count();
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

/// 文件名转义：仅保留 [a-z0-9_-]（字母统一小写——APFS/NTFS 默认大小写不敏感，统一小写
/// 使跨平台行为一致：仅大小写不同的 key 在这些卷上本来就会坍缩为同一文件），其余字节
/// 按 %XX（大写十六进制）。':' → "%3A"。可逆且单射，无路径分隔符风险（Windows 安全）。
/// Windows 保留设备名（CON/NUL/COM1-9/LPT1-9/AUX/PRN）加 "_" 前缀防吞文件（审查 M-1）。
fn escape_key(key: &str) -> String {
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
        out.insert(0, '_');
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

/// 落盘前收紧到 0o600（unix）：对话历史与 config.json 同级的敏感工件（审查 M-2）。
#[cfg(unix)]
fn set_private(p: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn set_private(_p: &std::path::Path) {}

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
    fn single_entry_truncated() {
        let h = temp_history("trunc", "oc_x");
        let long = "长".repeat(400);
        h.append_user("m1", "claude", &long);
        assert_eq!(h.read_entries()[0].text.chars().count(), 300);
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
    fn marker_roundtrip_and_clear_removes_both() {
        let h = temp_history("marker", "oc_x");
        assert!(h.marker().is_none());
        h.append_user("m1", "claude", "问");
        assert!(h.set_marker("sid-1", "pi"));
        let m = h.marker().unwrap();
        assert_eq!(m.session_id, "sid-1");
        assert_eq!(m.backend, "pi");
        assert!(h.path.exists() && h.marker_path.exists());
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
        // Windows 保留设备名加前缀防吞文件
        assert_eq!(escape_key("CON"), "_con");
        assert_eq!(escape_key("com1"), "_com1");
        assert_eq!(escape_key("aux"), "_aux");
    }
}
