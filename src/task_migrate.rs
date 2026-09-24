//! P5：`job` → `task` 的**一次性、幂等、可回滚**迁移（#326，设计见 `docs/task-model.md` §D7）。
//!
//! 迁移后 `job` CLI 只写 task store（`tasks/<bot>/tasks.json`），`jobs.json` 降级为
//! **只读迁移源**。本模块只做「读旧文件 → 转 task → 原子写 → 备份 + 落标记」，不做任何删除。
//!
//! 三条不变量（对应 §D7 的四问与验收）：
//! 1. **执行路径唯一**：标记存在 ⇒ 旧 job 调度循环不启动；数据已导入但标记没写（崩溃窗口）
//!    仍判「已迁移」——否则同一 job 会被新旧两条路径各跑一遍。
//! 2. **幂等**：`legacy_job_id` 是去重键，重复启动/重复迁移不产生第二份任务。
//! 3. **失败不破坏**：迁移前先备份；任一 job 无法**无损**转换即整体放弃（不猜语义、不写半截），
//!    `jobs.json` 原样保留，旧周期照旧。
//!
//! 刻意**不复用** `JobStore::at` 的读盘：它对解析失败静默回落成空表（对运行时热重载是合理的
//! 容错），但迁移若照抄，就会把「读不出来」当成「没有任务」而写出标记——原任务被静默搁浅。
//! 迁移宁可不做（保留旧循环），也不静默丢任务。

use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::path::Path;

use crate::schedule::{Job, JobKind};
use crate::task_store::{
    new_id, CreatedBy, PayloadKind, Task, TaskDelivery, TaskLimits, TaskPayload, TaskStore,
    TaskTarget, TaskTrigger, TriggerKind, DEFAULT_RESUME_ON_BOOT, TASK_SCHEMA_VERSION,
};

/// 旧定时任务定义文件（`workspaces/<bot>/jobs.json`），迁移后只读。
pub const LEGACY_JOBS_FILE: &str = "jobs.json";
/// 迁移完成标记。存在 ⇒ 迁移已收尾，旧 job 调度循环**不得**启动。
pub const MIGRATION_MARKER: &str = "jobs.json.migrated";
/// 迁移前备份（`jobs.json` 本身**永不删除/改写**，备份只是额外的回滚保底）。
pub const MIGRATION_BACKUP: &str = "jobs.json.migrated.bak";

/// 迁移结局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrateOutcome {
    /// 迁移已完成（本次做完，或先前已做）：旧 job 调度循环不启动。
    Done,
    /// 本次没能迁移（读/解析/校验/备份/写盘失败）：`jobs.json` 未改动，旧 job 循环照旧。
    LegacyKept(String),
}

/// 一次迁移的结果（service 启动时据此决定是否启动旧 job 调度循环）。
#[derive(Debug, Clone)]
pub struct MigrateReport {
    pub outcome: MigrateOutcome,
    /// 本次真正导入的 job 数。
    pub imported: usize,
    /// 已在 task store 里、本次跳过（幂等去重）的 job 数。
    pub skipped: usize,
    /// 标记文件是否已就位（`LegacyKept` 时恒 false）。
    pub marker_written: bool,
}

impl MigrateReport {
    /// 旧 job 调度循环是否允许启动。**只有「本次没能迁移」才允许**——任何其它情形下
    /// 任务数据要么已在 task store 里、要么标记已就位，再跑旧循环就是双跑。
    pub fn legacy_job_loop_allowed(&self) -> bool {
        matches!(self.outcome, MigrateOutcome::LegacyKept(_))
    }

    fn done(imported: usize, skipped: usize, marker_written: bool) -> MigrateReport {
        MigrateReport {
            outcome: MigrateOutcome::Done,
            imported,
            skipped,
            marker_written,
        }
    }
}

/// 生产入口：按 bot key 解析路径并迁移。
pub fn migrate_bot_if_needed(bot_key: &str) -> MigrateReport {
    let workspace = crate::workspace_dir(bot_key);
    let tasks_root = crate::bridge_dir().join("tasks");
    migrate_with_paths(&workspace, &tasks_root, bot_key)
}

/// **注入缝**（测试用 temp 目录）：`workspace` = `workspaces/<bot>/`，
/// `tasks_root` = `tasks/` 的根（`TaskPaths::with_root` 同款）。告警落 service stdout。
pub(crate) fn migrate_with_paths(
    workspace: &Path,
    tasks_root: &Path,
    bot_key: &str,
) -> MigrateReport {
    migrate_with_paths_warned(workspace, tasks_root, bot_key, &mut std::io::stdout())
}

/// 同上，但注入告警 sink（测试捕获告警文本，验证「伪造标记」路径**不静默**，#326 F1b）。
pub(crate) fn migrate_with_paths_warned(
    workspace: &Path,
    tasks_root: &Path,
    bot_key: &str,
    warn: &mut dyn std::io::Write,
) -> MigrateReport {
    let marker = workspace.join(MIGRATION_MARKER);
    // 已迁移：直接收工（幂等第一道门，也挡住「重复启动重复导入」）。
    //
    // **但标记是执行时信任凭据**（存在 ⇒ 旧 job 循环不启动），它落在 agent 自己的工作区，
    // 受限会话可 Write 出一个空文件/`{}` 把它造出来 ⇒ 该 bot 全部旧 job 静默搁浅
    // （`Done(imported=0)`、旧循环也不启动，用户只看到「迁移完成 0 条」；#326 F1b）。
    // 故这里不只看存在性：内容校验不过即**视为未迁移**，继续走导入路径
    // （导入失败仍 LegacyKept、jobs.json 原样保留），并向 service stdout 发 loud 告警。
    if marker.exists() {
        match validate_marker(&marker) {
            Ok(()) => return MigrateReport::done(0, 0, true),
            Err(e) => {
                crate::log_to!(
                    warn,
                    "[migrate:{bot_key}] ⚠️ 迁移标记 {} 内容无效（{e:#}）：视为未迁移，继续走导入路径（jobs.json 原样保留；迁移成功后会重写标记）",
                    marker.display()
                );
            }
        }
    }
    let src = workspace.join(LEGACY_JOBS_FILE);
    let jobs = match read_legacy_jobs(&src) {
        Ok(jobs) => jobs,
        Err(e) => return legacy_kept(format!("读取旧 job 定义失败：{e:#}")),
    };

    let store = TaskStore::new_at(tasks_root, bot_key);
    // 幂等去重键：store 里已记录 lineage 的 job id。
    let existing: HashSet<String> = store
        .list()
        .into_iter()
        .map(|t| t.legacy_job_id)
        .filter(|s| !s.is_empty())
        .collect();
    let pending: Vec<&Job> = jobs.iter().filter(|j| !existing.contains(&j.id)).collect();
    let skipped = jobs.len() - pending.len();

    // 全量转换（任一 job 无法无损转换 → 整体放弃，不写半截）。转换**先于**任何写盘。
    let mut to_add: Vec<Task> = Vec::with_capacity(pending.len());
    for job in &pending {
        match job_to_task(bot_key, job) {
            Ok(t) => to_add.push(t),
            Err(e) => {
                return legacy_kept(format!(
                    "旧 job {} 无法无损迁移（本次不迁移任何 job）：{e:#}",
                    job.id
                ))
            }
        }
    }

    // 迁移前备份（拿不到备份就不迁移——回滚保底）。`jobs.json` 始终原样保留。
    let backup = workspace.join(MIGRATION_BACKUP);
    if src.exists() {
        if let Err(e) = std::fs::copy(&src, &backup) {
            return legacy_kept(format!("备份 {} 失败（本次不迁移）：{e}", src.display()));
        }
    }

    // 原子写 task store（失败即整体回退语义：单次落盘，不留半截）。
    if !to_add.is_empty() {
        if let Err(e) = store.add_many(to_add.clone()) {
            return legacy_kept(format!("写入 task store 失败（本次不迁移）：{e:#}"));
        }
    }

    // 标记：数据已在 store 里，标记写失败**也判 Done**——旧循环绝不能启动（否则双跑）；
    // 下次启动会重跑本函数，按 `legacy_job_id` 去重后只补写标记与备份。
    let marker_written = match write_marker(&marker, jobs.len(), to_add.len()) {
        Ok(()) => true,
        Err(e) => {
            crate::log_to!(
                warn,
                "[migrate:{bot_key}] ⚠️ 迁移标记写入失败（数据已导入，旧 job 循环仍不启动；下次启动补写）：{e:#}"
            );
            false
        }
    };
    MigrateReport::done(to_add.len(), skipped, marker_written)
}

fn legacy_kept(reason: String) -> MigrateReport {
    MigrateReport {
        outcome: MigrateOutcome::LegacyKept(reason),
        imported: 0,
        skipped: 0,
        marker_written: false,
    }
}

/// 读旧 `jobs.json`。文件不存在 = 没有旧任务（合法，返回空表）；存在但解析失败 =
/// **迁移中止**（返回 Err，调用方保留旧循环），绝不静默当空表。
fn read_legacy_jobs(src: &Path) -> Result<Vec<Job>> {
    let text = match std::fs::read_to_string(src) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("读 {} 失败", src.display())),
    };
    serde_json::from_str::<Vec<Job>>(&text)
        .with_context(|| format!("解析 {} 失败（原文件未动，旧 job 循环照旧）", src.display()))
}

fn write_marker(marker: &Path, jobs: usize, imported: usize) -> Result<()> {
    let body = serde_json::json!({
        "migrated_at": crate::chrono_lite::unix_secs(),
        "source": LEGACY_JOBS_FILE,
        "backup": MIGRATION_BACKUP,
        "jobs": jobs,
        "imported": imported,
    })
    .to_string();
    crate::atomic_write_text(marker, &body)
        .with_context(|| format!("写迁移标记 {} 失败", marker.display()))
}

/// 校验一个**已存在**的迁移标记：必须是可解析的 JSON，且带数字型 `migrated_at`
/// （`write_marker` 的产物形态）。只判「可信」，不判 job 数——数字字段即「本进程写的」。
///
/// 任何读失败/解析失败/字段缺失或类型不符都返回 Err，调用方据此把标记**当作不存在**
/// 处理（走导入路径），从而把「受限会话伪造标记 ⇒ 静默搁浅」降级为「照常迁移 + 告警」。
fn validate_marker(marker: &Path) -> Result<()> {
    let text = std::fs::read_to_string(marker)
        .with_context(|| format!("读迁移标记 {} 失败", marker.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("迁移标记 {} 不是合法 JSON", marker.display()))?;
    match value.get("migrated_at") {
        Some(serde_json::Value::Number(n)) if n.as_f64().is_some() => Ok(()),
        _ => bail!(
            "迁移标记 {} 缺 `migrated_at` 数字字段（非本进程写入的标记）",
            marker.display()
        ),
    }
}

/// 一条旧 `job` → `task` 定义（四问里「定义文件与运行态分离」的落点：只转定义，
/// 运行态由 worker 以 Pending 起步）。
///
/// 映射（§D7「原样映射」，不发明语义）：
/// - `JobKind::Once` → `TriggerKind::Once`（`expr` = 时间点）；`Cron` → `Cron`（5 段）。
/// - `prompt` → `payload.prompt`；`note` → `name`（旧列表展示的「原句」）；`chat_id` →
///   `created_by.chat_id`（默认回创建者的事实源）；`role` → `created_by.role`
///   （授权者建的任务迁移后仍走受限剖面）。
/// - `targets` → `delivery.targets`（`bot_key` 空 = 本 bot）。
///
/// **唯一拒绝项**：`targets.len() > 1`。task 模型当前只支持单投递目标
/// （`Task::validate` 硬拒 >1、执行侧 `delivery_target` 只取第一个），多目标任务
/// 无法**无损**表达——迁移宁可整体不做，也不静默降级成「只投第一个」。
pub(crate) fn job_to_task(bot_key: &str, job: &Job) -> Result<Task> {
    if job.targets.len() > 1 {
        bail!(
            "有 {} 个投递目标，而 task 模型只支持单个目标（不静默降级成只投第一个）",
            job.targets.len()
        );
    }
    let (kind, expr) = match job.kind {
        JobKind::Once => (TriggerKind::Once, job.schedule.trim().to_string()),
        JobKind::Cron => (TriggerKind::Cron, job.schedule.trim().to_string()),
    };
    let targets: Vec<TaskTarget> = job
        .targets
        .iter()
        .map(|t| TaskTarget {
            bot_key: t.bot_key.clone(),
            chat_id: t.chat_id.clone(),
        })
        .collect();
    let task = Task {
        schema_version: TASK_SCHEMA_VERSION,
        id: new_id(crate::chrono_lite::unix_secs()),
        name: job.note.clone(),
        legacy_job_id: job.id.clone(),
        bot_key: bot_key.to_string(),
        created_by: CreatedBy {
            // `Job.role` 的 serde default 兼容旧文件（无角色 → Owner），迁移沿用同一口径。
            role: job.role,
            bot_key: bot_key.to_string(),
            chat_id: job.chat_id.clone(),
        },
        payload: TaskPayload {
            kind: PayloadKind::Agent,
            prompt: job.prompt.clone(),
            ..Default::default()
        },
        trigger: TaskTrigger {
            kind,
            expr,
            timezone: String::new(),
        },
        resume_on_boot: DEFAULT_RESUME_ON_BOOT,
        delivery: TaskDelivery {
            targets,
            ..Default::default()
        },
        limits: TaskLimits::default(),
    };
    // 单条先验，错误信息带上 job id（调用方会拼进整体放弃的原因里）。
    task.validate()
        .with_context(|| format!("旧 job {} 迁移后定义不合法", job.id))?;
    Ok(task)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SenderRole;
    use crate::schedule::{Job, JobKind, JobTarget};

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("abb-migrate-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn job(id: &str, kind: JobKind, schedule: &str, prompt: &str, targets: Vec<JobTarget>) -> Job {
        Job {
            id: id.to_string(),
            kind,
            schedule: schedule.to_string(),
            prompt: prompt.to_string(),
            chat_id: "oc_creator".to_string(),
            note: format!("原句 {id}"),
            targets,
            role: SenderRole::Granted,
        }
    }

    fn write_jobs(ws: &Path, jobs: &[Job]) {
        std::fs::write(
            ws.join(LEGACY_JOBS_FILE),
            serde_json::to_string_pretty(jobs).unwrap(),
        )
        .unwrap();
    }

    fn read_tasks(root: &Path, bot: &str) -> Vec<Task> {
        TaskStore::new_at(root, bot).list()
    }

    #[test]
    fn migrates_once_and_cron_losslessly() {
        let ws = tmp("ok-ws");
        let root = tmp("ok-tasks");
        write_jobs(
            &ws,
            &[
                job("job-a", JobKind::Once, "2030-01-02 09:00", "提醒我", vec![]),
                job(
                    "job-b",
                    JobKind::Cron,
                    "30 9 * * 1-5",
                    "写日报",
                    vec![JobTarget {
                        bot_key: "feishu".into(),
                        chat_id: "oc_other".into(),
                    }],
                ),
            ],
        );
        let original = std::fs::read_to_string(ws.join(LEGACY_JOBS_FILE)).unwrap();

        let report = migrate_with_paths(&ws, &root, "b");
        assert!(matches!(report.outcome, MigrateOutcome::Done));
        assert_eq!(report.imported, 2);
        assert_eq!(report.skipped, 0);
        assert!(report.marker_written);
        assert!(
            !report.legacy_job_loop_allowed(),
            "迁移完成后旧循环不得启动"
        );

        let tasks = read_tasks(&root, "b");
        assert_eq!(tasks.len(), 2);
        let a = tasks.iter().find(|t| t.legacy_job_id == "job-a").unwrap();
        assert_eq!(a.trigger.kind, TriggerKind::Once);
        assert_eq!(a.trigger.expr, "2030-01-02 09:00");
        assert_eq!(a.name, "原句 job-a");
        assert_eq!(a.payload.prompt, "提醒我");
        assert_eq!(a.created_by.chat_id, "oc_creator");
        assert_eq!(
            a.created_by.role,
            SenderRole::Granted,
            "授权者任务不得借 owner 执行"
        );
        assert!(a.delivery.targets.is_empty(), "无 --to 的 job 回创建者会话");
        let b = tasks.iter().find(|t| t.legacy_job_id == "job-b").unwrap();
        assert_eq!(b.trigger.kind, TriggerKind::Cron);
        assert_eq!(b.trigger.expr, "30 9 * * 1-5");
        assert_eq!(b.delivery.targets.len(), 1);
        assert_eq!(b.delivery.targets[0].bot_key, "feishu");
        assert_eq!(b.delivery.targets[0].chat_id, "oc_other");

        // 标记 + 备份就位；原文件与备份**都不删**且内容一致。
        assert!(ws.join(MIGRATION_MARKER).exists());
        let backup = std::fs::read_to_string(ws.join(MIGRATION_BACKUP)).unwrap();
        assert_eq!(backup, original);
        assert_eq!(
            std::fs::read_to_string(ws.join(LEGACY_JOBS_FILE)).unwrap(),
            original
        );
    }

    #[test]
    fn migration_is_idempotent_across_restarts() {
        let ws = tmp("idem-ws");
        let root = tmp("idem-tasks");
        write_jobs(
            &ws,
            &[job("job-a", JobKind::Once, "2030-01-02 09:00", "p", vec![])],
        );

        assert_eq!(migrate_with_paths(&ws, &root, "b").imported, 1);
        let second = migrate_with_paths(&ws, &root, "b");
        assert!(matches!(second.outcome, MigrateOutcome::Done));
        assert_eq!(second.imported, 0);
        assert_eq!(second.skipped, 0, "标记存在 → 直接收工，不走去重");
        assert_eq!(read_tasks(&root, "b").len(), 1, "重复启动不重复导入");
    }

    /// 崩溃窗口：数据已写入 task store 但标记没落盘 → 下次启动必须靠 `legacy_job_id`
    /// 去重，并补写标记；**绝不产生第二份**。
    #[test]
    fn crash_between_write_and_marker_dedups_on_rerun() {
        let ws = tmp("crash-ws");
        let root = tmp("crash-tasks");
        write_jobs(
            &ws,
            &[job("job-a", JobKind::Cron, "0 9 * * *", "p", vec![])],
        );
        assert_eq!(migrate_with_paths(&ws, &root, "b").imported, 1);

        std::fs::remove_file(ws.join(MIGRATION_MARKER)).unwrap(); // 模拟标记丢失
        let again = migrate_with_paths(&ws, &root, "b");
        assert!(matches!(again.outcome, MigrateOutcome::Done));
        assert_eq!(again.imported, 0);
        assert_eq!(again.skipped, 1);
        assert!(again.marker_written);
        assert_eq!(read_tasks(&root, "b").len(), 1, "去重后不得产生第二份");
        assert!(
            !again.legacy_job_loop_allowed(),
            "数据已在 store，旧循环仍不得启动"
        );
    }

    /// 迁移并捕获告警 sink（验证「伪造标记」路径不静默）。返回 (报告, 告警全文)。
    fn migrate_warned(ws: &Path, root: &Path, bot: &str) -> (MigrateReport, String) {
        let mut sink: Vec<u8> = Vec::new();
        let report = migrate_with_paths_warned(ws, root, bot, &mut sink);
        (report, String::from_utf8_lossy(&sink).into_owned())
    }

    /// 合法标记（本进程 `write_marker` 的形态）→ 幂等收工：不导入、旧循环不启动、无告警。
    #[test]
    fn valid_marker_short_circuits_and_blocks_legacy_loop() {
        let ws = tmp("marker-ws");
        let root = tmp("marker-tasks");
        write_jobs(
            &ws,
            &[job("job-a", JobKind::Once, "2030-01-02 09:00", "p", vec![])],
        );
        std::fs::write(
            ws.join(MIGRATION_MARKER),
            serde_json::json!({"migrated_at": 1_790_000_000u64}).to_string(),
        )
        .unwrap();

        let (report, warns) = migrate_warned(&ws, &root, "b");
        assert!(matches!(report.outcome, MigrateOutcome::Done));
        assert_eq!(report.imported, 0);
        assert!(read_tasks(&root, "b").is_empty(), "合法标记存在不导入");
        assert!(
            !report.legacy_job_loop_allowed(),
            "防双跑：标记存在旧循环不启动"
        );
        assert!(warns.is_empty(), "合法标记不该告警，实测：{warns}");
    }

    /// #326 F1b：伪造的无效标记（受限会话可 Write）**不得**让旧 job 静默搁浅——
    /// 必须发 loud 告警并**照常走导入路径**，导入成功后再重写为合法标记。
    #[test]
    fn forged_invalid_marker_is_ignored_import_proceeds_with_warning() {
        for forged in [
            "{}",                               // 空对象：无 migrated_at
            "not json at all",                  // 非 JSON
            r#"{"migrated_at": "1700000000"}"#, // 类型不符（字符串）
            r#"{"jobs": 2}"#,                   // 缺字段
        ] {
            let ws = tmp("forge-ws");
            let root = tmp("forge-tasks");
            write_jobs(
                &ws,
                &[job("job-a", JobKind::Once, "2030-01-02 09:00", "p", vec![])],
            );
            std::fs::write(ws.join(MIGRATION_MARKER), forged).unwrap();

            let (report, warns) = migrate_warned(&ws, &root, "b");
            assert!(
                matches!(report.outcome, MigrateOutcome::Done),
                "伪造标记 {forged:?} 应走导入后 Done"
            );
            assert_eq!(
                report.imported, 1,
                "伪造标记 {forged:?} 不得静默搁浅：必须照常导入"
            );
            assert!(
                !report.legacy_job_loop_allowed(),
                "数据已入 store，旧循环不得启动"
            );
            assert_eq!(read_tasks(&root, "b").len(), 1);
            assert!(
                warns.contains("无效") && warns.contains("视为未迁移"),
                "伪造标记 {forged:?} 必须发 loud 告警，实测：{warns:?}"
            );
            // 收尾把标记重写为可信形态（自愈），后续启动可正常短路。
            assert!(validate_marker(&ws.join(MIGRATION_MARKER)).is_ok());
            assert!(
                migrate_with_paths(&ws, &root, "b").imported == 0,
                "重写后幂等"
            );
        }
    }

    /// 伪造标记 + 无 jobs.json：仍判未迁移、告警、并补写合法标记（不静默）。
    #[test]
    fn forged_marker_without_jobs_file_rewrites_marker_with_warning() {
        let ws = tmp("forge-empty-ws");
        let root = tmp("forge-empty-tasks");
        std::fs::write(ws.join(MIGRATION_MARKER), "{}").unwrap();

        let (report, warns) = migrate_warned(&ws, &root, "b");
        assert!(matches!(report.outcome, MigrateOutcome::Done));
        assert!(!report.legacy_job_loop_allowed());
        assert!(warns.contains("视为未迁移"), "实测：{warns:?}");
        assert!(
            validate_marker(&ws.join(MIGRATION_MARKER)).is_ok(),
            "标记已自愈"
        );
    }

    /// 校验函数本身：合法/非法形态直判（与迁移路径解耦的单元锁）。
    #[test]
    fn validate_marker_accepts_only_trusted_shape() {
        let ws = tmp("vm-ws");
        let m = ws.join(MIGRATION_MARKER);
        std::fs::write(&m, r#"{"migrated_at": 1}"#).unwrap();
        assert!(validate_marker(&m).is_ok());
        for bad in [
            r#"{}"#,
            r#"{"migrated_at": "1"}"#,
            "nope",
            r#"{"migrated_at": null}"#,
        ] {
            std::fs::write(&m, bad).unwrap();
            assert!(validate_marker(&m).is_err(), "{bad:?} 应判无效");
        }
        assert!(
            validate_marker(&ws.join("absent.marker")).is_err(),
            "缺失标记判无效"
        );
    }

    #[test]
    fn no_jobs_file_still_finalizes_and_blocks_legacy_loop() {
        let ws = tmp("empty-ws");
        let root = tmp("empty-tasks");
        let report = migrate_with_paths(&ws, &root, "b");
        assert!(matches!(report.outcome, MigrateOutcome::Done));
        assert_eq!(report.imported, 0);
        assert!(ws.join(MIGRATION_MARKER).exists());
        assert!(!ws.join(MIGRATION_BACKUP).exists(), "无源文件不产生备份");
        assert!(!report.legacy_job_loop_allowed());
    }

    /// 解析失败 = 迁移中止：不写任何东西、不落标记、旧循环照旧（原文件未动）。
    #[test]
    fn unparsable_jobs_file_keeps_legacy_loop() {
        let ws = tmp("bad-ws");
        let root = tmp("bad-tasks");
        std::fs::write(ws.join(LEGACY_JOBS_FILE), "{ not json").unwrap();

        let report = migrate_with_paths(&ws, &root, "b");
        assert!(report.legacy_job_loop_allowed(), "读不出来就必须保留旧循环");
        assert!(matches!(report.outcome, MigrateOutcome::LegacyKept(_)));
        assert!(!ws.join(MIGRATION_MARKER).exists());
        assert!(!ws.join(MIGRATION_BACKUP).exists());
        assert!(read_tasks(&root, "b").is_empty());
        assert_eq!(
            std::fs::read_to_string(ws.join(LEGACY_JOBS_FILE)).unwrap(),
            "{ not json"
        );
    }

    /// 原子性：只要有一条 job 无法无损迁移（多目标），**整体放弃**——合法的那条也不写盘，
    /// 不留半截。旧循环继续跑原文件。
    #[test]
    fn unmigratable_job_aborts_the_whole_migration() {
        let ws = tmp("multi-ws");
        let root = tmp("multi-tasks");
        write_jobs(
            &ws,
            &[
                job("job-ok", JobKind::Once, "2030-01-02 09:00", "p", vec![]),
                job(
                    "job-multi",
                    JobKind::Cron,
                    "0 9 * * *",
                    "p2",
                    vec![
                        JobTarget {
                            bot_key: String::new(),
                            chat_id: "oc_a".into(),
                        },
                        JobTarget {
                            bot_key: "feishu".into(),
                            chat_id: "oc_b".into(),
                        },
                    ],
                ),
            ],
        );

        let report = migrate_with_paths(&ws, &root, "b");
        assert!(report.legacy_job_loop_allowed());
        assert_eq!(report.imported, 0);
        assert!(
            read_tasks(&root, "b").is_empty(),
            "不留半截：合法的那条也不写"
        );
        assert!(!ws.join(MIGRATION_MARKER).exists());
        assert!(ws.join(LEGACY_JOBS_FILE).exists());
    }

    /// 回滚保底：备份写不进去（这里把备份路径占成目录）→ 不迁移，原文件/无标记。
    #[test]
    fn backup_failure_blocks_migration() {
        let ws = tmp("bak-ws");
        let root = tmp("bak-tasks");
        write_jobs(
            &ws,
            &[job("job-a", JobKind::Once, "2030-01-02 09:00", "p", vec![])],
        );
        std::fs::create_dir_all(ws.join(MIGRATION_BACKUP)).unwrap(); // 让 copy 失败

        let report = migrate_with_paths(&ws, &root, "b");
        assert!(report.legacy_job_loop_allowed());
        assert!(read_tasks(&root, "b").is_empty());
        assert!(!ws.join(MIGRATION_MARKER).exists());
    }

    /// 转换期校验：日历非法的一次性时间点（旧 `parse_once` 放行、task 的严格校验拒绝）
    /// 必须走「整体放弃」而不是静默丢。
    #[test]
    fn calendar_invalid_once_job_aborts_migration() {
        let ws = tmp("cal-ws");
        let root = tmp("cal-tasks");
        write_jobs(
            &ws,
            &[job(
                "job-bad",
                JobKind::Once,
                "2030-02-30 09:00",
                "p",
                vec![],
            )],
        );
        let report = migrate_with_paths(&ws, &root, "b");
        assert!(report.legacy_job_loop_allowed());
        assert!(read_tasks(&root, "b").is_empty());
    }

    #[test]
    fn job_to_task_rejects_multiple_targets() {
        let j = job(
            "job-multi",
            JobKind::Cron,
            "0 9 * * *",
            "p",
            vec![
                JobTarget {
                    bot_key: String::new(),
                    chat_id: "a".into(),
                },
                JobTarget {
                    bot_key: String::new(),
                    chat_id: "b".into(),
                },
            ],
        );
        assert!(job_to_task("b", &j).is_err());
    }
}
