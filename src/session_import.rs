//! 历史会话迁移（#33）：把后端私有 session 文件（claude jsonl / codex rollout /
//! pi session / prime session）里的对话一次性导入 ABB 自己的 history.rs。
//!
//! #49 上线时明确「不做后端 session 格式迁移」——history.rs 从 #49 起从零记录，
//! **切换前聊过的对话只存在于后端私有位置**，切后端/会话丢失时注入的历史不含它们。
//! 本模块补做一次性迁移：枚举 sessions.json 每个 chat 的已 started 槽位 → 定位
//! session 文件 → 提取 user/assistant 纯文本 → 导入对应 chat 的 history。
//!
//! - 幂等：`history/<key>.imported.json` 记录来源键（`backend:sid`），重跑跳过已导入
//! - 去重：该 chat 的 history 非空 → 只导入 `ts < history 首条 ts` 的消息（#49 后的
//!   轮次两端都有，防重复注入）
//! - 条数：导入绕过 history 的 50 条裁剪（填充老历史，裁剪丢最旧违背目的）；
//!   每来源 200 条上限 + 单条 20k 保险（最坏 4MB/来源）
//! - 提取范围：只取 user/assistant 最终文本（thinking/工具调用/系统消息有意排除）；
//!   prime-agent 无真实数据实证，按 pi 同构推断 + 解析失败跳过容错

use crate::history::HistoryEntry;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 一条待导入的消息（已提取的纯文本 + 原时间戳）。
pub struct Msg {
    pub user: bool,
    pub text: String,
    pub ts: u64,
}

/// 单个 chat 的导入结果。
pub struct ChatReport {
    pub chat: String,
    pub imported: usize,
    /// 跳过原因（来源: 原因），逐条记录供用户核对。
    pub skipped: Vec<String>,
}

/// 一个 bot 的导入汇总。
#[derive(Default)]
pub struct ImportReport {
    pub chats: Vec<ChatReport>,
    pub total: usize,
}

/// 每来源导入条数上限（防异常大文件撑爆 history）。
const MAX_PER_SOURCE: usize = 200;

/// 解析 RFC3339 UTC 时间戳（"YYYY-MM-DDTHH:MM:SS[.fff]Z"）→ unix 秒。
/// 仅接受 Z 结尾（四后端落盘均为 UTC Z）；带偏移/非法输入返回 None。
pub fn parse_rfc3339(s: &str) -> Option<u64> {
    let t = s.strip_suffix('Z')?;
    let (date, time) = t.split_once('T')?;
    let mut d = date.split('-');
    let y: u64 = d.next()?.parse().ok()?;
    let mo: u64 = d.next()?.parse().ok()?;
    let day: u64 = d.next()?.parse().ok()?;
    let mut tm = time.split(':');
    let h: u64 = tm.next()?.parse().ok()?;
    let mi: u64 = tm.next()?.parse().ok()?;
    let sec: u64 = tm.next()?.split('.').next()?.parse().ok()?;
    if !(1..=12).contains(&mo) || day == 0 || day > 31 || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let days = days_from_civil(y, mo, day);
    Some(days * 86400 + h * 3600 + mi * 60 + sec)
}

/// Howard Hinnant 的 civil → days 算法（与 chrono_lite::epoch_to_ymd 互逆）。
fn days_from_civil(y: u64, m: u64, d: u64) -> u64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719_468
}

/// 提取 JSON 文本块（content 数组里 type=="text" 的 text，换行连接）。
fn json_text_blocks(content: Option<&Value>) -> String {
    let Some(arr) = content.and_then(|c| c.as_array()) else {
        return String::new();
    };
    arr.iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// claude jsonl：`type=="user"`（content 为 string）= 用户；`type=="assistant"`
/// text blocks = 助手（**注意**：真实文件中同一 message.id 通常只出现一次（每事件
/// 一个 block），跨工具轮的多条 assistant 事件是不同 message.id——「覆盖」只对
/// 罕见的多事件同 id 生效，轮中解说文本会被如实导入（行为无害，注释如实）；
/// `isSidechain:true` 排除；`queue-operation`/`mode` 等独立 type 天然排除。
fn extract_claude(path: &Path) -> Result<Vec<Msg>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("读取失败: {e}"))?;
    let mut msgs: Vec<Msg> = Vec::new();
    let mut pending: Option<(String, u64)> = None; // 待配对用户轮的助手回复
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        }; // 坏行跳过
        if v.get("isSidechain")
            .and_then(|b| b.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        let ts = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(parse_rfc3339)
            .unwrap_or(0);
        match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "user" => {
                if let Some((t, tts)) = pending.take() {
                    if !t.is_empty() {
                        msgs.push(Msg {
                            user: false,
                            text: t,
                            ts: tts,
                        });
                    }
                }
                // 用户文本：content 是 string（array 是 tool_result 等，跳过）
                let content = v
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_str())
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                if !content.is_empty() {
                    msgs.push(Msg {
                        user: true,
                        text: content,
                        ts,
                    });
                }
            }
            "assistant" => {
                let content = v.get("message").and_then(|m| m.get("content"));
                let blocks = json_text_blocks(content);
                if !blocks.is_empty() {
                    // 覆盖：同一 message.id 拆成 thinking→text→tool_use 多事件，
                    // 最后一个有 text 的 assistant 事件 = 该轮的最终回复
                    pending = Some((blocks, ts));
                }
            }
            _ => {}
        }
    }
    if let Some((t, tts)) = pending.take() {
        if !t.is_empty() {
            msgs.push(Msg {
                user: false,
                text: t,
                ts: tts,
            });
        }
    }
    Ok(msgs)
}

/// codex rollout：`response_item.payload.type=="message"`；
/// user → `input_text` 块；assistant → `phase=="final_answer"` 的 `output_text` 块
/// （commentary 轮中解说跳过）；developer role 跳过；首条 `# AGENTS.md` 前缀
/// 的自动注入 user 消息过滤。
fn extract_codex(path: &Path) -> Result<Vec<Msg>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("读取失败: {e}"))?;
    let mut msgs: Vec<Msg> = Vec::new();
    let mut first_user_skipped = false;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("response_item") {
            continue;
        }
        let Some(p) = v.get("payload") else { continue };
        if p.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        let ts = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(parse_rfc3339)
            .unwrap_or(0);
        let role = p.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let blocks = |want: &str| -> String {
            p.get("content")
                .and_then(|c| c.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some(want))
                        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default()
        };
        match role {
            "user" => {
                let t = blocks("input_text");
                if t.is_empty() {
                    continue;
                }
                if !first_user_skipped && t.starts_with("# AGENTS.md") {
                    first_user_skipped = true; // codex 自动注入的指引，非真实用户消息
                    continue;
                }
                msgs.push(Msg {
                    user: true,
                    text: t,
                    ts,
                });
            }
            "assistant" => {
                // 只取最终回复（phase=="final_answer"）；commentary 与
                // agent_message 重复，跳过
                if p.get("phase").and_then(|t| t.as_str()) != Some("final_answer") {
                    continue;
                }
                let t = blocks("output_text");
                if !t.is_empty() {
                    msgs.push(Msg {
                        user: false,
                        text: t,
                        ts,
                    });
                }
            }
            _ => {} // developer（注入指令）/ 其它跳过
        }
    }
    Ok(msgs)
}

/// pi/prime session：`type=="message"` + role user/assistant + text blocks；
/// assistant 用 `stopReason=="stop"` 筛最终回复（toolUse 中途轮跳过）；
/// `toolResult` role 天然排除。prime 与 pi 同构（agent.rs 测试固定），
/// 差异仅目录/文件名定位（调用方处理）。
fn extract_pi(path: &Path) -> Result<Vec<Msg>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("读取失败: {e}"))?;
    let mut msgs: Vec<Msg> = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        let Some(m) = v.get("message") else { continue };
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let text_val = json_text_blocks(m.get("content"));
        let ts = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(parse_rfc3339)
            .or_else(|| {
                m.get("timestamp")
                    .and_then(|t| t.as_u64())
                    .map(|ms| ms / 1000)
            })
            .unwrap_or(0);
        match role {
            "user" => {
                if !text_val.is_empty() {
                    msgs.push(Msg {
                        user: true,
                        text: text_val,
                        ts,
                    });
                }
            }
            "assistant" => {
                let stop = m.get("stopReason").and_then(|s| s.as_str()).unwrap_or("");
                if stop == "stop" && !text_val.is_empty() {
                    msgs.push(Msg {
                        user: false,
                        text: text_val,
                        ts,
                    });
                }
            }
            _ => {} // toolResult 等
        }
    }
    Ok(msgs)
}

/// 定位某后端的 session 文件（按 sid）。
fn locate_session_file(backend: &str, workspace: &Path, sid: &str) -> Option<PathBuf> {
    match backend {
        "claude" => {
            // 按 sid 全盘搜：项目目录名（CJK 坍缩）不可重建，`~/.claude/projects/*/<sid>.jsonl`
            let home = dirs::home_dir()?;
            let projects = home.join(".claude/projects");
            let rd = std::fs::read_dir(&projects).ok()?;
            for ent in rd.flatten() {
                let p = ent.path().join(format!("{sid}.jsonl"));
                if p.is_file() {
                    return Some(p);
                }
            }
            None
        }
        "codex" => {
            let home = dirs::home_dir()?;
            // 活跃会话：~/.codex/sessions/<y>/<m>/<d>/rollout-<ts>-<tid>.jsonl
            let sessions = home.join(".codex/sessions");
            if let Ok(rd) = std::fs::read_dir(&sessions) {
                for y in rd.flatten() {
                    let Ok(rm) = std::fs::read_dir(y.path()) else {
                        continue;
                    };
                    for m in rm.flatten() {
                        let Ok(rd2) = std::fs::read_dir(m.path()) else {
                            continue;
                        };
                        for d in rd2.flatten() {
                            let Ok(rf) = std::fs::read_dir(d.path()) else {
                                continue;
                            };
                            for f in rf.flatten() {
                                let name = f.file_name().to_string_lossy().into_owned();
                                if name.starts_with("rollout-") && name.contains(sid) {
                                    return Some(f.path());
                                }
                            }
                        }
                    }
                }
            }
            // 归档：~/.codex/archived_sessions/rollout-*.jsonl
            let archived = home.join(".codex/archived_sessions");
            if let Ok(rd) = std::fs::read_dir(archived) {
                for f in rd.flatten() {
                    let name = f.file_name().to_string_lossy().into_owned();
                    if name.contains(sid) {
                        return Some(f.path());
                    }
                }
            }
            None
        }
        "pi" => {
            // workspace/.pi-sessions/<ts>_<sid>.jsonl（文件名含 sid）
            let dir = workspace.join(".pi-sessions");
            let rd = std::fs::read_dir(dir).ok()?;
            for f in rd.flatten() {
                if f.file_name().to_string_lossy().contains(sid) {
                    return Some(f.path());
                }
            }
            None
        }
        "prime-agent" => {
            // workspace/.prime-sessions/*.jsonl：文件名是 ULID 不含 sid，按首行 id 扫描
            let dir = workspace.join(".prime-sessions");
            let rd = std::fs::read_dir(dir).ok()?;
            for f in rd.flatten() {
                if crate::agent::session_file_id(&f.path()).as_deref() == Some(sid) {
                    return Some(f.path());
                }
            }
            None
        }
        _ => None,
    }
}

fn extract_for(backend: &str, path: &Path) -> Result<Vec<Msg>, String> {
    match backend {
        "claude" => extract_claude(path),
        "codex" => extract_codex(path),
        "pi" | "prime-agent" => extract_pi(path),
        _ => Err(format!("未知后端 {backend}")),
    }
}

/// 导入某 bot 的全部 chat 历史（幂等：已导入来源跳过）。
/// dry_run = 只统计不写入。
pub fn import_bot(bot_key: &str, dry_run: bool) -> ImportReport {
    let ws = crate::workspace_dir(bot_key);
    let sessions_path = ws.join("sessions.json");
    let data: HashMap<String, crate::sessions::ChatEntry> = std::fs::read_to_string(&sessions_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let mut report = ImportReport::default();
    for (chat, entry) in data {
        let mut cr = ChatReport {
            chat: chat.clone(),
            imported: 0,
            skipped: Vec::new(),
        };
        // 审查 C1 修复：每 chat 在槽位循环**前**计算一次去重基线（原实现在每个来源内
        // 重读 history 首条——claude 先导入后截点被其最早消息占据，后续 pi 来源全部
        // 「无更早消息」误丢且不 mark，重跑复现同一跳过 = 永久丢失）。
        // 去重改为**内容级**（(user, text) 匹配，跨 ts 宽容）：同文本不重复导入——
        // 既防 #49 后两端重叠（同轮次同文本），又允许已丢来源（C1 事故现场：pi 内容
        // 与 history 零重叠）在重跑时无损恢复；同文本不同轮次的重复提问去重为一条，
        // 注入场景可接受（重复文本无接续价值）。
        let hist = crate::history::History::open(bot_key, &chat);
        let existing: std::collections::HashSet<(bool, String)> = hist
            .entries()
            .iter()
            .map(|e| (e.user, e.text.clone()))
            .collect();
        let slots = [
            ("claude", &entry.claude),
            ("codex", &entry.codex),
            ("pi", &entry.pi),
            ("prime-agent", &entry.prime_agent),
        ];
        for (backend, slot) in slots {
            if !slot.started || slot.session_id.is_empty() {
                continue; // 占位 UUID（未开首轮）无文件
            }
            let source = format!("{backend}:{}", slot.session_id);
            if hist.imported_sources().contains(&source) {
                continue; // 已导入（幂等）
            }
            let Some(path) = locate_session_file(backend, &ws, &slot.session_id) else {
                cr.skipped.push(format!(
                    "{backend}: 会话文件未找到（sid {}）",
                    &slot.session_id[..slot.session_id.len().min(8)]
                ));
                continue;
            };
            let msgs = match extract_for(backend, &path) {
                Ok(m) => m,
                Err(e) => {
                    cr.skipped.push(format!("{backend}: 解析失败 {e}"));
                    continue;
                }
            };
            if msgs.is_empty() {
                cr.skipped.push(format!("{backend}: 无可提取消息"));
                continue;
            }
            // 内容级去重：跳过与现有 history 同 (user, text) 的消息（#49 后重叠防重复；
            // ts 宽容——history 条目 ts 是实时写入秒、后端原 ts 是 ISO 秒，可能差 1 秒）
            let fresh: Vec<&Msg> = msgs
                .iter()
                .filter(|m| !existing.contains(&(m.user, m.text.clone())))
                .collect();
            if fresh.is_empty() {
                cr.skipped
                    .push(format!("{backend}: 内容与现有历史全部重复"));
                continue;
            }
            // 审查 I1：取**最新** MAX_PER_SOURCE 条（靠近 #49 边界的内容对注入接续
            // 更有价值；原 take 取最旧 200，丢的是最新端）
            let take = fresh.iter().rev().take(MAX_PER_SOURCE).count();
            if dry_run {
                cr.imported += take;
                continue;
            }
            let entries: Vec<HistoryEntry> = fresh
                .iter()
                .rev()
                .take(MAX_PER_SOURCE)
                .enumerate()
                .map(|(i, m)| HistoryEntry {
                    mid: format!("imp-{backend}-{i}"),
                    user: m.user,
                    backend: backend.to_string(),
                    text: crate::agent::truncate(&m.text, crate::history::ENTRY_MAX),
                    ts: m.ts,
                })
                .collect();
            if hist.import_entries(entries) {
                hist.mark_imported(&source);
                cr.imported += take;
            } else {
                cr.skipped.push(format!("{backend}: 写入失败"));
            }
        }
        if cr.imported > 0 || !cr.skipped.is_empty() {
            report.total += cr.imported;
            report.chats.push(cr);
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rfc3339_matrix() {
        // 可靠锚点：2026-01-01T00:00:00Z（2024-01-01 = 1704067200 起推算）
        assert_eq!(parse_rfc3339("2026-01-01T00:00:00Z"), Some(1767225600));
        assert_eq!(
            parse_rfc3339("2026-01-01T00:00:01Z"),
            Some(1767225601),
            "+1 秒"
        );
        assert_eq!(
            parse_rfc3339("2026-01-01T00:00:00.992Z"),
            Some(1767225600),
            "毫秒忽略"
        );
        assert_eq!(
            parse_rfc3339("2026-01-02T00:00:00Z"),
            Some(1767312000),
            "+1 天"
        );
        assert_eq!(
            parse_rfc3339("2026-08-12T02:46:15+08:00"),
            None,
            "非 Z 拒绝"
        );
        assert_eq!(parse_rfc3339("garbage"), None);
        assert_eq!(parse_rfc3339("2026-13-01T00:00:00Z"), None, "非法月份");
        assert_eq!(parse_rfc3339("2026-00-01T00:00:00Z"), None, "非法月份 0");
    }

    fn write_fixture(name: &str, lines: &[&str]) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("abb-sessimp-{name}-{}.jsonl", uuid::Uuid::new_v4()));
        std::fs::write(&p, lines.join("\n")).unwrap();
        p
    }

    #[test]
    fn extract_claude_filters_and_pairs() {
        let p = write_fixture(
            "claude",
            &[
                r#"{"type":"mode","mode":"normal","timestamp":"2026-08-01T01:00:00Z"}"#,
                r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":"你好"},"timestamp":"2026-08-01T01:01:00Z"}"#,
                // 同 message.id 多事件：thinking → text（解说）→ tool_use → text（最终）
                r#"{"type":"assistant","isSidechain":false,"message":{"id":"msg_1","role":"assistant","content":[{"type":"thinking","thinking":"..."}]},"timestamp":"2026-08-01T01:01:05Z"}"#,
                r#"{"type":"assistant","isSidechain":false,"message":{"id":"msg_1","role":"assistant","content":[{"type":"text","text":"我先看看"}]},"timestamp":"2026-08-01T01:01:06Z"}"#,
                r#"{"type":"assistant","isSidechain":false,"message":{"id":"msg_1","role":"assistant","content":[{"type":"tool_use","name":"Bash"}]},"timestamp":"2026-08-01T01:01:07Z"}"#,
                r#"{"type":"assistant","isSidechain":false,"message":{"id":"msg_1","role":"assistant","content":[{"type":"text","text":"最终结论"}]},"timestamp":"2026-08-01T01:01:08Z"}"#,
                // tool_result 用户事件（content array）跳过
                r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]},"timestamp":"2026-08-01T01:01:09Z"}"#,
                // subagent 侧链排除
                r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"子任务"},"timestamp":"2026-08-01T01:02:00Z"}"#,
                // ABB 注入的 queue-operation 独立 type 排除
                r#"{"type":"queue-operation","operation":"enqueue","content":"[引用消息]","timestamp":"2026-08-01T01:03:00Z"}"#,
                r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":"第二个问题"},"timestamp":"2026-08-01T01:04:00Z"}"#,
                r#"{"type":"assistant","isSidechain":false,"message":{"id":"msg_2","role":"assistant","content":[{"type":"text","text":"第二个答案"}]},"timestamp":"2026-08-01T01:04:10Z"}"#,
            ],
        );
        let msgs = extract_claude(&p).unwrap();
        assert_eq!(msgs.len(), 4, "两个用户轮 + 两个最终回复");
        assert!(msgs[0].user && msgs[0].text == "你好");
        assert!(
            !msgs[1].user && msgs[1].text == "最终结论",
            "覆盖策略取最终回复: {}",
            msgs[1].text
        );
        assert!(msgs[2].user && msgs[2].text == "第二个问题");
        assert!(!msgs[3].user && msgs[3].text == "第二个答案");
        assert_eq!(
            msgs[1].ts,
            parse_rfc3339("2026-08-01T01:01:08Z").unwrap(),
            "保留原时间戳"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn extract_codex_filters_final_answer_and_agents_md() {
        let p = write_fixture(
            "codex",
            &[
                r#"{"timestamp":"2026-07-20T14:59:02Z","type":"session_meta","payload":{"session_id":"t1"}}"#,
                // codex 自动注入的 AGENTS.md 指引（首条 user，须过滤）
                // （r## 定界：内容含 "# 序列）
                r##"{"timestamp":"2026-07-20T14:59:03Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions\n..."}]}}"##,
                r#"{"timestamp":"2026-07-20T15:00:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"真实问题"}]}}"#,
                // commentary 解说跳过
                r#"{"timestamp":"2026-07-20T15:00:10Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"先分析"}],"phase":"commentary"}}"#,
                // 工具调用
                r#"{"timestamp":"2026-07-20T15:00:11Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec"}}"#,
                // 最终回复
                r#"{"timestamp":"2026-07-20T15:00:30Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"最终答案"}],"phase":"final_answer"}}"#,
                // developer 注入跳过
                r#"{"timestamp":"2026-07-20T15:00:31Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"AGENTS 指令"}]}}"#,
            ],
        );
        let msgs = extract_codex(&p).unwrap();
        assert_eq!(msgs.len(), 2, "一个真实用户轮 + 一个最终回复");
        assert!(msgs[0].user && msgs[0].text == "真实问题");
        assert!(!msgs[1].user && msgs[1].text == "最终答案");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn extract_pi_stop_reason_and_toolresult() {
        let p = write_fixture(
            "pi",
            &[
                r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-08-14T14:07:54Z"}"#,
                r#"{"type":"message","id":"m1","timestamp":"2026-08-14T14:08:00Z","message":{"role":"user","content":[{"type":"text","text":"你是谁"}],"timestamp":1786766880000}}"#,
                // 中途轮（toolUse）跳过
                r#"{"type":"message","id":"m2","timestamp":"2026-08-14T14:08:05Z","message":{"role":"assistant","content":[{"type":"text","text":"让我查"},{"type":"toolCall","toolCallId":"c1"}],"stopReason":"toolUse"}}"#,
                // 工具结果跳过
                r#"{"type":"message","id":"m3","timestamp":"2026-08-14T14:08:06Z","message":{"role":"toolResult","content":[{"type":"text","text":"查询结果"}]}}"#,
                // 最终回复
                r#"{"type":"message","id":"m4","timestamp":"2026-08-14T14:08:10Z","message":{"role":"assistant","content":[{"type":"text","text":"我是助手"}],"stopReason":"stop"}}"#,
            ],
        );
        let msgs = extract_pi(&p).unwrap();
        assert_eq!(
            msgs.len(),
            2,
            "用户 + 最终回复（toolUse 中途轮/工具结果排除）"
        );
        assert!(msgs[0].user && msgs[0].text == "你是谁");
        assert!(!msgs[1].user && msgs[1].text == "我是助手");
        assert_eq!(
            msgs[0].ts,
            parse_rfc3339("2026-08-14T14:08:00Z").unwrap(),
            "顶层 ISO 优先于 message.timestamp"
        );
        let _ = std::fs::remove_file(&p);
    }
}
