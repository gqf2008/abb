//! 每日会话归纳清理（session_gc）——设置页（历史记录）全局开关控制（默认关）。
//!
//! 每天把过期会话（history jsonl **内容 ts** 距今超 `session_gc_days`，默认 7 天——用
//! 内容 ts 而非文件 mtime，原子重写会让 mtime 失真，见 history::last_ts）交给 bot
//! 后端 agent 归纳成摘要存档（`summaries/<escaped>.md`），再清理工作区内历史/后端
//! 会话文件；摘要在下一次会话（/new 后或清理后的首条消息）里注入衔接上下文。
//!
//! 破坏性语义：
//! - **每会话一次 LLM 调用**（烧钱）+ 删除用户历史——故默认关，需用户在设置页
//!   显式打开；首轮延迟 24h+jitter（见 service.rs session_gc_loop）。
//! - 只删工作区内文件（history/、.pi-sessions/、sessions.json 槽位）；claude/codex
//!   的 transcript 在后端私有目录（~/.claude、~/.codex），
//!   物理不可达，绝不触碰。
//! - 清理次序（摘要写盘**成功后**）：二次校验仍过期（TOCTOU 收窄——归纳期间用户
//!   又发消息则保留会话）→ 删槽位并持久化（未持久化 = 磁盘错误，文件一律未动，
//!   宁留不删下轮重试）→ 删 history jsonl + 迁移/导入标记 → 按槽位 sid 精确删
//!   pi 会话文件（10 分钟 mtime 护栏宁留不删，对齐 /new 清理语义）。
//! - 会话级 AGENTS.md 与摘要文件保留（摘要本身就是下一步的上下文）。
//!
//! 归纳任务角色用 Owner（内部维护任务，pi 也放行——与 run_job 的受限分支
//! 相反：受限会话跑不了 pi，但维护任务必须能跟 bot 后端走）。汇报只走 `crate::log!`，
//! 绝不发聊天消息（系统维护不打扰用户）。

use std::path::Path;

/// 摘要文件注入上限（字符）。超限截断 + 标记（复用 agents_md 的截断标记文案）。
pub const SUMMARY_CAP_CHARS: usize = 8192;
/// 归纳输入的历史预算（字符）：50K 字符覆盖绝大多数会话；超出的旧条目自动丢弃
/// （inject_block 从最新往旧收，旧的先被切掉——归纳只需主线，不必全量）。
pub const HISTORY_BUDGET: usize = 50_000;
/// 单轮归纳封顶 chat 数：agent 调用慢且烧钱，宁可分几天消化也不让单日跑太久。
pub const MAX_CHATS_PER_RUN: usize = 30;

/// 一轮归纳的结果计数（日志汇总用）。
#[derive(Debug, Default, Clone)]
pub struct GcReport {
    /// 归纳 + 清理成功的会话数。
    pub summarized: usize,
    /// agent 调用失败/被中断的会话数（不清理，宁留不删，下轮重试）。
    pub failed: usize,
    /// 跳过数（归纳期间历史被清、或二次校验发现会话恢复活跃）。
    pub skipped: usize,
}

/// 一轮归纳清理（per-bot，service 每日循环调用）：选候选 → 逐个归纳 → 写盘 → 清理。
/// 天数热读 config（与 tidy_loop 同款），热读失败按 7 天兜底。
pub async fn run_once(
    bridge: &crate::bridge::Bridge,
    stop: &tokio_util::sync::CancellationToken,
) -> GcReport {
    let days = crate::config::Config::load()
        .map(|c| c.session_gc_days.max(1))
        .unwrap_or(7);
    run_once_with_days(bridge, stop, days, bridge.agent_runner.as_ref()).await
}

/// 可测核心：天数与 agent runner 由调用方注入（生产 = run_once 热读 config +
/// 桥的 RealAgentRunner；测试注入固定天数 + 挡板 runner）。归纳走桥的 AgentRunner
/// 抽象（与聊天/job 路径同源），使「归纳 → 摘要写盘 → 二次校验 → 清理」整条编排
/// 可被挡板驱动（此前 run_once 无任何测试——本 diff 最破坏性的路径）。
pub async fn run_once_with_days(
    bridge: &crate::bridge::Bridge,
    stop: &tokio_util::sync::CancellationToken,
    days: u32,
    runner: &dyn crate::agent::AgentRunner,
) -> GcReport {
    let bot_key = bridge.bot.key();
    let workspace = crate::workspace_dir(&bot_key);
    let now = crate::chrono_lite::unix_secs();
    let cutoff = now.saturating_sub(u64::from(days.max(1)) * 86400);
    // 复用桥自己的 SessionStore 实例：与消息路径共享同一把锁与刷新签名——另建实例
    // 会与 bridge.sessions 产生 refresh→save 交叉覆盖，把刚删的槽位复活（审查）。
    let store = &bridge.sessions;
    let mut report = GcReport::default();

    // #200：buzz 后端不经 CLI → 归纳无法执行。整轮**一次性**跳过并留一行日志：
    // 落到 per-chat 循环里撞 agent::run 的守卫，会变成「每 chat 每天一行 归纳失败」
    // 的永刷（审查 #205r2）。代价（记录）：buzz bot 的历史/会话不因归纳而收缩，
    // 接线归纳到 relay 前进 #206。
    if crate::agent::Backend::parse(&bridge.default_backend).is_buzz() {
        crate::log!("[session-gc:{bot_key}] buzz 后端不经 CLI，跳过本轮会话归纳（见 #206）");
        return report;
    }

    for key in select_candidates_in(&workspace, now, days)
        .into_iter()
        .take(MAX_CHATS_PER_RUN)
    {
        if stop.is_cancelled() {
            crate::log!("[session-gc:{bot_key}] 关停中，中断本轮归纳");
            break;
        }
        let esc = crate::history::escape_key(&key);
        // 归纳期间历史被清（/new / 手动删文件）→ 无内容可归纳，跳过
        let hist = crate::history::History::open_in(&workspace.join("history"), &key);
        let Some(last_ts) = hist.last_ts() else {
            report.skipped += 1;
            continue;
        };
        let (history_block, n) = hist.inject_block("", HISTORY_BUDGET);
        if n == 0 {
            report.skipped += 1;
            continue;
        }
        // 与 run_job 相同的 prompt 结构：三级 AGENTS.md 指令块（可选）+ 归纳指令体
        let agents_block =
            crate::agents_md::collect_block_at(&bridge.agents_md_root, &bot_key, &key);
        let prompt = build_summary_prompt(&agents_block, &history_block);
        // 镜像 run_job 的 agent 调用：fresh UUID、resume=false、sessions=None（无需
        // 回存 thread_id）、role=Owner（内部维护任务，pi 也放行）。
        let backend = crate::agent::Backend::parse(&bridge.default_backend);
        let sum_sid = uuid::Uuid::new_v4().to_string();
        // 关停联动：stop 触发即置位 cancel flag——agent::run 阶段间检查它，
        // 正在跑的归纳尽快中断（#69：不可让 shutdown_wait 无界等一轮 LLM 调用跑完；
        // 中断走 Cancelled 分支，宁留不删下轮重试）。flag 与 stop 同寿命，进程退出即弃。
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let cancel = cancel.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                stop.cancelled().await;
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            });
        }
        // 归纳会话自身转录用 run 返回的真实 id 删（见下方注释；占位 UUID 的 InSet
        // 匹配不到后端自生成/真实 id 的转录——审查修复）
        let (reply, transcript_sid) = match runner
            .run(
                backend,
                &prompt,
                &sum_sid,
                false,
                &key,
                &key,
                &bot_key,
                crate::config::SenderRole::Owner,
                None,
                None,
                Some(cancel),
            )
            .await
        {
            Ok(crate::agent::RunOutcome::Reply {
                reply,
                // 真实 id：pi 的 --session-id 固定 = 入口 sum_sid；agent::run 已把
                // 返回值换成对端真实 id（sessions=None 也换）——与 .pi-sessions 文件
                // 一致，InSet 才能命中。
                session_id: real_sid,
                ..
            }) => (reply, real_sid),
            Ok(crate::agent::RunOutcome::Cancelled) => {
                crate::log!("[session-gc:{bot_key}] ⏰ {esc} 归纳被中断，保留会话");
                // 失败/中断也要清掉归纳会话自己的转录：pi 文件含入口 sum_sid（固定
                // --session-id），精确删（留待 tidy 的孤儿清理兜底——审查修复）
                crate::agent::remove_pi_transcripts(
                    &workspace,
                    &std::collections::HashSet::from([sum_sid.clone()]),
                    crate::agent::SidMatch::InSet,
                    None,
                );
                report.failed += 1;
                continue;
            }
            Err(e) => {
                crate::log!("[session-gc:{bot_key}] ⚠️ {esc} 归纳失败：{e}");
                crate::agent::remove_pi_transcripts(
                    &workspace,
                    &std::collections::HashSet::from([sum_sid.clone()]),
                    crate::agent::SidMatch::InSet,
                    None,
                );
                report.failed += 1;
                continue;
            }
        };
        // 归纳会话自身的转录文件：pi 后端会在 .pi-sessions 落盘（<ts>_<真实sid>.jsonl，
        // id 不属于任何槽位）——run 已结束，即刻删掉，不留孤儿（tidy 默认关，不能
        // 依赖它兜底）。claude/codex 的转录在后端私有目录（~/.claude 等），物理不可达，跳过。
        crate::agent::remove_pi_transcripts(
            &workspace,
            &std::collections::HashSet::from([transcript_sid]),
            crate::agent::SidMatch::InSet,
            None,
        );
        // 摘要写盘（幂等：同 key 覆盖旧摘要）。写盘失败**不得进入清理**——历史已删
        // 而摘要未落盘 = 永久丢上下文；按失败计，下轮重试（宁留不删）。
        let Some(path) = write_summary_file(&workspace, &key, &reply, last_ts) else {
            crate::log!("[session-gc:{bot_key}] ⚠️ {esc} 摘要写盘失败，保留会话与历史");
            report.failed += 1;
            continue;
        };
        if cleanup_chat_in(&workspace, &key, store, cutoff, &bridge.history_lock(&key)) {
            crate::log!(
                "[session-gc:{bot_key}] {esc} 归纳完成（{} 字）→ {}",
                reply.chars().count(),
                crate::agent::truncate(&path.display().to_string(), 60)
            );
            report.summarized += 1;
        } else {
            // 二次校验没过（归纳期间用户又发消息）→ 保留会话与历史；摘要已存档无害，
            // 会话重新过期时下轮再归纳会覆盖它
            crate::log!("[session-gc:{bot_key}] {esc} 归纳完成但会话已恢复活跃，保留");
            report.skipped += 1;
        }
    }
    report
}

/// 枚举过期候选 chat key（会话归纳用）：读 sessions.json 的槽位，history jsonl 的
/// 内容 ts 距今超 `days` 才入选；无历史文件/空/新鲜跳过。最久未活跃优先（排序稳定）。
pub fn select_candidates_in(workspace: &Path, now: u64, days: u32) -> Vec<String> {
    select_candidates_in_with_state(
        workspace,
        now,
        days,
        &crate::session_state::SessionState::production(),
    )
}

/// 可测核心：暂停态由调用方注入（生产 = production；测试注入临时 session_state，
/// 不碰真实 ~/.agent-bridge/session_state.json）。
pub(crate) fn select_candidates_in_with_state(
    workspace: &Path,
    now: u64,
    days: u32,
    state: &crate::session_state::SessionState,
) -> Vec<String> {
    // 复用 SessionStore 的解析（新格式/旧扁平迁移/刷新签名单一来源，不绕行裸读
    // sessions.json——schema 演进只改一处；审查修复）。无槽位文件 → 空（常态）。
    let store = crate::sessions::SessionStore::at("claude", workspace.join("sessions.json"));
    let cutoff = now.saturating_sub(u64::from(days.max(1)) * 86400);
    // #87 暂停豁免：暂停会话不参与 GC（用户显式想保留，不应被静默归纳回收）。
    // 暂停期消息不入 history → last_ts 不再推进，若不豁免必然下一轮入选。
    // bot_key = workspace 目录名（workspace_dir(bot_key) 的末段）；测试临时目录
    // 无对应暂停记录，lookup 自然不命中。
    let bot_key = workspace
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let mut out: Vec<(u64, String)> = Vec::new();
    for key in store.chat_keys() {
        if state.is_paused(&bot_key, &key) {
            continue;
        }
        let hist = crate::history::History::open_in(&workspace.join("history"), &key);
        let Some(last) = hist.last_ts() else {
            continue; // 无历史文件/空 → 跳过（已归纳清理过的会话不再进候选）
        };
        if last == 0 {
            continue; // ts 未知（导入条目缺失时间戳按 0 反序列化）→ 无法判定过期，
                      // 宁留不删（审查修复：否则导入的会话次日即入选、1970 头部摘要+删除）
        }
        if last < cutoff {
            out.push((last, key));
        }
    }
    out.sort_by_key(|(ts, _)| *ts);
    out.into_iter().map(|(_, k)| k).collect()
}

/// 组装归纳 prompt：三级 AGENTS.md 指令块（可选，与 job_prompt 同构）+ 结构化归纳
/// 指令体。指令体用明确的小节名（模型输出直接按节填充 → 摘要文件结构稳定可读）。
pub fn build_summary_prompt(agents_block: &str, history_block: &str) -> String {
    let body = format!(
        "[会话归纳]\n（以下是 ABB 内部维护任务：请阅读下方对话记录，输出该会话的归纳摘要。\
不执行任何操作、不回复对话内容本身，只输出摘要文本）\n\n\
# 会话主题\n一句话概括该会话的主题。\n\n\
# 关键结论\n用列表列出该会话达成的关键结论与决定。\n\n\
# 待办事项\n列出未完成或需要跟进的事项；没有则写「无」。\n\n\
# 涉及资源\n列出对话中提及的文件、链接、账号等资源（若可识别）；没有则写「无」。\n\n\
# 对话记录\n{history_block}"
    );
    if agents_block.is_empty() {
        body
    } else {
        format!("{agents_block}\n\n{body}")
    }
}

/// 写摘要存档：`summaries/<escaped>.md`，头注释（生成时间/原最后活跃/会话 key）+
/// agent 回复。返回写入路径（日志用）；**写盘失败返回 None**——调用方不得继续清理
/// （历史已删而摘要未落盘 = 永久丢上下文）。会话级 AGENTS.md 与历史标记由调用方处理。
pub fn write_summary_file(
    workspace: &Path,
    key: &str,
    reply: &str,
    last_ts: u64,
) -> Option<std::path::PathBuf> {
    let esc = crate::history::escape_key(key);
    let dir = workspace.join("summaries");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{esc}.md"));
    // epoch_to_ymd 约定入参为本地（UTC+8）秒（chrono_lite::now 先加 8h 再拆）；
    // last_ts 是原始 UTC 秒，先加偏移再拆——否则头部时间比本地早 8 小时（审查）。
    let (y, mo, d, h, mi, _) = crate::chrono_lite::epoch_to_ymd(last_ts + 8 * 3600);
    let content = format!(
        "> ABB 会话归纳自动生成（{}）\n> 原会话最后活跃：{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}\n> 会话 key：{key}\n\n{reply}\n",
        crate::chrono_lite::now()
    );
    // 摘要与历史同用原子写（uuid tmp + rename）：半写文件不会出现在注入面上
    if crate::atomic_write_sensitive(&path, &content).is_err() {
        return None;
    }
    Some(path)
}

/// 清理已归纳会话的工作区残留（摘要写盘成功后调用）：
/// 1. 二次校验仍过期（TOCTOU 收窄：归纳期间用户又发消息 → 保留会话，返回 false）；
/// 2. 删 sessions.json 槽位并持久化（未持久化 = 磁盘错误，文件一律未动，返回 false
///    ——「先文件后槽位」在 save 失败时文件已删而槽位还在，陈旧槽位指向已删文件；
///    审查修复。槽位删后文件清理中断 → 残留文件由 tidy 孤儿清理兜底，宁留文件）；
/// 3. 删 history jsonl + 迁移/导入标记（`.migrated.json` 是会话级迁移状态，一并删）；
/// 4. 按槽位 sid 精确删 pi 会话文件（mtime 超 [`TRANSCRIPT_FRESH_SECS`] 才删，
///    宁留不删）；claude/codex 的 transcript 在后端私有目录，物理不可达，跳过。
///
/// 返回是否真的清理了（false = 会话恢复活跃或持久化失败，保留一切）。
pub fn cleanup_chat_in(
    workspace: &Path,
    key: &str,
    store: &crate::sessions::SessionStore,
    cutoff_ts: u64,
    epoch: &std::sync::Mutex<u64>,
) -> bool {
    // 0. 全程持 per-key 历史代际锁（与 handle 的注入读/写盘、/new 清盘同锁串行，见
    // bridge::history_lock）：二次校验与文件删除对「归纳期间新到消息」原子——消息落盘
    // 要么发生在锁前（校验看到新鲜 ts → 保留会话），要么在锁后（全新会话，摘要兜底
    // 注入衔接）。无锁时校验与落盘可交错：删除恰在用户轮与助手轮之间 → 该轮回复落进
    // 已删文件，历史残缺（审查修复；持锁段为纯同步文件操作，不跨 await）。
    let _epoch = epoch.lock().unwrap_or_else(|e| e.into_inner());
    // 1. 二次校验（TOCTOU 收窄）
    let hist = crate::history::History::open_in(&workspace.join("history"), key);
    if hist.last_ts().map(|ts| ts >= cutoff_ts).unwrap_or(false) {
        return false;
    }
    // 2. 先取槽位 sid（文件删除要用），再**先删槽位并持久化**：save 失败 = 磁盘错误，
    //    此时文件一律未动，保留会话状态返回 false（宁留不删，下轮重试）——原次序
    //    「先删文件后删槽位」在持久化失败时文件已丢而槽位还在，陈旧槽位指向已删
    //    文件（审查修复）。槽位删成功后文件删除若中断（崩溃），残留文件由 tidy
    //    孤儿清理兜底，语义是「宁留文件」而非「宁留空洞」。
    let pi_sid = store.chat_entry(key).map(|e| e.pi.session_id.clone());
    if !store.remove_chat(key) {
        crate::log!(
            "[session-gc] ⚠️ {} 槽位删除未持久化，保留会话状态",
            crate::history::escape_key(key)
        );
        return false;
    }
    // 3. 删 history jsonl + 标记文件
    let esc = crate::history::escape_key(key);
    let history_dir = workspace.join("history");
    for suffix in ["jsonl", "migrated.json", "imported.json"] {
        let _ = std::fs::remove_file(history_dir.join(format!("{esc}.{suffix}")));
    }
    // 4. 按槽位 sid 精确删后端会话文件（mtime 护栏宁留不删；claude/codex 的 transcript
    //    在后端私有目录，物理不可达，跳过）。公共判定见 agent::remove_*_transcripts
    //    （与 /new 同源；护栏 10 分钟，过期会话槽位理论上无在跑任务，护栏只是双保险）。
    if let Some(pi_sid) = pi_sid {
        if !pi_sid.is_empty() {
            let sids = std::collections::HashSet::from([pi_sid]);
            crate::agent::remove_pi_transcripts(
                workspace,
                &sids,
                crate::agent::SidMatch::InSet,
                Some(crate::agent::TRANSCRIPT_FRESH_SECS),
            );
        }
    }
    true
}

/// 可测核心：任意 workspace 目录。读 `summaries/<escaped>.md`，包成
/// `[会话摘要]` 块（8KB 截断 + 标记）。bridge 在历史为空（/new 或归纳清理后）时
/// 兜底注入它衔接上下文——复用 should_inject 闸，无新 marker。
/// （生产路径由 bridge 以 `agents_md_root/workspaces/<bot>` 为 workspace 调用。）
pub(crate) fn summary_block_at(workspace: &Path, key: &str) -> Option<String> {
    let esc = crate::history::escape_key(key);
    let path = workspace.join("summaries").join(format!("{esc}.md"));
    let content = std::fs::read_to_string(&path).ok()?;
    if content.trim().is_empty() {
        return None;
    }
    // 截断逻辑与 AGENTS.md 注入块同源（cap_content：超限截断 + TRUNC_MARKER；审查清理）
    let content = crate::agents_md::cap_content(&content, SUMMARY_CAP_CHARS);
    Some(format!(
        "[会话摘要]\n（以下是该会话归档时的归纳摘要，供衔接上下文）\n\n{content}\n\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 唯一 temp workspace（每个测试独立，避免并发互踩）。
    fn temp_ws(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("abb-gc-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// 构造一条 history jsonl 行（内容 ts 精确可控——候选判定依赖它）。
    fn line(mid: &str, ts: u64) -> String {
        format!(
            "{{\"mid\":\"{mid}\",\"user\":true,\"backend\":\"claude\",\"text\":\"{mid}\",\"ts\":{ts}}}\n"
        )
    }

    /// 写 history jsonl + sessions.json 槽位（模拟一个会话）。
    fn seed_chat(ws: &std::path::Path, key: &str, ts: u64) {
        std::fs::create_dir_all(ws.join("history")).unwrap();
        std::fs::write(
            ws.join("history")
                .join(format!("{}.jsonl", crate::history::escape_key(key))),
            line("m1", ts),
        )
        .unwrap();
        let path = ws.join("sessions.json");
        let mut data: std::collections::HashMap<String, crate::sessions::ChatEntry> =
            if path.exists() {
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap_or_default()
            } else {
                std::collections::HashMap::new()
            };
        data.insert(
            key.into(),
            crate::sessions::ChatEntry {
                claude: crate::sessions::Slot {
                    session_id: format!("sid_{key}"),
                    started: true,
                    ..Default::default()
                },
                // pi 槽位同 sid：cleanup 按槽位 sid 精确删会话文件
                pi: crate::sessions::Slot {
                    session_id: format!("sid_{key}"),
                    started: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        std::fs::write(&path, serde_json::to_string(&data).unwrap()).unwrap();
    }

    #[test]
    fn select_candidates_picks_stale_only_oldest_first() {
        let ws = temp_ws("cand");
        let now = crate::chrono_lite::unix_secs();
        seed_chat(&ws, "oc_stale_old", now - 30 * 86400);
        seed_chat(&ws, "oc_stale_new", now - 10 * 86400);
        seed_chat(&ws, "oc_fresh", now - 3600); // 新鲜：不入选
                                                // 无历史文件的槽位（已归纳清理过）→ 跳过
        let mut all: std::collections::HashMap<String, crate::sessions::ChatEntry> =
            serde_json::from_str(&std::fs::read_to_string(ws.join("sessions.json")).unwrap())
                .unwrap();
        all.insert("oc_nohist".into(), crate::sessions::ChatEntry::default());
        std::fs::write(
            ws.join("sessions.json"),
            serde_json::to_string(&all).unwrap(),
        )
        .unwrap();

        let cands = select_candidates_in(&ws, now, 7);
        assert_eq!(
            cands,
            vec!["oc_stale_old", "oc_stale_new"],
            "只选过期且最久优先"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn paused_chat_is_exempt_from_gc_candidates() {
        // #87：暂停会话不参与 GC（即使已超 session_gc_days）
        let ws = temp_ws("paused");
        let now = crate::chrono_lite::unix_secs();
        let bot_key = ws.file_name().unwrap().to_str().unwrap().to_string();
        seed_chat(&ws, "oc_stale_paused", now - 30 * 86400);
        seed_chat(&ws, "oc_stale_free", now - 30 * 86400);
        // 注入临时暂停态：暂停 stale_paused
        let state_path =
            std::env::temp_dir().join(format!("abb-gc-paused-{}.json", uuid::Uuid::new_v4()));
        let state = crate::session_state::SessionState::at(state_path.clone());
        state.pause(&bot_key, "oc_stale_paused", "test");

        let cands = select_candidates_in_with_state(&ws, now, 7, &state);
        assert_eq!(
            cands,
            vec!["oc_stale_free"],
            "暂停会话被豁免，仅未暂停的过期会话入选"
        );
        let _ = std::fs::remove_file(&state_path);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn build_summary_prompt_has_sections_and_agents_prefix() {
        let p = build_summary_prompt("", "用户: hi\n助手: hello");
        for section in ["会话主题", "关键结论", "待办事项", "涉及资源", "对话记录"]
        {
            assert!(p.contains(&format!("# {section}")), "缺小节 {section}: {p}");
        }
        assert!(p.contains("用户: hi\n助手: hello"), "对话记录在后");
        let p2 = build_summary_prompt("[指令文件]\n规则", "历史");
        assert!(p2.starts_with("[指令文件]"), "有指令块时在最前");
        assert!(p2.find("[指令文件]").unwrap() < p2.find("# 会话主题").unwrap());
    }

    #[test]
    fn write_and_cleanup_roundtrip() {
        let ws = temp_ws("clean");
        let now = crate::chrono_lite::unix_secs();
        seed_chat(&ws, "oc_a", now - 30 * 86400);
        // 迁移标记 + 导入标记（应随清理删除）
        let esc = crate::history::escape_key("oc_a");
        std::fs::write(
            ws.join("history").join(format!("{esc}.migrated.json")),
            "{}",
        )
        .unwrap();
        std::fs::write(
            ws.join("history").join(format!("{esc}.imported.json")),
            "[]",
        )
        .unwrap();
        // 后端会话文件：pi（文件名含 sid）拨旧可删；另一个新鲜 pi 文件（含同 sid
        // 但 mtime 新）宁留不删
        std::fs::create_dir_all(ws.join(".pi-sessions")).unwrap();
        let pi_old = ws.join(".pi-sessions/1000_sid_oc_a.jsonl");
        std::fs::write(&pi_old, "x").unwrap();
        set_mtime_old(&pi_old, 2 * 3600);
        let pi_fresh = ws.join(".pi-sessions/2000_sid_oc_a.jsonl");
        std::fs::write(&pi_fresh, "x").unwrap();
        // 会话级 AGENTS.md：保留（与摘要同属「下一步的上下文」）
        std::fs::create_dir_all(ws.join("sessions")).unwrap();
        let ses_agents = ws.join("sessions").join(format!("{esc}.AGENTS.md"));
        std::fs::write(&ses_agents, "会话指令").unwrap();

        let last_ts = now - 30 * 86400;
        let path = write_summary_file(&ws, "oc_a", "主题：…\n关键结论：…", last_ts)
            .expect("摘要写盘应成功");
        assert!(path.exists(), "摘要已写盘");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("原会话最后活跃"), "头注释含原活跃时间");
        assert!(content.contains("会话 key：oc_a"), "头注释含会话 key");
        assert!(content.contains("主题：…"), "agent 回复在头部之后");

        let store = crate::sessions::SessionStore::at("claude", ws.join("sessions.json"));
        let cutoff = now.saturating_sub(7 * 86400);
        assert!(
            cleanup_chat_in(
                &ws,
                "oc_a",
                &store,
                cutoff,
                &std::sync::Mutex::new(0) // 单测无并发消息管线，用空锁占位
            ),
            "过期会话应清理"
        );
        // 历史/标记/后端会话文件全清；摘要与会话级 AGENTS.md 保留
        assert!(
            !ws.join("history").join(format!("{esc}.jsonl")).exists(),
            "历史 jsonl 已删"
        );
        assert!(
            !ws.join("history")
                .join(format!("{esc}.migrated.json"))
                .exists(),
            "迁移标记已删"
        );
        assert!(
            !ws.join("history")
                .join(format!("{esc}.imported.json"))
                .exists(),
            "导入标记已删"
        );
        assert!(!pi_old.exists(), "旧 pi 会话文件已删");
        assert!(pi_fresh.exists(), "新鲜 pi 文件宁留不删");
        assert!(ses_agents.exists(), "会话级 AGENTS.md 保留");
        assert!(path.exists(), "摘要保留");
        assert!(store.chat_entry("oc_a").is_none(), "槽位已删");
        // 二次校验：新鲜会话（最后活跃 < cutoff）即使有摘要也不清理
        seed_chat(&ws, "oc_b", now - 3600);
        assert!(
            !cleanup_chat_in(&ws, "oc_b", &store, cutoff, &std::sync::Mutex::new(0)),
            "恢复活跃的会话保留"
        );
        assert!(store.chat_entry("oc_b").is_some(), "槽位仍在");
        assert!(ws.join("history/oc_b.jsonl").exists(), "历史仍在");
        std::fs::remove_dir_all(&ws).ok();
    }

    fn set_mtime_old(path: &std::path::Path, secs_ago: u64) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_times(
            std::fs::FileTimes::new().set_modified(
                std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago),
            ),
        )
        .unwrap();
    }

    #[test]
    fn summary_block_reads_archive_truncates_and_missing_none() {
        let ws = temp_ws("block");
        // 缺失 → None
        assert!(summary_block_at(&ws, "oc_x").is_none());
        // 存在 → 块带 [会话摘要] 头与内容
        let esc = crate::history::escape_key("oc_x");
        std::fs::create_dir_all(ws.join("summaries")).unwrap();
        std::fs::write(
            ws.join("summaries").join(format!("{esc}.md")),
            "> 头注释\n主题：好",
        )
        .unwrap();
        let block = summary_block_at(&ws, "oc_x").unwrap();
        assert!(block.starts_with("[会话摘要]"), "块头: {block}");
        assert!(block.contains("主题：好"));
        assert!(block.ends_with("\n\n"), "块尾与后续段隔空行");
        // 超限截断 + 标记
        let big = "长".repeat(SUMMARY_CAP_CHARS + 100);
        std::fs::write(ws.join("summaries").join(format!("{esc}.md")), &big).unwrap();
        let block2 = summary_block_at(&ws, "oc_x").unwrap();
        assert!(
            block2.contains(crate::agents_md::TRUNC_MARKER),
            "超限带截断标记"
        );
        assert_eq!(
            block2.matches("长").count(),
            SUMMARY_CAP_CHARS,
            "截到恰好上限"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    // ── run_once 全编排（本 diff 最破坏性的路径，此前零测试）──

    /// 挡板 messenger（Bridge::new 需要；run_once 不调用它）。
    struct DummyMsgr;
    #[async_trait::async_trait]
    impl crate::messenger::Messenger for DummyMsgr {
        async fn send_text(&self, _chat_id: &str, _text: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// 挡板 agent runner：run 立即返回固定回复（Some）或报错（None）。
    struct StubRunner {
        reply: Option<String>,
    }
    #[async_trait::async_trait]
    impl crate::agent::AgentRunner for StubRunner {
        #[allow(clippy::too_many_arguments)]
        async fn run(
            &self,
            _backend: crate::agent::Backend,
            _prompt: &str,
            session_id: &str,
            _resume: bool,
            _chat_id: &str,
            _session_key: &str,
            _bot_key: &str,
            _role: crate::config::SenderRole,
            _sessions: Option<&crate::sessions::SessionStore>,
            _progress: Option<tokio::sync::mpsc::UnboundedSender<String>>,
            _cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        ) -> Result<crate::agent::RunOutcome, String> {
            match &self.reply {
                Some(r) => Ok(crate::agent::RunOutcome::Reply {
                    reply: r.clone(),
                    session_id: session_id.to_string(),
                    rebuilt: false,
                }),
                None => Err("后端不可用".into()),
            }
        }
    }

    /// 建一个隔离桥（唯一 uuid bot key + 真实 temp workspace，测后整树删除）。
    fn gc_test_bridge() -> std::sync::Arc<crate::bridge::Bridge> {
        let bot = crate::config::BotConfig {
            name: format!("abb-gc-test-{}", uuid::Uuid::new_v4()),
            kind: "feishu".into(),
            bot_name: "庆小丰".into(),
            bot_open_id: "ou_bot".into(),
            owner_open_id: "ou_boss".into(),
            ..Default::default()
        };
        std::sync::Arc::new(crate::bridge::Bridge::new(
            std::sync::Arc::new(DummyMsgr),
            bot,
            &crate::config::Config::default(),
        ))
    }

    /// 种一个过期会话（history + 槽位 + 一个旧 pi 转录）。
    fn seed_gc_chat(ws: &std::path::Path, key: &str) {
        let now = crate::chrono_lite::unix_secs();
        seed_chat(ws, key, now - 8 * 86400); // 8 天前最后活跃 > 7 天阈值
        std::fs::create_dir_all(ws.join(".pi-sessions")).unwrap();
        let pi_file = ws.join(".pi-sessions/1000_sid_oc_gc.jsonl");
        std::fs::write(&pi_file, "x").unwrap();
        set_mtime_old(&pi_file, 2 * 3600);
    }

    #[tokio::test]
    async fn run_once_summarizes_writes_summary_and_cleans() {
        // 归纳成功 → 摘要落盘 → 历史/转录/槽位全清（编排级回归网）
        let bridge = gc_test_bridge();
        let ws = crate::workspace_dir(&bridge.bot.key());
        seed_gc_chat(&ws, "oc_gc");
        let stub = StubRunner {
            reply: Some("主题：写周报\n关键结论：无\n待办事项：无\n涉及资源：无".into()),
        };
        let stop = tokio_util::sync::CancellationToken::new();
        let report = run_once_with_days(&bridge, &stop, 7, &stub).await;
        assert_eq!(report.summarized, 1, "归纳应成功: {report:?}");
        assert_eq!(report.failed, 0);
        assert_eq!(report.skipped, 0);
        // 历史与标记已清
        let esc = crate::history::escape_key("oc_gc");
        assert!(
            !ws.join("history").join(format!("{esc}.jsonl")).exists(),
            "历史 jsonl 已删"
        );
        // 转录已清（槽位 sid 精确匹配）
        assert!(
            !ws.join(".pi-sessions/1000_sid_oc_gc.jsonl").exists(),
            "pi 转录已删"
        );
        // 摘要已存档
        assert!(
            ws.join("summaries").join(format!("{esc}.md")).exists(),
            "摘要已落盘"
        );
        // 槽位已删并落盘（新实例读盘验证）
        let store = crate::sessions::SessionStore::at("claude", ws.join("sessions.json"));
        assert!(store.chat_entry("oc_gc").is_none(), "槽位已删并持久化");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn run_once_failure_keeps_everything() {
        // 归纳失败 → 不清理任何文件（宁留不删，下轮重试）
        let bridge = gc_test_bridge();
        let ws = crate::workspace_dir(&bridge.bot.key());
        seed_gc_chat(&ws, "oc_gc");
        let stub = StubRunner { reply: None };
        let stop = tokio_util::sync::CancellationToken::new();
        let report = run_once_with_days(&bridge, &stop, 7, &stub).await;
        assert_eq!(report.failed, 1, "归纳失败应计数: {report:?}");
        assert_eq!(report.summarized, 0);
        // 一切保留
        let esc = crate::history::escape_key("oc_gc");
        assert!(
            ws.join("history").join(format!("{esc}.jsonl")).exists(),
            "历史保留"
        );
        assert!(
            ws.join(".pi-sessions/1000_sid_oc_gc.jsonl").exists(),
            "pi 转录保留"
        );
        assert!(
            !ws.join("summaries").join(format!("{esc}.md")).exists(),
            "无摘要落盘"
        );
        let store = crate::sessions::SessionStore::at("claude", ws.join("sessions.json"));
        assert!(store.chat_entry("oc_gc").is_some(), "槽位保留");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn run_once_cancel_stops_before_work() {
        // 关停中 → 不做任何归纳（stop 检查在每 chat 前）
        let bridge = gc_test_bridge();
        let ws = crate::workspace_dir(&bridge.bot.key());
        seed_gc_chat(&ws, "oc_gc");
        let stub = StubRunner {
            reply: Some("主题：x".into()),
        };
        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();
        let report = run_once_with_days(&bridge, &stop, 7, &stub).await;
        assert_eq!(
            report.summarized + report.failed + report.skipped,
            0,
            "关停中不工作"
        );
        assert!(ws.join("history").join("oc_gc.jsonl").exists(), "历史保留");
        std::fs::remove_dir_all(&ws).ok();
    }
}
