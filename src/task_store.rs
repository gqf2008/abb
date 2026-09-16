//! 用户可见的「任务」域（#326 / `docs/task-model.md` P2a）：**定义**与**运行态**两套存储。
//!
//! - 定义：`~/.agent-bridge/tasks/<bot>/tasks.json`（低频写，CLI 与 service 都读）
//! - 运行态：`~/.agent-bridge/tasks/<bot>/tasks-state.json`（高频写；**不回写定义文件**，
//!   避免 pid/重启计数把定义文件搅成一团）
//!
//! ## 为什么放在 `tasks/` 而不是 bot 工作区里（安全前提，别挪回去）
//!
//! bot 工作区（`~/.agent-bridge/workspaces/<bot>/`）是**受限会话的可读域**：现有桥状态
//! 文件的保护是「只禁写不禁读」。任务定义含 prompt / 目标会话 / cmd / env，日志含进程
//! 输出——放进工作区就等于任何受限会话 `cat` 一下全拿到，「只限 owner 管理」形同虚设。
//! 所以这两份文件必须在 agent 读域**之外**，只经受权的 `$ABB_BIN task …` 暴露过滤视图。
//!
//! ## 术语（与 `src/tasks.rs` 区分）
//!
//! `src/tasks.rs` 是 service 内部的 async 任务治理（`TaskGovernance`），与用户可见的
//! 「任务」无关；本模块是后者。内部模块改名（`tasks.rs` → `svc_tasks.rs`）见 #326。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

/// 定义文件 schema 版本。读取时高于本版本 → 拒绝（不猜测未来字段语义）。
pub const TASK_SCHEMA_VERSION: u32 = 1;

/// 默认回合预算（秒）：与一次性同步回合的 30 分钟量级对齐；0 = 不限。
pub const DEFAULT_TIMEOUT_SECS: u64 = 30 * 60;
/// 默认单文件日志上限（Q4 待定前的保守值）。
pub const DEFAULT_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// 默认「被进程退出中断后允许自动重跑」的次数（审查 B3）。
///
/// 取 1 而不是 0：ABB 升级/重启打断任务是很常见的，完全不让恢复会让任务白跑；
/// 取 1 而不是更多：重跑的是**整条 prompt**（副作用整体重放），必须有界，
/// 否则「prompt 能把 ABB 跑挂」会变成崩溃—重启—再崩的循环。
pub const DEFAULT_MAX_RESTARTS: u32 = 1;

/// 任务的两种载荷。两条轴的取值一样多，但**执行引擎完全不同**：
/// `Agent` 走 ACP（`buzz::oneshot`），`Proc` 走进程超管（P3 落地）。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PayloadKind {
    #[default]
    Agent,
    Proc,
}

/// 触发方式。`Now` = 立即跑一次（#306 的后台子代理）；其余是 P3/P5 的编排档。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TriggerKind {
    #[default]
    Now,
    Once,
    Cron,
    Interval,
    Keepalive,
}

impl TriggerKind {
    /// 本档是否需要 `expr`（once=时间点，cron=表达式，interval=间隔）。
    pub fn needs_expr(self) -> bool {
        matches!(
            self,
            TriggerKind::Once | TriggerKind::Cron | TriggerKind::Interval
        )
    }

    /// 是否是**可重复触发**的档（跑完一轮后回到可认领状态，而不是终态）。
    /// `keepalive` 也是重复档，但本批尚未支持（见 `task_run` 的认领判定）。
    pub fn is_repeating(self) -> bool {
        matches!(self, TriggerKind::Cron | TriggerKind::Interval)
    }
}

/// 任务 id 必须是**单一文件名组件**：只允许 `[A-Za-z0-9_-]`，长度 1..=64。
///
/// 为什么要硬校验（审查实测）：id 会直接拼进文件路径（`TaskPaths::log_file` / `cancel_file`），
/// 而 `tasks.json` 是**用户可写**的——一个 `../../other/victim` 形式的 id 能让 `task rm`、
/// 孤儿回收、日志回收删到**别的 bot 的目录**（实测把别的 bot 的日志删掉了）。
pub fn validate_task_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("任务 id 不能为空");
    }
    if id.len() > 64 {
        bail!("任务 id 过长（{} 字符，上限 64）", id.len());
    }
    if id == "." || id == ".." {
        bail!("任务 id 不能是 {id:?}（会被当成上级目录）");
    }
    if let Some(bad) = id
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '_' || *c == '-'))
    {
        bail!("任务 id 含非法字符 {bad:?}（只允许字母/数字/下划线/连字符：{id:?}）");
    }
    Ok(())
}

/// 把任意字符串收敛成**安全的文件名组件**（防御纵深：即使某个 id 绕过校验走到路径拼接，
/// 也不可能穿出目录）。非法字符一律替换成 `_`，空串回落 `unnamed`。
pub fn safe_path_component(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "unnamed".to_string()
    } else {
        cleaned
    }
}

/// 删除一条任务的**全部**日志（当前 `id.log` + 轮转 `.1`/`.2`）。
///
/// 统一入口：`task rm`、孤儿回收、保留期回收三处都必须走这里——审查实测「只删当前文件」
/// 会把 `.1/.2` 留成永久孤儿（`task rm` 之后没有任何入口能再看到它们）。
pub fn remove_task_logs(paths: &TaskPaths, id: &str) {
    let base = paths.log_file(id);
    let _ = fs::remove_file(&base);
    for i in 1..8 {
        let _ = fs::remove_file(std::path::PathBuf::from(format!("{}.{i}", base.display())));
    }
}

/// 严格解析 `once` 的 `YYYY-MM-DD HH:MM`：
/// 必须是**恰好两段**、日期三段/时间两段、且日历合法（含闰年 2 月）。
///
/// `schedule::parse_once` 是给 job 用的宽松实现（接受 `2026-02-31`、`09:00:99`、
/// `2026-09-20-extra 09:00` 这类垃圾），task 这里在**登记期**就要挡掉——坏表达式落到运行时
/// 只表现为「永远不触发」，用户看不到任何错误。
pub fn parse_once_strict(expr: &str) -> Option<(i64, u32, u32, u32, u32)> {
    let t = expr.trim();
    let mut fields = t.split_whitespace();
    let date = fields.next()?;
    let time = fields.next()?;
    if fields.next().is_some() {
        return None; // 多余字段（trailing token）
    }
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    if d.next().is_some() {
        return None;
    }
    let mut h = time.split(':');
    let hour: u32 = h.next()?.parse().ok()?;
    let minute: u32 = h.next()?.parse().ok()?;
    if h.next().is_some() {
        return None;
    }
    if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) || hour > 23 || minute > 59 {
        return None;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => return None,
    };
    if day == 0 || day > max_day {
        return None;
    }
    Some((year, month, day, hour, minute))
}

/// 解析 `interval` 的间隔表达式：`30`（纯秒）/ `30s` / `5m` / `2h` / `1d`。
///
/// **硬下限 5 秒**：worker 轮询间隔是 2s，比它更密的周期没有意义，只会把 LLM 打成
/// 忙循环（也是"误配一条 `--every 1s` 就把额度烧光"的护栏）。上限不限（`1d` 也合法）。
pub fn parse_interval_secs(expr: &str) -> Option<u64> {
    const MIN_INTERVAL_SECS: u64 = 5;
    let t = expr.trim();
    if t.is_empty() {
        return None;
    }
    let (num, unit) = match t.chars().last()? {
        's' | 'S' => (&t[..t.len() - 1], 1u64),
        'm' | 'M' => (&t[..t.len() - 1], 60),
        'h' | 'H' => (&t[..t.len() - 1], 3600),
        'd' | 'D' => (&t[..t.len() - 1], 86400),
        c if c.is_ascii_digit() => (t, 1),
        _ => return None,
    };
    let n: u64 = num.trim().parse().ok()?;
    let secs = n.checked_mul(unit)?;
    if secs < MIN_INTERVAL_SECS {
        return None;
    }
    Some(secs)
}

/// 把秒数渲染成人话（`task list/status` 用）。
pub fn human_interval(secs: u64) -> String {
    if secs.is_multiple_of(86_400) {
        format!("{} 天", secs / 86_400)
    } else if secs.is_multiple_of(3_600) {
        format!("{} 小时", secs / 3_600)
    } else if secs.is_multiple_of(60) {
        format!("{} 分钟", secs / 60)
    } else {
        format!("{secs} 秒")
    }
}

/// 载荷：`agent` 用 prompt（+cwd），`proc` 用 cmd（+cwd/env）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TaskPayload {
    pub kind: PayloadKind,
    /// kind=agent：这一轮要做什么。
    #[serde(default)]
    pub prompt: String,
    /// 工作目录（空 = 该 bot 的工作区）。
    #[serde(default)]
    pub cwd: String,
    /// kind=proc：命令 argv（**不用 shell 串**，避免引号/注入面）。
    #[serde(default)]
    pub cmd: Vec<String>,
    /// kind=proc：追加环境变量。用 BTreeMap 保证序列化确定性（同 HashMap 的坑见经验库）。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TaskTrigger {
    pub kind: TriggerKind,
    /// once="YYYY-MM-DD HH:MM" / cron=5 段 / interval=秒数（字符串，便于统一校验）。
    #[serde(default)]
    pub expr: String,
    /// 仅 cron 用；空 = 本机时区（UTC+8）。
    #[serde(default)]
    pub timezone: String,
}

/// 一个投递目标。`bot_key` 空 = 本 bot。**与 `schedule::JobTarget` 同构但不共用类型**：
/// 两者的兼容面不同（job 的 serde 已冻结），共用会让任一侧的演进卡住另一侧。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskTarget {
    #[serde(default)]
    pub bot_key: String,
    pub chat_id: String,
}

/// 投递策略：`targets` 非空则按它投；否则按 `default`（当前只支持 creator）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskDelivery {
    #[serde(default)]
    pub targets: Vec<TaskTarget>,
    /// 缺省目标：`creator` = 回到创建者会话（与 `deliver` 的统一默认一致）。
    #[serde(default = "default_creator")]
    pub default: String,
}

fn default_creator() -> String {
    "creator".to_string()
}

impl Default for TaskDelivery {
    fn default() -> Self {
        TaskDelivery {
            targets: Vec::new(),
            default: default_creator(),
        }
    }
}

/// 创建者身份。`role` 决定执行时走哪个剖面（granted 建的任务不得借 owner 全权限跑）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CreatedBy {
    #[serde(default)]
    pub role: crate::config::SenderRole,
    #[serde(default)]
    pub bot_key: String,
    /// 创建者会话——「默认回创建者」的事实源。空 = 无来源（人工 CLI），此时
    /// `delivery.default == "creator"` 无法解析，执行时须显式目标。
    #[serde(default)]
    pub chat_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskLimits {
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_restarts")]
    pub max_restarts: u32,
    #[serde(default = "default_log_max")]
    pub log_max_bytes: u64,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECS
}
fn default_max_restarts() -> u32 {
    DEFAULT_MAX_RESTARTS
}
fn default_log_max() -> u64 {
    DEFAULT_LOG_MAX_BYTES
}

impl Default for TaskLimits {
    fn default() -> Self {
        TaskLimits {
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            max_restarts: DEFAULT_MAX_RESTARTS,
            log_max_bytes: DEFAULT_LOG_MAX_BYTES,
        }
    }
}

/// 一条任务**定义**。运行态（pid/退出码/重启计数）不在这里，见 [`TaskRuntime`]。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Task {
    #[serde(default = "default_schema")]
    pub schema_version: u32,
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// 归属 bot（权限面：不允许跨 bot 建任务）。
    pub bot_key: String,
    pub created_by: CreatedBy,
    pub payload: TaskPayload,
    pub trigger: TaskTrigger,
    #[serde(default)]
    pub delivery: TaskDelivery,
    #[serde(default)]
    pub limits: TaskLimits,
}

fn default_schema() -> u32 {
    TASK_SCHEMA_VERSION
}

impl Task {
    /// 校验定义自身是否自洽（**不查身份/权限**——那是 CLI 与 guard 的事）。
    ///
    /// 只做「结构上一定跑不起来/一定危险」的拒绝，不做业务判断。
    pub fn validate(&self) -> Result<()> {
        if self.schema_version > TASK_SCHEMA_VERSION {
            bail!(
                "任务定义 schema 版本 {} 高于本程序支持的 {}（请升级 ABB）",
                self.schema_version,
                TASK_SCHEMA_VERSION
            );
        }
        validate_task_id(&self.id)?;
        if self.bot_key.trim().is_empty() {
            bail!("任务缺少 bot_key（任务按 bot 归属，不允许无主）");
        }
        if self.trigger.kind.needs_expr() && self.trigger.expr.trim().is_empty() {
            bail!(
                "触发方式 {:?} 需要 expr（时间/表达式/间隔）",
                self.trigger.kind
            );
        }
        // 表达式**登记期**就要校验：坏表达式留到运行时只会表现为「永远不触发」，
        // 用户看不到任何错误（这类静默失效在本仓库踩过，宁可当场拒绝）。
        // `timezone` 目前**未参与调度**（调度一律按本地 UTC+8）。留着它只会让人以为
        // 写了时区就按那个时区跑——宁可拒绝，也不要有这种静默无效的字段。
        if !self.trigger.timezone.trim().is_empty() {
            bail!(
                "trigger.timezone 暂不支持（当前一律按本地时区调度，收到 {:?}）",
                self.trigger.timezone
            );
        }
        match self.trigger.kind {
            TriggerKind::Once => {
                if parse_once_strict(&self.trigger.expr).is_none() {
                    bail!(
                        "once 需要 `YYYY-MM-DD HH:MM` 形式且日历合法的时间点（收到 {:?}）",
                        self.trigger.expr
                    );
                }
            }
            TriggerKind::Cron => {
                if crate::schedule::CronExpr::parse(&self.trigger.expr).is_none() {
                    bail!(
                        "cron 需要 5 段表达式 `分 时 日 月 周`（收到 {:?}）",
                        self.trigger.expr
                    );
                }
            }
            TriggerKind::Interval => {
                if parse_interval_secs(&self.trigger.expr).is_none() {
                    bail!(
                        "interval 需要 `30`/`30s`/`5m`/`2h`/`1d` 形式且不小于 5 秒（收到 {:?}）",
                        self.trigger.expr
                    );
                }
            }
            // `now` 不该带表达式（带了会被静默忽略）——直接拒绝，别让用户以为设了什么。
            TriggerKind::Now => {
                if !self.trigger.expr.trim().is_empty() {
                    bail!(
                        "trigger=now 不接受 expr（立即任务无需表达式，收到 {:?}）",
                        self.trigger.expr
                    );
                }
            }
            // `keepalive` 要的是「重启恢复 + 信号/退出语义」（Q5/Q14），本批未实现：
            // 放行只会让它在列表里显示「常驻」却永远不触发（静默失效）——显式拒绝。
            TriggerKind::Keepalive => {
                bail!("keepalive 触发档尚未支持（待 Q5/Q14 拍板后再放开）");
            }
        }
        match self.payload.kind {
            PayloadKind::Agent => {
                if self.payload.prompt.trim().is_empty() {
                    bail!("agent 任务需要 prompt");
                }
            }
            PayloadKind::Proc => {
                if self.payload.cmd.is_empty() {
                    bail!("proc 任务需要 cmd（argv 数组）");
                }
                if self.payload.cmd[0].trim().is_empty() {
                    bail!("proc 任务的 cmd[0]（可执行文件）不能为空");
                }
            }
        }
        if self.delivery.targets.is_empty() && self.delivery.default != "creator" {
            bail!(
                "投递目标为空且 default={:?} 无法解析（当前只支持 creator）",
                self.delivery.default
            );
        }
        for t in &self.delivery.targets {
            if t.chat_id.trim().is_empty() {
                bail!("投递目标的 chat_id 不能为空");
            }
            // `bot_key` 留空 = 本 bot（`task add --to` 的缺省写法）；非空则按它投，
            // 跨 bot 由 `Router::deliver` 的「跨会话投递」开关在投递时判定（关着就
            // 拒绝并回源告警——那是开关的既有语义，不在定义校验里越权代替）。
            // 审查 N1 的旧禁令（执行侧忽略 bot_key）已随 #306 的 `--to` 落地解除。
        }
        if self.delivery.targets.len() > 1 {
            bail!(
                "暂不支持多投递目标（给了 {} 个）——当前一条任务只投一个目标",
                self.delivery.targets.len()
            );
        }
        // 审查：log_max_bytes=0 会让 write_log 在文件存在后静默不再写（任务日志是
        // 排障唯一入口，静默不写比报错更坏）→ 直接拒绝，别给「看着像开了」的配置。
        if self.limits.log_max_bytes == 0 {
            bail!("limits.log_max_bytes 不能为 0（日志是排障入口；要禁用请删任务）");
        }
        Ok(())
    }

    /// 展示名：没起名就用 id 前缀，避免列表里出现整串 uuid。
    pub fn display_name(&self) -> String {
        if self.name.trim().is_empty() {
            self.id[..self.id.len().min(12)].to_string()
        } else {
            self.name.clone()
        }
    }

    /// 执行这条任务时的回合预算。
    pub fn timeout(&self) -> Option<std::time::Duration> {
        if self.limits.timeout_secs == 0 {
            None
        } else {
            Some(std::time::Duration::from_secs(self.limits.timeout_secs))
        }
    }
}

/// 生成任务 id：`tk_<YYYYMMDD>_<6 位小写 hex>`（本地时区日期，便于人肉按天找）。
///
/// 随机段来自 uuid v4 的前 3 字节——**不引 rand 依赖**，也不与
/// `deliveries.json` 的 uuid 共享命名空间。
pub fn new_id(now_secs: u64) -> String {
    let local = now_secs + 8 * 3600;
    let (y, mo, d, _, _, _) = crate::chrono_lite::epoch_to_ymd(local);
    let u = uuid::Uuid::new_v4();
    let b = u.as_bytes();
    format!(
        "tk_{y:04}{mo:02}{d:02}_{:02x}{:02x}{:02x}",
        b[0], b[1], b[2]
    )
}

/// 运行态档位。与定义分开存：定义是「要做什么」，运行态是「跑到哪了」。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum TaskStateKind {
    /// 已登记，等待 service 认领。
    #[default]
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

/// 一条任务的运行态投影。`pid` 仅 proc 用；agent 任务没有独立 pid（在 harness 里）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TaskRuntime {
    #[serde(default)]
    pub kind: TaskStateKind,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub started_at: Option<u64>,
    #[serde(default)]
    pub finished_at: Option<u64>,
    #[serde(default)]
    pub last_exit_code: Option<i32>,
    #[serde(default)]
    pub restarts: u32,
    /// 最近一次失败/取消的原因（给人看的一句话）。
    #[serde(default)]
    pub last_error: String,
    /// 最近一次**触发**的 unix 秒（P2b-C 调度记账）。
    ///
    /// 用途有二：① `interval` 任务按它 + 间隔算下次到点；② `cron` 任务按它的**分钟桶**
    /// 去重——worker 每 2s 轮询一次，没有这笔记账同一个 cron 分钟会被反复触发。
    /// 语义是「认领时刻」而不是「跑完时刻」：认领即记账，与 `Running` 状态共同保证
    /// 同一任务不会并发跑两轮。
    #[serde(default)]
    pub last_fired_at: Option<u64>,
}

/// 任务的三个落盘路径（定义 / 运行态 / 日志目录）。
pub struct TaskPaths {
    pub dir: PathBuf,
}

impl TaskPaths {
    /// 生产入口：任务数据落在 `~/.agent-bridge/tasks/<bot>/`。
    pub fn for_bot(bot_key: &str) -> TaskPaths {
        TaskPaths::with_root(crate::bridge_dir().join("tasks"), bot_key)
    }

    /// **注入缝**：根目录由调用方给。测试一律走这里 + temp 目录——绝不能借
    /// [`TaskPaths::for_bot`] 往用户真实的 `~/.agent-bridge` 里写测试数据
    /// （既有经验：单测不得写用户真实运行数据，见 `LESSON_单测不得写用户真实运行数据须拆出注入缝.md`；
    /// 用 Drop 守卫清理治不了 Ctrl-C/OOM 中途退出留下的残留）。
    /// 生产入口只是「解析根目录 + 委托本函数」，两者共享同一段实现。
    pub fn with_root(root: impl AsRef<std::path::Path>, bot_key: &str) -> TaskPaths {
        TaskPaths {
            dir: root.as_ref().join(bot_key),
        }
    }
    pub fn definitions(&self) -> PathBuf {
        self.dir.join("tasks.json")
    }
    pub fn states(&self) -> PathBuf {
        self.dir.join("tasks-state.json")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.dir.join("task-logs")
    }
    pub fn log_file(&self, id: &str) -> PathBuf {
        // 防御纵深：id 即便绕过 `validate_task_id` 也**不可能**穿出目录
        self.logs_dir()
            .join(format!("{}.log", safe_path_component(id)))
    }
    /// 取消请求目录（Q3 的 `task cancel`）。
    ///
    /// **方向必须是 CLI 写、service 读**：运行态的唯一写者是 service 侧的 task
    /// worker（`docs/task-model.md` Q12 的单写者约束）。CLI 只投一个「请求」文件，
    /// 由 worker 在轮询点消费，因此不存在两个进程同时改 `tasks-state.json` 的竞态。
    pub fn cancel_requests_dir(&self) -> PathBuf {
        self.dir.join("cancel-requests")
    }
    pub fn cancel_file(&self, id: &str) -> PathBuf {
        self.cancel_requests_dir().join(safe_path_component(id))
    }
    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("创建任务目录失败：{}", self.dir.display()))
    }
}

/// 定义文件的存储。读写模型与 `JobStore` 一致：内存缓存 + mtime 热重载（CLI 在别的
/// 进程写、service 靠它看到新任务），写盘走「唯一 tmp 名 + rename」的原子替换。
pub struct TaskStore {
    paths: TaskPaths,
    /// 本 store 归属的 bot（目录名）。`add` 校验定义里的 `bot_key` 与它一致：
    /// 否则手改 JSON 就能把任务挂到别的 bot 名下，执行侧按 bot 选的受限/全权限
    /// 剖面也会跟着被带偏（安全审查：剖面判据已改用 worker 的 bot_key，这里是第二道）。
    bot_key: String,
    data: Mutex<Vec<Task>>,
    loaded_mtime: Mutex<Option<std::time::SystemTime>>,
}

impl TaskStore {
    pub fn new(bot_key: &str) -> TaskStore {
        TaskStore::new_at(crate::bridge_dir().join("tasks"), bot_key)
    }

    /// **注入缝**（测试用 temp 根目录）；与 [`TaskStore::new`] 共享全部实现。
    pub fn new_at(root: impl AsRef<std::path::Path>, bot_key: &str) -> TaskStore {
        let paths = TaskPaths::with_root(root, bot_key);
        let _ = paths.ensure();
        let data = read_defs(&paths.definitions()).unwrap_or_default();
        let mtime = mtime_of(&paths.definitions());
        TaskStore {
            paths,
            bot_key: bot_key.to_string(),
            data: Mutex::new(data),
            loaded_mtime: Mutex::new(mtime),
        }
    }

    /// 若定义文件被别的进程改过（mtime 变），重新读盘。
    fn refresh(&self) {
        let cur = mtime_of(&self.paths.definitions());
        if *self.loaded_mtime.lock().unwrap() == cur {
            return;
        }
        if let Some(data) = read_defs(&self.paths.definitions()) {
            *self.data.lock().unwrap() = data;
        }
        *self.loaded_mtime.lock().unwrap() = cur;
    }

    pub fn list(&self) -> Vec<Task> {
        self.refresh();
        self.data.lock().unwrap().clone()
    }

    /// 登记一条任务。校验失败 / id 重复 → Err（不覆盖已有定义）。
    pub fn add(&self, task: Task) -> Result<()> {
        task.validate()?;
        if task.bot_key != self.bot_key {
            bail!(
                "任务归属（{}）与所在 bot 目录（{}）不一致——不接受跨 bot 的任务定义",
                task.bot_key,
                self.bot_key
            );
        }
        self.refresh();
        let mut d = self.data.lock().unwrap();
        if d.iter().any(|t| t.id == task.id) {
            bail!("任务 id 已存在：{}", task.id);
        }
        d.push(task);
        save_json(&self.paths.definitions(), &*d)
    }

    /// 删除一条定义（不动运行态——由调用方决定是否一并清）。返回是否删到了。
    pub fn remove(&self, id: &str) -> bool {
        self.refresh();
        let mut d = self.data.lock().unwrap();
        let before = d.len();
        d.retain(|t| t.id != id);
        if d.len() != before {
            let ok = save_json(&self.paths.definitions(), &*d).is_ok();
            return ok;
        }
        false
    }
}

/// 运行态存储。**只由 service 写**（CLI 只读），因此不需要 CAS：单写者。
pub struct TaskStateStore {
    paths: TaskPaths,
    data: Mutex<BTreeMap<String, TaskRuntime>>,
}

impl TaskStateStore {
    pub fn new(bot_key: &str) -> TaskStateStore {
        TaskStateStore::new_at(crate::bridge_dir().join("tasks"), bot_key)
    }

    /// **注入缝**（测试用 temp 根目录）；与 [`TaskStateStore::new`] 共享全部实现。
    pub fn new_at(root: impl AsRef<std::path::Path>, bot_key: &str) -> TaskStateStore {
        let paths = TaskPaths::with_root(root, bot_key);
        let _ = paths.ensure();
        let data = read_states(&paths.states()).unwrap_or_default();
        TaskStateStore {
            paths,
            data: Mutex::new(data),
        }
    }

    /// 该 bot 的任务路径（日志读写用同一条解析链，测试才能全程留在 temp 里）。
    pub fn paths(&self) -> &TaskPaths {
        &self.paths
    }

    /// 当前有运行态记录的任务 id（清「有状态无定义」的孤儿条目用）。
    pub fn ids(&self) -> Vec<String> {
        self.data.lock().unwrap().keys().cloned().collect()
    }

    pub fn get(&self, id: &str) -> TaskRuntime {
        self.data
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    /// 覆盖写一条运行态并落盘。
    pub fn set(&self, id: &str, rt: TaskRuntime) -> Result<()> {
        let snap = {
            let mut d = self.data.lock().unwrap();
            d.insert(id.to_string(), rt);
            d.clone()
        };
        save_json(&self.paths.states(), &snap)
    }

    /// 删掉一条运行态（删任务时一并清，避免 file 里留孤儿）。
    pub fn remove(&self, id: &str) -> Result<()> {
        let snap = {
            let mut d = self.data.lock().unwrap();
            d.remove(id);
            d.clone()
        };
        save_json(&self.paths.states(), &snap)
    }
}

fn mtime_of(p: &std::path::Path) -> Option<std::time::SystemTime> {
    fs::metadata(p).ok().and_then(|m| m.modified().ok())
}

fn read_defs(p: &std::path::Path) -> Option<Vec<Task>> {
    let text = fs::read_to_string(p).ok()?;
    // 坏文件不静默当空（否则「任务凭空消失」无从排查）——上报到日志，调用方自行兜底。
    match serde_json::from_str::<Vec<Task>>(&text) {
        Ok(v) => {
            // 手改过的定义文件可能带非法 id（会拼进文件路径）→ 加载时挡掉并留痕，
            // 不让它进入 store（否则 rm/GC 会按它拼路径）。
            let (ok, bad): (Vec<Task>, Vec<Task>) =
                v.into_iter().partition(|t| validate_task_id(&t.id).is_ok());
            for t in &bad {
                crate::log!(
                    "[task] 忽略 id 非法的任务定义（{:?}）：id 只能是字母/数字/下划线/连字符",
                    t.id
                );
            }
            Some(ok)
        }
        Err(e) => {
            crate::log!("[task] 定义文件解析失败（{}）：{e}", p.display());
            None
        }
    }
}

fn read_states(p: &std::path::Path) -> Option<BTreeMap<String, TaskRuntime>> {
    let text = fs::read_to_string(p).ok()?;
    match serde_json::from_str::<BTreeMap<String, TaskRuntime>>(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            crate::log!("[task] 运行态文件解析失败（{}）：{e}", p.display());
            None
        }
    }
}

/// 原子落盘：唯一 tmp 名（pid + 进程内自增序号）避免并发共用同一 tmp 名互相踩。
/// 只带 pid 不够——同进程两个线程（store 与 state store 之间也共享命名空间）会撞名。
fn save_json<T: Serialize>(path: &std::path::Path, value: &T) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!(
        "json.tmp{}.{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let text = serde_json::to_string_pretty(value)?;
    fs::write(&tmp, text).with_context(|| format!("写临时文件失败：{}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("替换失败：{}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_task(bot: &str) -> Task {
        Task {
            schema_version: TASK_SCHEMA_VERSION,
            id: "tk_test_000001".to_string(),
            name: "试跑".to_string(),
            bot_key: bot.to_string(),
            created_by: CreatedBy {
                role: crate::config::SenderRole::Owner,
                bot_key: bot.to_string(),
                chat_id: "wx_chat_1".to_string(),
            },
            payload: TaskPayload {
                kind: PayloadKind::Agent,
                prompt: "数一下今天有几个新文件".to_string(),
                cwd: String::new(),
                cmd: Vec::new(),
                env: BTreeMap::new(),
            },
            trigger: TaskTrigger {
                kind: TriggerKind::Now,
                expr: String::new(),
                timezone: String::new(),
            },
            delivery: TaskDelivery::default(),
            limits: TaskLimits::default(),
        }
    }

    #[test]
    fn id_has_expected_shape() {
        // 2026-09-14 00:00:00 UTC = 08:00 本地，日期段按本地时区
        let id = new_id(1_787_000_000);
        let parts: Vec<&str> = id.split('_').collect();
        assert_eq!(parts.len(), 3, "{id}");
        assert_eq!(parts[0], "tk");
        assert_eq!(parts[1].len(), 8, "{id}");
        assert!(parts[1].chars().all(|c| c.is_ascii_digit()), "{id}");
        assert_eq!(parts[2].len(), 6, "{id}");
        assert!(parts[2].chars().all(|c| c.is_ascii_hexdigit()), "{id}");
    }

    #[test]
    fn validate_rejects_structurally_broken_definitions() {
        let mut t = agent_task("b");
        assert!(t.validate().is_ok());

        // 空 prompt 的 agent 任务
        let mut empty_prompt = t.clone();
        empty_prompt.payload.prompt = "   ".to_string();
        assert!(empty_prompt.validate().is_err());

        // proc 任务必须有 argv
        let mut proc_no_cmd = t.clone();
        proc_no_cmd.payload.kind = PayloadKind::Proc;
        proc_no_cmd.payload.cmd = Vec::new();
        assert!(proc_no_cmd.validate().is_err());

        // proc 任务 cmd[0] 空
        let mut proc_blank0 = t.clone();
        proc_blank0.payload.kind = PayloadKind::Proc;
        proc_blank0.payload.cmd = vec!["".to_string()];
        assert!(proc_blank0.validate().is_err());

        // 需要 expr 的触发挡位不给 expr
        let mut cron_no_expr = t.clone();
        cron_no_expr.trigger.kind = TriggerKind::Cron;
        cron_no_expr.trigger.expr = String::new();
        assert!(cron_no_expr.validate().is_err());

        // 无目标且 default 不是 creator
        t.delivery = TaskDelivery {
            targets: Vec::new(),
            default: "someone".to_string(),
        };
        assert!(t.validate().is_err());
    }

    #[test]
    fn validate_rejects_future_schema_and_missing_bot() {
        let mut t = agent_task("b");
        t.schema_version = TASK_SCHEMA_VERSION + 1;
        let e = t.validate().unwrap_err().to_string();
        assert!(e.contains("schema"), "{e}");

        let mut t2 = agent_task("b");
        t2.schema_version = TASK_SCHEMA_VERSION;
        t2.bot_key = "  ".to_string();
        assert!(t2.validate().is_err());
    }

    /// 审查：投递目标与日志上限的边界要在 `validate` 就拒掉——执行侧只投
    /// `targets[0]` 且忽略 `bot_key`，静默截断/投错 bot 比报错糟得多；
    /// `log_max_bytes=0` 则会让写日志在文件已存在后静默不再写。
    #[test]
    fn validate_rejects_unsupported_delivery_and_zero_log_cap() {
        let mut t = agent_task("b");

        t.delivery.targets = vec![
            TaskTarget {
                bot_key: String::new(),
                chat_id: "a".into(),
            },
            TaskTarget {
                bot_key: String::new(),
                chat_id: "b".into(),
            },
        ];
        let e = t.validate().unwrap_err().to_string();
        assert!(e.contains("多投递目标"), "{e}");

        // #306：跨 bot 目标**不再是定义校验的拒绝项**——执行侧（`delivery_target`）
        // 已按 `targets[0].bot_key` 真投，跨会话开关由 Router 在投递时判定。
        t.delivery.targets = vec![TaskTarget {
            bot_key: "other".into(),
            chat_id: "a".into(),
        }];
        assert!(
            t.validate().is_ok(),
            "跨 bot 目标应通过定义校验（旧实现忽略 bot_key 才需要拒绝）"
        );

        // 同 bot 显式写 bot_key 照旧允许
        t.delivery.targets = vec![TaskTarget {
            bot_key: "b".into(),
            chat_id: "a".into(),
        }];
        assert!(t.validate().is_ok());

        // chat_id 为空仍拒绝（无论 bot_key 怎么写）
        t.delivery.targets = vec![TaskTarget {
            bot_key: "other".into(),
            chat_id: "  ".into(),
        }];
        let e = t.validate().unwrap_err().to_string();
        assert!(e.contains("chat_id"), "{e}");

        t.delivery = TaskDelivery::default();
        t.limits.log_max_bytes = 0;
        let e = t.validate().unwrap_err().to_string();
        assert!(e.contains("log_max_bytes"), "{e}");
    }

    /// 审查回归（严重）：任务 id 必须挡目录穿越——id 会拼进日志/取消请求路径，
    /// `../../other/victim` 形式能让 rm/GC 删到别的 bot 目录。
    #[test]
    fn task_id_rejects_path_traversal_and_weird_chars() {
        for bad in [
            "",
            ".",
            "..",
            "../../other/victim",
            "a/b",
            "a\\b",
            "tk x",
            "tk\u{4e2d}",
        ] {
            assert!(
                validate_task_id(bad).is_err(),
                "非法 id 必须被拒：{bad:?}（否则能穿出任务目录删别人的文件）"
            );
        }
        for ok in ["tk_20260916_abcdef", "job-queued-1", "A1_-z"] {
            assert!(validate_task_id(ok).is_ok(), "合法 id 应放行：{ok:?}");
        }

        // **防御纵深**：即便某条非法 id 绕过校验走到路径拼接，也不能穿出目录
        let paths = TaskPaths::with_root("/tmp/abb-id-guard", "b");
        let evil = paths.log_file("../../other/victim");
        assert!(
            evil.starts_with(paths.logs_dir()),
            "日志路径必须仍在 logs_dir 内（实际 {}）",
            evil.display()
        );
        assert!(
            !evil.to_string_lossy().contains(".."),
            "sanitize 后不应残留 ..：{}",
            evil.display()
        );
    }

    /// 审查回归（中）：`once` 登记期必须是**严格**日历校验，宽松解析会放进
    /// `2026-02-31` / `09:00:99` / trailing token 这类"永远不触发"的垃圾。
    #[test]
    fn once_expr_is_strictly_validated() {
        for ok in ["2026-09-20 09:00", "2024-02-29 00:00", " 2026-1-1 9:5 "] {
            assert!(parse_once_strict(ok).is_some(), "合法 once 应通过：{ok:?}");
        }
        for bad in [
            "2026-02-31 09:00",       // 2 月没有 31 号
            "2025-02-29 09:00",       // 非闰年
            "2026-13-01 09:00",       // 月份越界
            "2026-09-20 24:00",       // 小时越界
            "2026-09-20 09:60",       // 分钟越界
            "2026-09-20 09:00:99",    // 多一段
            "2026-09-20-extra 09:00", // 日期里多一段
            "2026-09-20",             // 少时间
            "2026-09-20 09:00 extra", // trailing token
            "下周三 09:00",
        ] {
            assert!(
                parse_once_strict(bad).is_none(),
                "非法 once 必须被拒：{bad:?}"
            );
        }

        // validate 层同样要挡住（这是 CLI/定义文件共用的闸）
        let mut t = agent_task("b");
        t.trigger = TaskTrigger {
            kind: TriggerKind::Once,
            expr: "2026-02-31 09:00".into(),
            ..Default::default()
        };
        assert!(t.validate().is_err(), "非法日历的 once 不得通过 validate");

        // now 不该带 expr；keepalive 未支持；timezone 未实现 —— 三者都必须显式拒绝
        t.trigger = TaskTrigger {
            kind: TriggerKind::Now,
            expr: "30 9 * * *".into(),
            ..Default::default()
        };
        assert!(
            t.validate().is_err(),
            "now 带 expr 必须拒绝（否则被静默忽略）"
        );
        t.trigger = TaskTrigger {
            kind: TriggerKind::Keepalive,
            ..Default::default()
        };
        assert!(t.validate().is_err(), "keepalive 未支持必须显式拒绝");
        t.trigger = TaskTrigger {
            kind: TriggerKind::Cron,
            expr: "0 9 * * *".into(),
            timezone: "Asia/Tokyo".into(),
        };
        assert!(t.validate().is_err(), "timezone 未参与调度必须拒绝");
    }

    /// P2b-C：interval 表达式解析（单位 / 下限 5 秒）。
    #[test]
    fn interval_expr_parses_units_and_enforces_floor() {
        assert_eq!(parse_interval_secs("30"), Some(30), "纯数字 = 秒");
        assert_eq!(parse_interval_secs("30s"), Some(30));
        assert_eq!(parse_interval_secs("5m"), Some(300));
        assert_eq!(parse_interval_secs("2h"), Some(7200));
        assert_eq!(parse_interval_secs("1d"), Some(86400));
        assert_eq!(parse_interval_secs(" 10M "), Some(600), "允许空白与大小写");
        // 下限护栏：比 worker 轮询（2s）还密的周期会把 LLM 打成忙循环
        assert_eq!(parse_interval_secs("4"), None, "小于 5 秒应拒绝");
        assert_eq!(parse_interval_secs("1s"), None);
        assert_eq!(parse_interval_secs("s"), None, "缺数字");
        assert_eq!(parse_interval_secs("5x"), None, "未知单位");
        assert_eq!(parse_interval_secs(""), None);
        assert_eq!(parse_interval_secs("99999999999999999999"), None, "溢出");

        assert_eq!(human_interval(90), "90 秒");
        assert_eq!(human_interval(300), "5 分钟");
        assert_eq!(human_interval(7200), "2 小时");
        assert_eq!(human_interval(86400), "1 天");
    }

    /// P2b-C：坏表达式必须在**登记期**被拒（留到运行时只会表现为「永远不触发」的静默失效）。
    #[test]
    fn validate_rejects_bad_trigger_exprs() {
        let mut t = agent_task("b");

        t.trigger = TaskTrigger {
            kind: TriggerKind::Once,
            expr: "下周三".into(),
            ..Default::default()
        };
        let e = t.validate().unwrap_err().to_string();
        assert!(e.contains("once"), "{e}");

        t.trigger = TaskTrigger {
            kind: TriggerKind::Cron,
            expr: "每天九点".into(),
            ..Default::default()
        };
        let e = t.validate().unwrap_err().to_string();
        assert!(e.contains("cron"), "{e}");

        t.trigger = TaskTrigger {
            kind: TriggerKind::Interval,
            expr: "1s".into(),
            ..Default::default()
        };
        let e = t.validate().unwrap_err().to_string();
        assert!(e.contains("interval"), "{e}");

        // 合法三档都放行
        for (kind, expr) in [
            (TriggerKind::Once, "2026-09-20 09:00"),
            (TriggerKind::Cron, "30 9 * * *"),
            (TriggerKind::Interval, "5m"),
        ] {
            t.trigger = TaskTrigger {
                kind,
                expr: expr.into(),
                ..Default::default()
            };
            assert!(t.validate().is_ok(), "{kind:?} {expr} 应放行");
        }
    }

    #[test]
    fn serde_roundtrip_keeps_every_field() {
        let t = agent_task("bot-x");
        let s = serde_json::to_string(&t).unwrap();
        let back: Task = serde_json::from_str(&s).unwrap();
        assert_eq!(t, back);
        // 未知字段不该出现在产物里
        assert!(!s.contains("state\""), "定义文件不应含运行态：{s}");
    }

    #[test]
    fn old_definitions_without_new_fields_still_parse() {
        // 只有最小字段（模拟早期文件）——靠 serde default 补齐
        let json = r#"{
            "id":"tk_1", "bot_key":"b",
            "created_by":{"chat_id":"c"},
            "payload":{"kind":"agent","prompt":"p"},
            "trigger":{"kind":"now"}
        }"#;
        let t: Task = serde_json::from_str(json).unwrap();
        assert_eq!(t.schema_version, TASK_SCHEMA_VERSION);
        assert_eq!(t.delivery.default, "creator");
        assert_eq!(t.limits.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(t.limits.log_max_bytes, DEFAULT_LOG_MAX_BYTES);
        assert_eq!(t.limits.max_restarts, DEFAULT_MAX_RESTARTS);
        assert_eq!(t.created_by.role, crate::config::SenderRole::Owner);
    }

    #[test]
    fn payload_env_serializes_deterministically() {
        let mut t = agent_task("b");
        t.payload.kind = PayloadKind::Proc;
        t.payload.cmd = vec!["/bin/echo".to_string(), "hi".to_string()];
        for k in ["B", "A", "C"] {
            t.payload.env.insert(k.to_string(), k.to_lowercase());
        }
        let s = serde_json::to_string(&t).unwrap();
        let ia = s.find("\"A\"").unwrap();
        let ib = s.find("\"B\"").unwrap();
        let ic = s.find("\"C\"").unwrap();
        assert!(ia < ib && ib < ic, "env 键序必须确定：{s}");
    }

    /// 任务文件必须在 **bot 工作区之外**——否则受限会话能直接 `cat` 出 prompt/目标/env，
    /// 「只限 owner 管理」被文件系统读取绕过（见模块头）。
    #[test]
    fn task_paths_live_outside_bot_workspace() {
        let bot = "pathcheck";
        let ws = crate::bridge_dir().join("workspaces").join(bot);
        let p = TaskPaths::for_bot(bot);
        assert!(
            !p.definitions().starts_with(&ws),
            "定义文件不能落在工作区内：{}",
            p.definitions().display()
        );
        assert!(
            !p.logs_dir().starts_with(&ws),
            "日志目录不能落在工作区内：{}",
            p.logs_dir().display()
        );
        // 用 Path 逐段比较，**不要写字符串字面量 "/tasks/"**：Windows 的分隔符是 `\`，
        // 字符串断言会在 windows CI 上假红（#328 真实踩到，main 红了 38 分钟）。
        assert_eq!(p.dir, crate::bridge_dir().join("tasks").join(bot));
        assert_eq!(p.definitions().parent(), Some(p.dir.as_path()));
        assert_eq!(p.logs_dir().parent(), Some(p.dir.as_path()));
    }

    #[test]
    fn store_add_get_remove_roundtrip() {
        // 全程 temp 根目录：**不碰用户真实的 ~/.agent-bridge**
        let root = std::env::temp_dir().join(format!("abb-tstore-{}", uuid::Uuid::new_v4()));
        let _ = fs::create_dir_all(&root);
        let bot = "b";
        let store = TaskStore::new_at(&root, bot);
        let t = agent_task(bot);
        let id = t.id.clone();
        store.add(t.clone()).unwrap();
        assert!(store.list().iter().any(|x| x.id == id));
        assert_eq!(store.list().len(), 1);

        // id 重复 → 拒绝，不覆盖
        let e = store.add(t.clone()).unwrap_err().to_string();
        assert!(e.contains("已存在"), "{e}");

        // 校验不过的定义 → 拒绝
        let mut bad = agent_task(bot);
        bad.id = "tk_bad_1".to_string();
        bad.payload.prompt = String::new();
        assert!(store.add(bad).is_err());
        assert_eq!(store.list().len(), 1);

        assert!(store.remove(&id));
        assert!(!store.list().iter().any(|x| x.id == id));
        assert!(!store.remove(&id), "重复删除应为 false");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn state_store_keeps_runtime_out_of_definitions() {
        let root = std::env::temp_dir().join(format!("abb-tstate-{}", uuid::Uuid::new_v4()));
        let bot = "b";
        let paths = TaskPaths::with_root(&root, bot);
        let store = TaskStore::new_at(&root, bot);
        let states = TaskStateStore::new_at(&root, bot);
        let t = agent_task(bot);
        let id = t.id.clone();
        store.add(t).unwrap();

        assert_eq!(states.get(&id).kind, TaskStateKind::Pending);
        states
            .set(
                &id,
                TaskRuntime {
                    kind: TaskStateKind::Running,
                    started_at: Some(123),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(states.get(&id).kind, TaskStateKind::Running);

        // 定义文件里不能出现运行态字段
        let defs = fs::read_to_string(paths.definitions()).unwrap();
        assert!(!defs.contains("started_at"), "运行态不该进定义文件：{defs}");
        // 运行态在独立文件里
        let st = fs::read_to_string(paths.states()).unwrap();
        assert!(st.contains("running"), "{st}");

        states.remove(&id).unwrap();
        assert_eq!(states.get(&id).kind, TaskStateKind::Pending);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn timeout_zero_means_unbounded() {
        let mut t = agent_task("b");
        t.limits.timeout_secs = 0;
        assert!(t.timeout().is_none());
        t.limits.timeout_secs = 7;
        assert_eq!(t.timeout().unwrap().as_secs(), 7);
    }
}
