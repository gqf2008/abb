//! #87 会话可观察/可管控 CLI（session list / show / pause / resume / delete）。
//!
//! 由 main.rs `run_session_cli` 分发（原有 `reset` 保持不动）。所有命令的 bot 解析：
//! 显式 `--bot <bot名>` 优先 → `AGENT_BRIDGE_BOT_KEY` env → 唯一 bot（与 job/deliver
//! CLI 同款回落）。chat 解析：位置参数优先 → `AGENT_BRIDGE_CHAT_ID` env。
//!
//! 数据源：
//! - 会话 key：各 bot 工作区 `sessions.json`（SessionStore 槽位）+ `history/*.jsonl`
//!   （/new / 切后端后无槽位但仍有历史/GC 候选的会话）；
//! - 活跃/消息量：history 最后时间戳 + `messages.sqlite`（MsgStore 按 bot×chat 聚合）；
//! - 暂停态：`session_state.json`（SessionState，热重载）。
//! - 可读名：虚拟 Bot 登记表 role_name 优先（#75），否则 chat_id 原样。

use crate::config::Config;
use crate::history::History;
use crate::msgstore::MsgStore;
use crate::session_state::SessionState;
use crate::sessions::SessionStore;

/// 分发入口（main.rs run_session_cli 对非 reset 子命令调用）。
pub fn run(args: &[String]) -> i32 {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    match sub {
        "list" => cmd_list(&args[1..]),
        "show" => cmd_show(&args[1..]),
        "pause" => cmd_pause(&args[1..]),
        "resume" => cmd_resume(&args[1..]),
        "delete" => cmd_delete(&args[1..]),
        _ => {
            eprintln!("用法：\n  agent-bridge session list [--bot <名>] [--state active|paused|gc-pending] [--active-days N] [--paused]\n  agent-bridge session show <chat_id> [--last N] [--since YYYY-MM-DD] [--bot <名>]\n  agent-bridge session pause <chat_id> [--bot <名>]\n  agent-bridge session resume <chat_id> [--bot <名>]\n  agent-bridge session delete <chat_id> [--purge] [--yes] [--bot <名>]");
            1
        }
    }
}

/// 解析 `--flag value`（返回 value 或 None）。
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            return args.get(i + 1).cloned();
        }
        i += 1;
    }
    None
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// 解析 bot：--bot 显式 → env AGENT_BRIDGE_BOT_KEY → 唯一 bot。
fn resolve_bot(args: &[String], cfg: &Config) -> Result<String, String> {
    if let Some(v) = flag_value(args, "--bot") {
        let v = v.trim().to_string();
        if cfg.bots.iter().any(|b| b.key() == v) {
            return Ok(v);
        }
        return Err(format!("bot「{v}」不在 config 中"));
    }
    if let Ok(k) = std::env::var("AGENT_BRIDGE_BOT_KEY") {
        if !k.is_empty() {
            return Ok(k);
        }
    }
    match cfg.bots.len() {
        0 => Err("config.json 没有配置任何 bot".into()),
        1 => Ok(cfg.bots[0].key()),
        n => Err(format!(
            "有 {n} 个 bot 但未指定目标（用 --bot <bot名> 指定，或用 AGENT_BRIDGE_BOT_KEY env）"
        )),
    }
}

/// 解析 chat：位置参数优先，缺省回落 env AGENT_BRIDGE_CHAT_ID。
fn resolve_chat(args: &[String]) -> Result<String, String> {
    if let Some(c) = args.first() {
        let c = c.trim();
        if !c.is_empty() {
            return Ok(c.to_string());
        }
    }
    if let Ok(c) = std::env::var("AGENT_BRIDGE_CHAT_ID") {
        if !c.is_empty() {
            return Ok(c);
        }
    }
    Err(
        "缺 chat_id：agent-bridge session <子命令> <chat_id>（或用 AGENT_BRIDGE_CHAT_ID env）"
            .into(),
    )
}

/// 平台可读名。
fn platform_name(kind: &str) -> String {
    match kind {
        "feishu" => "飞书".to_string(),
        "wechat" => "微信".to_string(),
        "dingtalk" => "钉钉".to_string(),
        _ => kind.to_string(),
    }
}

/// unix 秒 → "YYYY-MM-DD HH:MM"（本地 UTC+8，chrono_lite 口径）。
fn fmt_ts(ts: u64) -> String {
    let (y, mo, d, h, mi, _) = crate::chrono_lite::epoch_to_ymd(ts + 8 * 3600);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
}

/// history 文件名 → 会话 key（escape_key 的逆：%XX 解码 + "%5F" 保留名前缀回退）。
/// escape 对 ASCII 大写统一小写——文件侧不存在区分，反向映射对每个文件是单射的。
fn unescape_key(stem: &str) -> String {
    const RESERVED: [&str; 22] = [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    // escape 对保留名插入的是**字面** "%5F" 前缀——须在 %XX 解码前判定（解码后
    // %5F 已变成 '_'，无从区分）。
    if let Some(rest) = stem.strip_prefix("%5F") {
        let plain = percent_decode(rest);
        if RESERVED.contains(&plain.as_str()) {
            return plain;
        }
    }
    percent_decode(stem)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 可读名：虚拟 Bot 登记 role_name 优先（按 chat 前缀匹配），否则 chat_id 原样。
fn display_name(vbs: &[crate::virtualbot::VirtualBot], bot_key: &str, chat_key: &str) -> String {
    let chat = chat_key.split(':').next().unwrap_or(chat_key);
    for v in vbs {
        if v.bot_key == bot_key && v.chat_id == chat {
            return v.role_name.clone();
        }
    }
    chat_key.to_string()
}

/// 收集某 bot 的全部会话 key：sessions.json 槽位 + history 目录文件（去重）。
/// #194：虚拟 Bot 独立工作区（vb/<uuid>/，与 bot 级同布局）一并并入。
fn chat_keys_for(ws: &std::path::Path, backend: &str) -> Vec<String> {
    let mut keys: Vec<String> = SessionStore::at(backend, ws.join("sessions.json")).chat_keys();
    let mut scan_hist = |dir: &std::path::Path, keys: &mut Vec<String>| {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let path = e.path();
                if !path.is_file()
                    || path
                        .extension()
                        .map(|x| !x.eq_ignore_ascii_case("jsonl"))
                        .unwrap_or(true)
                {
                    continue;
                }
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                let k = unescape_key(&stem);
                if !keys.contains(&k) {
                    keys.push(k);
                }
            }
        }
    };
    scan_hist(&ws.join("history"), &mut keys);
    // #194：vb/<uuid>/ 独立工作区的会话并入列表（布局与 bot 级相同）
    if let Ok(rd) = std::fs::read_dir(ws.join("vb")) {
        for d in rd.flatten() {
            if d.path().is_dir() {
                for k in SessionStore::at(backend, d.path().join("sessions.json")).chat_keys() {
                    if !keys.contains(&k) {
                        keys.push(k);
                    }
                }
                scan_hist(&d.path().join("history"), &mut keys);
            }
        }
    }
    keys
}

/// #194：chat 的工作目录——虚拟 Bot 群 = vb/<uuid>/（含存量迁移），否则 bot 级。
fn ws_for_chat(bot_key: &str, chat: &str) -> std::path::PathBuf {
    crate::virtualbot::ensure_vb_dir(bot_key, chat).unwrap_or_else(|| crate::workspace_dir(bot_key))
}

/// list 输出行。
struct SessionRow {
    last: Option<u64>,
    bot: String,
    chat: String,
    display: String,
    platform: String,
    state: String,
    count_7d: i64,
    count_total: i64,
    backend: String,
}

/// session list：全部 bot × 会话，字段齐全，默认最近活跃倒序。
fn cmd_list(args: &[String]) -> i32 {
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("读 config 失败: {e:#}");
            return 1;
        }
    };
    let bot_filter = flag_value(args, "--bot").map(|s| s.trim().to_string());
    let state_filter = if has_flag(args, "--paused") {
        Some("paused".to_string())
    } else {
        flag_value(args, "--state").map(|s| s.trim().to_string())
    };
    let active_days = flag_value(args, "--active-days")
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    if let Some(st) = &state_filter {
        if !matches!(st.as_str(), "active" | "paused" | "gc-pending") {
            eprintln!("--state 只支持 active / paused / gc-pending（当前：{st}）");
            return 1;
        }
    }

    let state = SessionState::production();
    let stats = MsgStore::production().chat_stats();
    let vbs = crate::virtualbot::VirtualBotStore::new().load();
    let now = crate::chrono_lite::unix_secs();
    let gc_days = cfg.session_gc_days.max(1) as u64;
    let cutoff_gc = now.saturating_sub(gc_days * 86400);

    let mut rows: Vec<SessionRow> = Vec::new();
    for bot in &cfg.bots {
        let key = bot.key();
        if let Some(f) = &bot_filter {
            if key != *f {
                continue;
            }
        }
        let backend = bot.effective_backend(&cfg.default_backend).to_string();
        let ws = crate::workspace_dir(&key);
        // 暂停态一次取齐（避免每会话热刷新）；话题 key 回落 chat 前缀判定
        let paused_keys: Vec<String> = state
            .paused_chats(&key)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        for ck in chat_keys_for(&ws, &backend) {
            let stat_chat = ck.split(':').next().unwrap_or(&ck).to_string();
            let s = stats
                .iter()
                .find(|x| x.bot_key == key && x.chat_id == stat_chat);
            // 最近活跃：history 最后时间戳优先，消息库兜底
            let last = History::open_in(&ws.join("history"), &ck)
                .last_ts()
                .or_else(|| s.as_ref().and_then(|x| x.last_ts).map(|t| t as u64));
            let paused = paused_keys
                .iter()
                .any(|p| ck == *p || ck.starts_with(&format!("{p}:")));
            let st = if paused {
                "paused".to_string()
            } else if last.map(|t| t < cutoff_gc).unwrap_or(false) {
                "gc-pending".to_string()
            } else {
                "active".to_string()
            };
            if let Some(f) = &state_filter {
                if st != *f {
                    continue;
                }
            }
            if active_days > 0 && last.map(|t| t + active_days * 86400 < now).unwrap_or(true) {
                continue;
            }
            rows.push(SessionRow {
                last,
                bot: key.clone(),
                chat: ck.clone(),
                display: display_name(&vbs, &key, &ck),
                platform: platform_name(&bot.kind),
                state: st,
                count_7d: s.map(|x| x.count_7d).unwrap_or(0),
                count_total: s.map(|x| x.count_total).unwrap_or(0),
                backend: backend.clone(),
            });
        }
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.last));

    if rows.is_empty() {
        println!("（没有会话）");
        return 0;
    }
    for r in &rows {
        let active = r.last.map(fmt_ts).unwrap_or_else(|| "-".to_string());
        let msgs = if r.count_total > 0 {
            format!("{}/{}", r.count_7d, r.count_total)
        } else {
            "-".to_string()
        };
        println!(
            "[{}] {} 名={} 平台={} 状态={} 最近活跃={} 消息7d/总={} 后端={}",
            r.bot, r.chat, r.display, r.platform, r.state, active, msgs, r.backend
        );
    }
    0
}

/// session show：单会话概要 + 最近 N 条消息。
fn cmd_show(args: &[String]) -> i32 {
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("读 config 失败: {e:#}");
            return 1;
        }
    };
    let bot_key = match resolve_bot(args, &cfg) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let chat = match resolve_chat(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let last_n = flag_value(args, "--last")
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(10);
    let since = flag_value(args, "--since")
        .and_then(|s| {
            crate::schedule::parse_once(&format!("{} 00:00", s.trim())).map(|d| d.to_unix())
        })
        .unwrap_or(0);
    let bot = cfg.bots.iter().find(|b| b.key() == bot_key).unwrap();
    let backend = bot.effective_backend(&cfg.default_backend).to_string();
    // #194：虚拟 Bot 群的会话/历史/指令在独立工作区 vb/<uuid>/
    let ws = ws_for_chat(&bot_key, &chat);
    let state = SessionState::production();
    let paused = state.is_paused(&bot_key, &chat);

    let hist = History::open_in(&ws.join("history"), &chat);
    let mut entries = hist.entries();
    entries.retain(|e| e.ts >= since.max(0) as u64);
    let total = entries.len();
    entries.sort_by_key(|e| std::cmp::Reverse(e.ts));
    entries.truncate(last_n);

    let (msgs, min_ts, max_ts) = MsgStore::production().chat_count_and_range(&bot_key, &chat);
    let last_active = hist
        .last_ts()
        .map(fmt_ts)
        .unwrap_or_else(|| "-".to_string());
    println!(
        "会话：bot={bot_key} chat={chat} 平台={} 状态={} 后端={backend}",
        platform_name(&bot.kind),
        if paused { "paused" } else { "active" }
    );
    println!(
        "概要：最近活跃={last_active} 历史消息={total}条 消息库={msgs}条{}",
        if let (Some(a), Some(b)) = (min_ts, max_ts) {
            format!("（{} ~ {}）", fmt_ts(a as u64), fmt_ts(b as u64))
        } else {
            String::new()
        }
    );
    if entries.is_empty() {
        println!("（该会话暂无消息记录）");
        return 0;
    }
    println!("最近 {} 条：", entries.len());
    for e in &entries {
        let who = if e.user { "用户" } else { "助手" };
        let text = crate::agent::truncate(&e.text, 60);
        println!("  [{who} {} {}] {}", fmt_ts(e.ts), e.backend, text);
    }
    0
}

/// session pause：暂停会话（不再触发 agent、不回复；消息仍入库）。
fn cmd_pause(args: &[String]) -> i32 {
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("读 config 失败: {e:#}");
            return 1;
        }
    };
    let bot_key = match resolve_bot(args, &cfg) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let chat = match resolve_chat(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let by = std::env::var("AGENT_BRIDGE_BOT_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "cli".to_string());
    let state = SessionState::production();
    if state.pause(&bot_key, &chat, &by) {
        println!(
            "✅ 已暂停会话 bot={bot_key} chat={chat}（新消息不再触发 agent、不回复，消息仍入库）"
        );
        crate::session_state::audit("session.pause", &bot_key, &chat, &by, "ok");
        0
    } else {
        println!("该会话已处于暂停状态（幂等）：bot={bot_key} chat={chat}");
        0
    }
}

/// session resume：恢复会话。
fn cmd_resume(args: &[String]) -> i32 {
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("读 config 失败: {e:#}");
            return 1;
        }
    };
    let bot_key = match resolve_bot(args, &cfg) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let chat = match resolve_chat(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let by = std::env::var("AGENT_BRIDGE_BOT_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "cli".to_string());
    let state = SessionState::production();
    if state.resume(&bot_key, &chat) {
        println!("✅ 已恢复会话 bot={bot_key} chat={chat}（重新接收/回复，历史不补发不重放）");
        crate::session_state::audit("session.resume", &bot_key, &chat, &by, "ok");
        0
    } else {
        eprintln!("该会话未在暂停状态：bot={bot_key} chat={chat}（无需恢复）");
        1
    }
}

/// session delete：终止会话（默认保留历史；--purge 同时清空消息与历史）。
/// 二次确认：交互 y/N 或 --yes；非交互无 --yes → 拒绝并写审计（不静默执行）。
fn cmd_delete(args: &[String]) -> i32 {
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("读 config 失败: {e:#}");
            return 1;
        }
    };
    let bot_key = match resolve_bot(args, &cfg) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let chat = match resolve_chat(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let purge = has_flag(args, "--purge");
    let yes = has_flag(args, "--yes");
    let by = std::env::var("AGENT_BRIDGE_BOT_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "cli".to_string());

    let bot = cfg.bots.iter().find(|b| b.key() == bot_key).unwrap();
    let backend = bot.effective_backend(&cfg.default_backend).to_string();
    // #194：虚拟 Bot 群的会话/历史/指令在独立工作区 vb/<uuid>/
    let ws = ws_for_chat(&bot_key, &chat);
    let state = SessionState::production();

    // 影响面统计：消息库 + 历史 + 会话槽位
    let (msgs, min_ts, max_ts) = MsgStore::production().chat_count_and_range(&bot_key, &chat);
    let hist = History::open_in(&ws.join("history"), &chat);
    let hist_n = hist.entries().len();
    let store = SessionStore::at(&backend, ws.join("sessions.json"));
    let slot = store
        .chat_keys()
        .iter()
        .any(|k| k == &chat || k.starts_with(&format!("{chat}:")));

    if msgs == 0 && hist_n == 0 && !slot && !state.is_paused(&bot_key, &chat) {
        eprintln!("会话不存在或无内容：bot={bot_key} chat={chat}");
        return 1;
    }

    let range = match (min_ts, max_ts) {
        (Some(a), Some(b)) => format!("（消息时间 {} ~ {}）", fmt_ts(a as u64), fmt_ts(b as u64)),
        _ => String::new(),
    };
    let purge_note = if purge {
        "，并清空全部消息与历史（不可恢复）"
    } else {
        "，历史与消息保留（GC 可继续清理）"
    };
    println!(
        "将删除会话 bot={bot_key} chat={chat}：消息库 {msgs} 条 {range}、历史 {hist_n} 条、会话槽位 {}，{purge_note}",
        if slot { "有" } else { "无" }
    );

    // 二次确认：--yes 跳过；否则交互 y/N；stdin 非交互（读不到）→ 拒绝
    if !yes {
        print!("确认删除？[y/N] ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let read = std::io::stdin().read_line(&mut line);
        let ok = match read {
            Ok(n) if n > 0 => {
                let t = line.trim().to_lowercase();
                t == "y" || t == "yes"
            }
            _ => false,
        };
        if !ok {
            eprintln!("已取消（未确认，不删除任何数据）。");
            crate::session_state::audit(
                "session.delete",
                &bot_key,
                &chat,
                &by,
                &format!("rejected-no-confirm purge={purge}"),
            );
            return 1;
        }
    }

    // 执行：槽位 + 暂停态（始终）；消息库 + 历史文件（仅 --purge）
    let mut removed_slots = 0usize;
    for k in store.chat_keys() {
        if (k == chat || k.starts_with(&format!("{chat}:"))) && store.remove_chat(&k) {
            removed_slots += 1;
        }
    }
    state.remove_chat(&bot_key, &chat);
    if purge {
        let n = MsgStore::production().delete_chat(&bot_key, &chat);
        // 清理 history 文件：精确 key + chat: 前缀（话题）全部移除
        let mut removed_files = 0usize;
        if let Ok(rd) = std::fs::read_dir(ws.join("history")) {
            for e in rd.flatten() {
                let path = e.path();
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
                    continue;
                };
                let k = unescape_key(&stem);
                if (k == chat || k.starts_with(&format!("{chat}:")))
                    && std::fs::remove_file(&path).is_ok()
                {
                    removed_files += 1;
                }
            }
        }
        println!(
            "✅ 已删除会话 bot={bot_key} chat={chat}：消息库 {n} 条、历史文件 {removed_files} 个、槽位 {removed_slots} 个（--purge 已清空）"
        );
        crate::session_state::audit(
            "session.delete",
            &bot_key,
            &chat,
            &by,
            &format!("ok purge=true msgs={n} hist_files={removed_files} slots={removed_slots}"),
        );
    } else {
        println!(
            "✅ 已删除会话 bot={bot_key} chat={chat}：槽位 {removed_slots} 个（历史与消息保留）"
        );
        crate::session_state::audit(
            "session.delete",
            &bot_key,
            &chat,
            &by,
            &format!("ok purge=false slots={removed_slots}"),
        );
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_value_and_has_flag_parse() {
        let args = vec![
            "--bot".to_string(),
            "龙虾".to_string(),
            "--state".to_string(),
            "paused".to_string(),
            "--paused".to_string(),
        ];
        assert_eq!(flag_value(&args, "--bot").as_deref(), Some("龙虾"));
        assert_eq!(flag_value(&args, "--state").as_deref(), Some("paused"));
        assert_eq!(flag_value(&args, "--active-days"), None);
        assert!(has_flag(&args, "--paused"));
        assert!(!has_flag(&args, "--yes"));
    }

    #[test]
    fn unescape_roundtrip_known_keys() {
        // 话题形态 chat:thread → 文件名含 %3A
        assert_eq!(unescape_key("oc_1%3Aomt_2"), "oc_1:omt_2");
        // 纯 ASCII 小写原样
        assert_eq!(unescape_key("oc_abc123"), "oc_abc123");
        // %5F 保留名前缀回退
        assert_eq!(unescape_key("%5Fcon"), "con");
        // 中文 chat_id → %XX 多字节解码
        assert_eq!(unescape_key("%E9%BE%99%E8%99%BE"), "龙虾");
    }

    #[test]
    fn fmt_ts_formats_local() {
        // epoch 0 = 1970-01-01 00:00 UTC → 本地 UTC+8 = 08:00
        assert_eq!(fmt_ts(0), "1970-01-01 08:00");
        // 与 chrono_lite::epoch_to_ymd 同口径（其日期换算自有测试覆盖）
        let now = crate::chrono_lite::unix_secs();
        let s = fmt_ts(now);
        assert_eq!(s.len(), 16, "YYYY-MM-DD HH:MM 形态: {s}");
    }

    #[test]
    fn resolve_chat_prefers_positional_over_env() {
        let args = vec!["oc_xxx".to_string()];
        assert_eq!(resolve_chat(&args).unwrap(), "oc_xxx");
        // 无位置参数且无 env → 报错（测试内清 env，桥注入的场景由集成侧覆盖）
        let saved = std::env::var("AGENT_BRIDGE_CHAT_ID").ok();
        std::env::remove_var("AGENT_BRIDGE_CHAT_ID");
        let empty: Vec<String> = Vec::new();
        let r = resolve_chat(&empty);
        match saved {
            Some(v) => std::env::set_var("AGENT_BRIDGE_CHAT_ID", v),
            None => std::env::remove_var("AGENT_BRIDGE_CHAT_ID"),
        }
        assert!(r.is_err());
    }
}
