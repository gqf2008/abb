//! 定时任务 —— 存储 + 中文 cron 解析 + 下次触发计算（本地时区，UTC+8）。
//! 自然语言由 claude --json-schema 解析成 Job{kind,time|cron,prompt}（见 bridge.rs 拦截逻辑），
//! 本模块只负责：持久化（~/.agent-bridge/workspaces/<bot>/jobs.json）+ 判断「到点了吗/下次何时」。
//! 不引 cron 解析依赖：标准 5 段中文 cron（分 时 日 月 周）手写求值，够用且可测。
//!
//! ## 定义与计时账分离（`jobs.json` / `jobs-state.json`）
//!
//! [`Job`] 是**定义**（谁来跑、跑什么），落在 `jobs.json`；「这一分钟已经认领过」是
//! **运行态**，落在同目录的 `jobs-state.json`（`job_id → 上次认领的分钟点`）。
//! 分两个文件是刻意的，与 `task_store` 的 `tasks.json` / `tasks-state.json` 同一口径：
//!
//! - CLI 在**别的进程**往 `jobs.json` 追加任务（本模块靠 mtime 热重载看到它），而计时账每
//!   分钟都要写。合成一个文件就得「读-改-写」整表，两边并发写会互相覆盖——轻则丢一个刚
//!   建的任务，重则把计时改回旧值导致重跑。
//! - 计时账写失败只影响「重启后可能重跑一次」，不会污染用户的定义文件。
//!
//! ## 为什么计时账必须落盘
//!
//! 旧实现只靠调度循环内存里的 `last_min` + `running` 集合去重：进程重启就会把「本分钟已
//! 触发过」忘掉 ⇒ 同一分钟内重启会**重跑一次**（cron 的副作用可能是发消息、跑 agent、烧
//! token）。落盘后 [`JobStore::claim_due`] 是「认领即记账」：返回给调用方的任务已经记为
//! 「这一分钟用掉了」，重启看不到重跑的窗口。
//!
//! 停摆之后怎么办：进程挂了几小时再起来，我们不重放整段历史（那会一次爆出成百上千轮），
//! 而是**只补最近一次**错过的分钟点（[`CRON_CATCHUP_MAX_MINUTES`]，与 `task_run` 的
//! `CRON_CATCHUP_WINDOW_SECS` = 48h 同量级）。补跑与首跑共用同一条认领路径，所以「补一轮」
//! 之后同一轮内不会再补。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

/// 本地时区偏移（小时）。服务跑在 Asia/Shanghai（UTC+8），与 chrono_lite 一致。
const TZ_OFFSET_SECS: i64 = 8 * 3600;

/// cron 逾期补跑的扫描窗口（分钟）= 48 小时。
///
/// 与 `task_run::CRON_CATCHUP_WINDOW_SECS` 对齐：换句柄执行位的长任务会占住分钟点，进程
/// 重启也会空过一段。窗口内的最近一个匹配分钟补跑一次；更早的不再追溯（否则一个
/// `* * * * *` 任务在停机一天后要连续跑 1440 轮）。
///
/// 这个值也是「表达式永不命中」（如 `0 0 30 2 *`）时每个 tick 的扫描上界——2880 次纯整数
/// 比较，微秒量级（`fired` 就落在窗口内时实际扫描区间更短）。
const CRON_CATCHUP_MAX_MINUTES: i64 = 48 * 60;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
    Once, // 一次性：time = "YYYY-MM-DD HH:MM"
    Cron, // 周期：cron = "分 时 日 月 周"
}

/// 定时任务消息的 `InboundMsg.prompt_tag`。**生产者（run_job）与消费者
/// （harness 的排队丢弃、桥的停止词路由）必须共用同一个值**——硬编码两处时
/// 任一处改动都会静默断链（停止词返回 false、落回透传且 job 照跑，无编译报错）。
pub const JOB_PROMPT_TAG: &str = "job_message";

/// 一个投递目标（#21 定时任务多目标）。bot_key 空 = 本 bot（创建任务的那个 bot）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobTarget {
    #[serde(default)]
    pub bot_key: String,
    pub chat_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub kind: JobKind,
    /// kind=once 时为 "YYYY-MM-DD HH:MM"（本地）；kind=cron 时为 5 段 cron
    pub schedule: String,
    pub prompt: String,
    pub chat_id: String,
    /// 创建时的原始自然语言（便于 /定时任务 列表展示）
    #[serde(default)]
    pub note: String,
    /// 多投递目标（#21）：空 = 只发 chat_id（旧行为）；非空 = 结果向每个目标各发一份。
    /// 目标 bot_key 空 = 本 bot；跨 bot 目标可把任务结果投到其它 bot 的会话。
    #[serde(default)]
    pub targets: Vec<JobTarget>,
    /// 创建者角色：执行时按此走受限/全权限 agent 分支（授权者建的任务不得借 owner
    /// 全权限执行）。serde default 兼容旧 jobs.json（无角色 → Owner，与现状一致）。
    #[serde(default)]
    pub role: crate::config::SenderRole,
}

pub struct JobStore {
    path: PathBuf,
    /// 计时账文件（`jobs-state.json`，与 `path` 同目录）。见模块文档「定义与计时账分离」。
    state_path: PathBuf,
    data: Mutex<Vec<Job>>,
    /// `job_id → 上次认领的分钟点`（unix 秒 / 60）。落盘于 [`JobStore::state_path`]。
    ///
    /// 「认领」而非「跑完」：`claim_due` 一返回就记账，所以执行中用尽轮次/崩溃都不会让
    /// 同一分钟重跑。代价是「认领了但没跑成」会丢这一轮——与 `task_run` 的认领语义一致
    /// （宁可少跑一次，不可重跑一次：cron 副作用可能是发消息/改文件）。
    fired: Mutex<HashMap<String, i64>>,
    /// 上次加载 jobs.json 时的文件 mtime。CLI（codex）在另一进程往 jobs.json 写新任务，
    /// service 内存里这份不会自动看到 → 这里在 claim_due 前按 mtime 热重载，避免漏触发。
    loaded_mtime: Mutex<Option<std::time::SystemTime>>,
}

impl JobStore {
    /// 按指定路径构造（生产入口委托 / 测试在隔离根下构造）。
    pub(crate) fn at(path: PathBuf) -> JobStore {
        let data = if path.exists() {
            fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        // 计时账读失败按空表：最坏是「重启后可能重跑一次」——不能因此不启动调度。
        let state_path = path.with_file_name("jobs-state.json");
        let fired = fs::read_to_string(&state_path)
            .ok()
            .and_then(|t| serde_json::from_str::<HashMap<String, i64>>(&t).ok())
            .unwrap_or_default();
        let mtime = fs::metadata(&path).ok().and_then(|m| m.modified().ok());
        JobStore {
            path,
            state_path,
            data: Mutex::new(data),
            fired: Mutex::new(fired),
            loaded_mtime: Mutex::new(mtime),
        }
    }

    /// 若 jobs.json 的 mtime 比上次加载新（CLI 在别的进程改了），重新读盘。
    fn refresh(&self) {
        let cur = fs::metadata(&self.path)
            .ok()
            .and_then(|m| m.modified().ok());
        let stale = { *self.loaded_mtime.lock().unwrap() != cur };
        if !stale {
            return;
        }
        if let Ok(text) = fs::read_to_string(&self.path) {
            if let Ok(data) = serde_json::from_str::<Vec<Job>>(&text) {
                *self.data.lock().unwrap() = data;
            }
        }
        *self.loaded_mtime.lock().unwrap() = cur;
    }

    fn save_locked(&self, data: &[Job]) {
        // 用共享原子写（uuid 唯一 tmp），而不是手写 `jobs.json.tmp` + rename：
        //
        // 1. **符号链接写面**（卡 `abb-proc-sysctl-nb-20261028` NB-3）：`fs::write` 是
        //    O_CREAT|O_TRUNC、**会跟随符号链接**。固定 tmp 名时，受限会话预放
        //    `jobs.json.tmp → ~/.zshrc`，我们下一次写定义就跟随链接覆盖工作区外的真实
        //    文件。guard 的 `.tmp` 剪尾只拦 Write/Edit 工具这条路，写入侧没堵——现在补上。
        //    uuid 唯一 tmp 名让「预放」从根上不可预测。
        // 2. **并发写方互踩**（#137）：固定 tmp 名时进程 A rename 后进程 B 拿不到 tmp
        //    → ENOENT。CLI 与应用双写 jobs.json 正是这个场景。
        if let Ok(text) = serde_json::to_string_pretty(data) {
            let _ = crate::atomic_write_text(&self.path, &text);
        }
    }

    /// 计时账落盘（原子替换）。写失败不影响本进程内的判定（内存已更新）。
    ///
    /// 但**写失败必须留痕**（评审 F1）：三个 `if let Ok` 套住的旧写法把序列化/写盘/替换
    /// 三步的失败全吞了，而这种降级的后果正是本卡要消除的那类缺陷——计时账没落盘就等于
    /// 「重启后会重跑本轮」（cron 的副作用可能是发消息、跑 agent、烧 token），却没有任何
    /// 日志线索，排障时只能靠猜。
    ///
    /// 频率上界：本函数只在真有认领/回收时被调用，而调度循环本身按分钟去重
    /// （`service.rs` 的 `last_min`）⇒ 至多每分钟一次告警，不必额外限频。
    /// 用 `crate::log!` 而非 `tracing::warn!`：根二进制没有装 tracing subscriber，
    /// `tracing::warn!` 在本仓等于静默丢弃。
    fn save_state_locked(&self, fired: &HashMap<String, i64>) {
        if let Err(e) = self.try_save_state(fired) {
            crate::log!("[schedule] ⚠️ 计时账写盘失败（重启后可能重跑本轮）: {e}");
        }
    }

    /// 计时账落盘本体，**返回结果**而不是自己吞掉。
    ///
    /// 拆出来是为了可测：[`save_state_locked`] 的告警走 `crate::log!`（写 stdout，测试
    /// 无法捕获），只断言「claim_due 不 panic + 返回值对」的话，把实现退回静默旧写法
    /// 用例**照绿**（实测如此）——那就只是装饰。返回 `Err` 后，「失败被识别到」这件事
    /// 就能被单测钉住；告警文案本身仍不在测试覆盖内（与 `pending.rs::save` 同款形状）。
    fn try_save_state(&self, fired: &HashMap<String, i64>) -> std::io::Result<()> {
        use std::io::Error;
        let text = serde_json::to_string_pretty(fired)
            .map_err(|e| Error::other(format!("序列化失败: {e}")))?;
        // 同 [`Self::save_locked`]：改用 uuid 唯一 tmp 的共享原子写，堵住
        // 「预放 `jobs-state.json.tmp` 符号链接 → 写入时跟随覆盖工作区外文件」这条
        // 写入侧旁路（卡 `abb-proc-sysctl-nb-20261028` NB-3；guard 只拦工具面）。
        crate::atomic_write_text(&self.state_path, &text)
            .map_err(|e| Error::other(format!("写字计账 {} 失败: {e}", self.state_path.display())))
    }

    pub fn remove(&self, id: &str) -> bool {
        let mut d = self.data.lock().unwrap();
        let before = d.len();
        d.retain(|j| j.id != id);
        let changed = d.len() != before;
        if changed {
            self.save_locked(&d);
        }
        drop(d);
        // 定义没了，计时账也不该留着（否则长跑服务里已删任务的键会永久堆积）。
        let mut fired = self.fired.lock().unwrap();
        if fired.remove(id).is_some() {
            self.save_state_locked(&fired);
        }
        changed
    }

    /// 认领此刻到点的任务（**认领即记账，返回值不要再拿 `is_due` 复核**）。
    ///
    /// 调用方负责执行后对 once 任务 remove；cron 任务留在表里，下一分钟重新判定。
    ///
    /// 记账所以必须在这里做（而不是在调用方）：判据是「这一分钟有没有用过」，而调用方
    /// 是异步派发的——若把记账交给调用方，认领与记账之间的崩溃/重启窗口就能重跑一次。
    ///
    /// 顺带回收已跑过的 once 任务（[`is_retired_once`]）：以前靠 run_job 收尾 remove，
    /// 中途崩掉的 once 任务会永久留在 jobs.json 里（现在它再也不会被认领，就变成僵尸项）。
    pub fn claim_due(&self, now: &DateTime) -> Vec<Job> {
        self.refresh(); // 先按 mtime 热重载（CLI 可能在别进程加了新任务）
        let now_min = now.to_unix().div_euclid(60);
        let mut claimed = Vec::new();
        let mut retired = Vec::new();
        let mut fired = self.fired.lock().unwrap();
        {
            let mut jobs = self.data.lock().unwrap();
            // 清掉已删任务的陈旧计时项：本函数是唯一写 entries 的地方。
            if fired.keys().any(|id| !jobs.iter().any(|j| &j.id == id)) {
                fired.retain(|id, _| jobs.iter().any(|j| &j.id == id));
            }
            for j in jobs.iter() {
                // 先查「已完成的一轮性任务」：认证过一次就不再认领，且从定义表里收走。
                // 顺序重要：必须在 claimable 前，否则 fired 落盘后就会永远停在表里。
                if is_retired_once(j, fired.get(&j.id).copied()) {
                    retired.push(j.id.clone());
                    continue;
                }
                if !claimable(j, fired.get(&j.id).copied(), now, now_min) {
                    continue;
                }
                fired.insert(j.id.clone(), now_min);
                claimed.push(j.clone());
            }
            if !retired.is_empty() {
                jobs.retain(|j| !retired.contains(&j.id));
                self.save_locked(&jobs);
            }
        }
        for id in &retired {
            fired.remove(id);
        }
        if !claimed.is_empty() || !retired.is_empty() {
            self.save_state_locked(&fired);
        }
        claimed
    }
}

/// 一轮性任务是否「已跑过（或被认领过）」⇒ 可从定义表收走（纯函数，单测点）。
///
/// 语义是 **at-most-once**：一次性的提醒/动作宁可少跑一次，不可重跑（重跑意味着用户
/// 收到两条一模一样的提醒，或是同一份改动的第二次应用）。旧实现在跑完才 remove，
/// 认领与 remove 之间崩溃就会重跑——这就是本函数要堵的窗口。
///
/// 表达式非法时不收（`None`）：留给用户看见并修正，而不是默默清掉。
fn is_retired_once(job: &Job, fired: Option<i64>) -> bool {
    if job.kind != JobKind::Once {
        return false;
    }
    let Some(t) = parse_once(&job.schedule) else {
        return false;
    };
    let due_min = t.to_unix().div_euclid(60);
    fired.is_some_and(|f| f >= due_min)
}

/// 该任务此刻可否被**认领**（纯函数，单测点）。
///
/// 与 [`Job::is_due`] 的区别：`is_due` 只回答「现在到点了吗」，是时间函数；本函数还看
/// **计时账**（`fired`），是「这一分钟该不该真跑」的判据。之所以单独成函数而不是写进
/// `claim_due` 的循环：循环里只能测「跑一遍看结果」，而这里的每条分支（首次/本分钟已认领/
/// 补跑窗口/未到点）都能被直接钉住。
///
/// - `once`：到点且**没认领过**。`once` 不靠 remove 去重——调用方在跑完才 remove，
///   中间崩溃/超时就会重跑；按 `due_min` 记账后，认领过一次就再也不认领。
/// - `cron`：上次认领之后、到现在之间的区间里有一个匹配分钟（含当前）。扫 `now_min`
///   往后最多 [`CRON_CATCHUP_MAX_MINUTES`]，找到就认领（补跑**一次**）；本轮认领后
///   `fired = now_min`，所以同一轮内不会连续补多个。
/// - `fired` 缺失（新任务或计时账丢失）：只看当前分钟，**不回溯**——否则刚建的
///   `* * * * *` 会把 48 小时全补一遍。
fn claimable(job: &Job, fired: Option<i64>, now: &DateTime, now_min: i64) -> bool {
    match job.kind {
        JobKind::Once => {
            // 「到点」的定义复用 [`Job::is_due`]，不在这里重写一遍（重写就会与表达式
            // 语义漂移：两个地方各判一次，改一处就静默不一致）。
            if !job.is_due(now) {
                return false;
            }
            let Some(t) = parse_once(&job.schedule) else {
                return false;
            };
            let due_min = t.to_unix().div_euclid(60);
            // 已认领过这个（或更晚的）分钟点 → 不重复
            !fired.is_some_and(|f| f >= due_min)
        }
        JobKind::Cron => {
            let Some(c) = CronExpr::parse(&job.schedule) else {
                return false;
            };
            let Some(fired) = fired else {
                // 首次（新任务 / 计时账丢失）：只看当前分钟，不回溯
                return job.is_due(now);
            };
            // 「本分钟已认领 / 时钟回拨」（`fired >= now_min`）不需要显式守卫：此时
            // `lo > now_min`，`lo..=now_min` 是空区间，下面的 `any` 返回 false。**不要去
            // 这里补 `if now_min <= fired { return false }`**——它与空区间等价（评审实测：
            // 整段删除的变异下全部用例照绿），留着只会让读者误以为去重靠那道守卫。
            //
            // `saturating_add` 而不是 `+ 1`（卡 `abb-proc-sysctl-nb-20261028` NB-1）：
            // `fired` 来自 `jobs-state.json`，是磁盘上的数据而不是本进程算出来的不变量。
            // 若它被写成 `i64::MAX`（人工编辑/位翻转/未来的 bug），`fired + 1` 在 debug 档
            // 直接 panic（调度循环死掉，所有定时任务静默停摆），在 release 档（本仓
            // `[profile.release]` 未设 `overflow-checks`，跟随默认=关）wrap 成 `i64::MIN`
            // ⇒ `lo = now_min - 2880` ⇒ 区间非空 ⇒ **重复认领一个已经跑过的分钟点**。
            // 饱和加法把异常值导回「空区间」这一侧，正好是我们要的 fail-closed。
            let lo = fired
                .saturating_add(1)
                .max(now_min - CRON_CATCHUP_MAX_MINUTES);
            (lo..=now_min)
                .rev()
                .any(|m| c.matches(&DateTime::from_unix(m * 60)))
        }
    }
}

/// 极简本地日期时间（UTC+8）。只用 到 分 级判断，秒忽略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub weekday: u32, // 0=周日 … 6=周六
}

impl DateTime {
    /// 从 unix 秒（UTC）转本地 DateTime。
    pub fn from_unix(secs: i64) -> DateTime {
        let local = secs + TZ_OFFSET_SECS;
        let days = local.div_euclid(86400);
        let secs_of_day = local.rem_euclid(86400);
        let hour = (secs_of_day / 3600) as u32;
        let minute = ((secs_of_day % 3600) / 60) as u32;
        let (year, month, day) = civil_from_days(days);
        // 1970-01-01 是周四(4)。days 相对该日偏移。
        let weekday = ((days % 7 + 7 + 4) % 7) as u32;
        DateTime {
            year,
            month,
            day,
            hour,
            minute,
            weekday,
        }
    }

    pub fn now() -> DateTime {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Self::from_unix(secs)
    }

    /// 转回 unix 秒（本地→UTC），用于 once 任务比较。
    pub fn to_unix(self) -> i64 {
        let days = days_from_civil(self.year, self.month, self.day);
        days * 86400 + (self.hour as i64) * 3600 + (self.minute as i64) * 60 - TZ_OFFSET_SECS
    }
}

// Howard Hinnant 的 civil calendar 换算（公历，公知算法）。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let mp = ((m + 9) % 12) as i64; // [0,11]
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

impl Job {
    /// 该任务在 `now` 是否到点（**纯时间判据，不含计时账**）。
    ///
    /// 只回答「现在是不是又到点了」；能不能真跑由 [`JobStore::claim_due`] 结合
    /// 计时账决定。保留本函数是为了可独立单测 cron/once 的表达式语义，
    /// 也可给「下次何时」类展示用——**但不要在调度循环里直接拿它筛任务**：
    /// 它不知道这一分钟是不是已经跑过。
    pub fn is_due(&self, now: &DateTime) -> bool {
        match self.kind {
            JobKind::Once => match parse_once(&self.schedule) {
                Some(t) => t.to_unix() <= now.to_unix(),
                None => false,
            },
            JobKind::Cron => match CronExpr::parse(&self.schedule) {
                Some(c) => c.matches(now),
                None => false,
            },
        }
    }
}

/// 解析 "YYYY-MM-DD HH:MM"（本地）为 DateTime。weekday 由换算得出。
pub fn parse_once(s: &str) -> Option<DateTime> {
    let s = s.trim();
    let (date_part, time_part) = s.split_once(' ').or_else(|| s.split_once('T'))?;
    let mut dparts = date_part.split('-');
    let year: i64 = dparts.next()?.parse().ok()?;
    let month: u32 = dparts.next()?.parse().ok()?;
    let day: u32 = dparts.next()?.parse().ok()?;
    let mut tparts = time_part.trim().split(':');
    let hour: u32 = tparts.next()?.parse().ok()?;
    let minute: u32 = tparts.next().unwrap_or("0").parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let weekday = ((days % 7 + 7 + 4) % 7) as u32;
    Some(DateTime {
        year,
        month,
        day,
        hour,
        minute,
        weekday,
    })
}

/// 标准 5 段中文 cron：分 时 日 月 周。支持 * 、数字、逗号列表、-范围、/步进。
pub struct CronExpr {
    minute: Field,
    hour: Field,
    day: Field,
    month: Field,
    weekday: Field,
}

#[derive(Debug)]
struct Field {
    any: bool,
    vals: Vec<u32>,
}

impl CronExpr {
    pub fn parse(s: &str) -> Option<CronExpr> {
        let parts: Vec<&str> = s.split_whitespace().collect();
        if parts.len() != 5 {
            return None;
        }
        Some(CronExpr {
            minute: parse_field(parts[0], 0, 59)?,
            hour: parse_field(parts[1], 0, 23)?,
            day: parse_field(parts[2], 1, 31)?,
            month: parse_field(parts[3], 1, 12)?,
            weekday: parse_field(parts[4], 0, 7)?, // 0/7 都视为周日
        })
    }

    pub fn matches(&self, dt: &DateTime) -> bool {
        field_match(&self.minute, dt.minute)
            && field_match(&self.hour, dt.hour)
            && field_match(&self.day, dt.day)
            && field_match(&self.month, dt.month)
            && {
                // cron 里 7 = 周日；本地 weekday 周日=0
                let wd = dt.weekday;
                field_match(&self.weekday, wd) || (wd == 0 && field_match(&self.weekday, 7))
            }
    }
}

fn field_match(f: &Field, v: u32) -> bool {
    f.any || f.vals.contains(&v)
}

/// 解析单段：* | a | a,b,c | a-b | */n | a-b/n
fn parse_field(s: &str, lo: u32, hi: u32) -> Option<Field> {
    if s == "*" {
        return Some(Field {
            any: true,
            vals: vec![],
        });
    }
    let mut vals = Vec::new();
    for part in s.split(',') {
        // 处理步进 /n
        let (range_part, step) = match part.split_once('/') {
            Some((r, st)) => (r, st.parse::<u32>().ok()?.max(1)),
            None => (part, 1),
        };
        let (start, end) = if range_part == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range_part.split_once('-') {
            (a.parse().ok()?, b.parse().ok()?)
        } else {
            let v: u32 = range_part.parse().ok()?;
            (v, v)
        };
        if start > end || start < lo || end > hi {
            return None;
        }
        let mut v = start;
        while v <= end {
            vals.push(v);
            v += step;
        }
    }
    Some(Field { any: false, vals })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn dt(y: i64, mo: u32, d: u32, h: u32, mi: u32) -> DateTime {
        parse_once(&format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")).unwrap()
    }

    #[test]
    fn civil_roundtrip() {
        // 已知：1970-01-01 是周四
        let e = DateTime::from_unix(0);
        assert_eq!((e.year, e.month, e.day), (1970, 1, 1));
        assert_eq!(e.weekday, 4);
        assert_eq!((e.hour, e.minute), (8, 0)); // UTC+8
                                                // to_unix 往返
        assert_eq!(e.to_unix(), 0);
        let x = dt(2026, 8, 5, 9, 30);
        let back = DateTime::from_unix(x.to_unix());
        assert_eq!(
            (back.year, back.month, back.day, back.hour, back.minute),
            (2026, 8, 5, 9, 30)
        );
    }

    #[test]
    fn parse_once_validates() {
        assert!(parse_once("2026-08-05 09:00").is_some());
        assert!(parse_once("2026-13-05 09:00").is_none()); // 月超界
        assert!(parse_once("2026-08-05 25:00").is_none()); // 时超界
        assert!(parse_once("随便").is_none());
    }

    #[test]
    fn cron_every_day_9am() {
        let c = CronExpr::parse("0 9 * * *").unwrap();
        assert!(c.matches(&dt(2026, 8, 5, 9, 0)));
        assert!(!c.matches(&dt(2026, 8, 5, 9, 1)));
        assert!(!c.matches(&dt(2026, 8, 5, 10, 0)));
    }

    #[test]
    fn cron_step_and_list() {
        let every30 = CronExpr::parse("*/30 * * * *").unwrap();
        assert!(every30.matches(&dt(2026, 8, 5, 9, 0)));
        assert!(every30.matches(&dt(2026, 8, 5, 9, 30)));
        assert!(!every30.matches(&dt(2026, 8, 5, 9, 15)));

        let list = CronExpr::parse("0 9,18 * * *").unwrap();
        assert!(list.matches(&dt(2026, 8, 5, 9, 0)));
        assert!(list.matches(&dt(2026, 8, 5, 18, 0)));
        assert!(!list.matches(&dt(2026, 8, 5, 12, 0)));
    }

    #[test]
    fn cron_weekday() {
        // 2026-08-05 是周三(3)
        let wd = dt(2026, 8, 5, 9, 0).weekday;
        assert_eq!(wd, 3);
        let mon9 = CronExpr::parse("0 9 * * 1").unwrap();
        assert!(!mon9.matches(&dt(2026, 8, 5, 9, 0))); // 周三不匹配周一
        let wed9 = CronExpr::parse("0 9 * * 3").unwrap();
        assert!(wed9.matches(&dt(2026, 8, 5, 9, 0)));
    }

    #[test]
    fn cron_rejects_bad() {
        assert!(CronExpr::parse("0 9 * *").is_none()); // 只有4段
        assert!(CronExpr::parse("61 9 * * *").is_none()); // 分超界
        assert!(CronExpr::parse("0 9 * * * *").is_none()); // 6段
    }

    /// P5 调整（语义不变）：原来借 `job_from_parsed` 当构造器，该函数随「jobs.json
    /// 只读化」删除（见 docs/task-model.md §D7）→ 改为直接构造 `Job`。断言的仍是
    /// 同一件事：once 到点即 due、错过后仍补跑。
    #[test]
    fn once_is_due() {
        let j = Job {
            id: "a".into(),
            kind: JobKind::Once,
            schedule: "2026-08-05 09:00".into(),
            prompt: "看邮件".into(),
            chat_id: "oc_x".into(),
            note: "原句".into(),
            targets: Vec::new(),
            role: crate::config::SenderRole::Owner,
        };
        assert!(!j.is_due(&dt(2026, 8, 5, 8, 59)));
        assert!(j.is_due(&dt(2026, 8, 5, 9, 0)));
        assert!(j.is_due(&dt(2026, 8, 5, 9, 1)));
    }

    /// 测试用隔离根（绝不碰真实 HOME 下的 `~/.agent-bridge`）。
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("abb-sched-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cron_job(id: &str, expr: &str) -> Job {
        Job {
            id: id.into(),
            kind: JobKind::Cron,
            schedule: expr.into(),
            prompt: "p".into(),
            chat_id: "oc_x".into(),
            note: String::new(),
            targets: Vec::new(),
            role: crate::config::SenderRole::Owner,
        }
    }

    fn once_job(id: &str, at: &str) -> Job {
        Job {
            id: id.into(),
            kind: JobKind::Once,
            schedule: at.into(),
            prompt: "p".into(),
            chat_id: "oc_x".into(),
            note: String::new(),
            targets: Vec::new(),
            role: crate::config::SenderRole::Owner,
        }
    }

    /// 写 jobs.json + （可选）种子计时账，再按生产路径构造 store。
    fn store_at(dir: &std::path::Path, jobs: &[Job], fired: &[(&str, i64)]) -> JobStore {
        let path = dir.join("jobs.json");
        std::fs::write(&path, serde_json::to_string_pretty(jobs).unwrap()).unwrap();
        let state: HashMap<String, i64> = fired.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        std::fs::write(
            dir.join("jobs-state.json"),
            serde_json::to_string_pretty(&state).unwrap(),
        )
        .unwrap();
        JobStore::at(path)
    }

    /// 同一分钟内重复认领必须为空（旧实现靠内存 `last_min`，重启就丢）。
    #[test]
    fn claim_is_idempotent_within_the_same_minute() {
        let dir = temp_dir("same-min");
        let s = store_at(&dir, &[cron_job("a", "0 9 * * *")], &[]);
        let t = dt(2026, 8, 5, 9, 0);
        assert_eq!(s.claim_due(&t).len(), 1, "到点应认领");
        assert_eq!(s.claim_due(&t).len(), 0, "同一分钟不得重复认领");
        // 模拟重启：计时账在盘上，所以新实例也不重跑
        let s2 = JobStore::at(dir.join("jobs.json"));
        assert_eq!(
            s2.claim_due(&t).len(),
            0,
            "重启后同一分钟不得重跑（这就是落盘要解决的问题）"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 停机错过分钟点：恢复后**补跑一次**，且只补一次。
    #[test]
    fn cron_catches_up_one_missed_point_then_stops() {
        let dir = temp_dir("catchup");
        // 昨天 9:00 跑过（进程随后挂了），今天 9:10 才起来。
        let yesterday9 = dt(2026, 8, 4, 9, 0).to_unix().div_euclid(60);
        let s = store_at(&dir, &[cron_job("a", "0 9 * * *")], &[("a", yesterday9)]);
        let t = dt(2026, 8, 5, 9, 10);
        assert_eq!(s.claim_due(&t).len(), 1, "错过的 9:00 应补跑一次");
        assert_eq!(s.claim_due(&t).len(), 0, "补跑不得在同一轮内重复");
        assert_eq!(
            s.claim_due(&dt(2026, 8, 5, 9, 11)).len(),
            0,
            "补跑后该分钟的其它时刻不再触发"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// NB-3 的另一半（评审 N2）：`save_locked`（定义文件）与 `try_save_state`（计时账）
    /// 是两条独立的写入路径，只测一条不能证明另一条也安全。这里命中 `save_locked`：
    /// 预放 `jobs.json.tmp` 符号链接，再用 `remove` 触发一次定义表落盘。
    #[test]
    fn save_locked_does_not_follow_preseeded_symlink() {
        let dir = temp_dir("symlink-jobs");
        let victim = dir.join("victim2.txt");
        std::fs::write(&victim, "ORIGINAL").unwrap();
        let evil_tmp = dir.join("jobs.json.tmp");
        std::os::unix::fs::symlink(&victim, &evil_tmp).unwrap();

        let s = store_at(
            &dir,
            &[cron_job("a", "0 9 * * *"), cron_job("b", "0 10 * * *")],
            &[],
        );
        // remove 走 save_locked（定义表真的变了）——不是空写。
        assert!(s.remove("b"), "删掉一个任务以触发 save_locked");

        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "ORIGINAL",
            "预置的 jobs.json.tmp 符号链接不得被跟随"
        );
        let on_disk = std::fs::read_to_string(dir.join("jobs.json")).unwrap();
        assert!(on_disk.contains("\"a\""), "定义表应正常落盘：{on_disk}");
        assert!(!on_disk.contains("\"b\""), "被删的任务不应还在：{on_disk}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// NB-1（卡 `abb-proc-sysctl-nb-20261028`）：计时账里的 `fired` 是**磁盘数据**，
    /// 不是本进程算出来的不变量。异常值（`i64::MAX` / `i64::MAX - 1`）不得让调度循环
    /// panic，也不得 wrap 成负数后重复认领。
    ///
    /// 改前（`fired + 1`）两个编译档两种坏：debug 档 `attempt to add with overflow`
    /// （调度循环死掉，所有定时任务静默停摆）；release 档（本仓未设 overflow-checks）
    /// wrap 成 `i64::MIN` ⇒ `lo = now_min - 2880` ⇒ 区间非空 ⇒ **重复认领已跑过的分钟点**。
    #[test]
    fn extreme_fired_value_neither_panics_nor_reclaims() {
        for extreme in [i64::MAX, i64::MAX - 1] {
            let dir = temp_dir(&format!("extreme-{extreme}"));
            // 高频表达式：任何「区间非空」的误判都会立刻变成一个认领。
            let s = store_at(&dir, &[cron_job("a", "* * * * *")], &[("a", extreme)]);
            let got = s.claim_due(&dt(2026, 8, 5, 9, 0));
            assert!(
                got.is_empty(),
                "fired={extreme} 必须被饱和加法导回空区间（不认领），实际认领 {} 条",
                got.len()
            );
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    /// NB-3（卡 `abb-proc-sysctl-nb-20261028`）：原子写必须用**不可预测**的 tmp 名。
    ///
    /// `fs::write` 是 O_CREAT|O_TRUNC、会跟随符号链接：固定 tmp 名（`jobs-state.json.tmp`）
    /// 时，受限会话预放 `jobs-state.json.tmp → 工作区外的真实文件`，我们下一次落盘就跟随
    /// 链接覆盖它。guard 的 `.tmp` 剪尾只拦 Write/Edit 工具这条路，写入侧靠这个用例守。
    #[test]
    fn atomic_write_does_not_follow_preseeded_symlink() {
        let dir = temp_dir("symlink");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "ORIGINAL").unwrap();
        // 预放固定 tmp 名为指向受害者的符号链接（模拟受限会话的预置）。
        let evil_tmp = dir.join("jobs-state.json.tmp");
        std::os::unix::fs::symlink(&victim, &evil_tmp).unwrap();

        let s = store_at(&dir, &[cron_job("a", "0 9 * * *")], &[]);
        assert_eq!(
            s.claim_due(&dt(2026, 8, 5, 9, 0)).len(),
            1,
            "认领并落盘计时账"
        );

        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "ORIGINAL",
            "预置的 jobs-state.json.tmp 符号链接不得被跟随（否则工作区外文件被覆盖）"
        );
        // 计时账本身要真的写进去了
        let state = std::fs::read_to_string(dir.join("jobs-state.json")).unwrap();
        assert!(state.contains("\"a\""), "计时账应正常落盘：{state}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 新建任务**不得**回溯：否则刚建的 `* * * * *` 会把 48 小时全补一遍。
    #[test]
    fn new_job_never_backfills_history() {
        let dir = temp_dir("no-backfill");
        let s = store_at(&dir, &[cron_job("a", "0 9 * * *")], &[]);
        assert_eq!(
            s.claim_due(&dt(2026, 8, 5, 12, 0)).len(),
            0,
            "无计时账时只看当前分钟"
        );
        assert_eq!(s.claim_due(&dt(2026, 8, 5, 9, 0)).len(), 1, "到点才认领");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 高频表达式在停机后也只补一轮，不把整段窗口刷出来。
    #[test]
    fn every_minute_job_catches_up_only_once() {
        let dir = temp_dir("every-min");
        let nine = dt(2026, 8, 5, 9, 0).to_unix().div_euclid(60);
        let s = store_at(&dir, &[cron_job("a", "* * * * *")], &[("a", nine)]);
        assert_eq!(s.claim_due(&dt(2026, 8, 5, 9, 5)).len(), 1, "只补最近一轮");
        assert_eq!(s.claim_due(&dt(2026, 8, 5, 9, 5)).len(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// F3 的等价点：`fired >= now_min`（本分钟已认领 / 时钟回拨）必须不认领。
    ///
    /// 拦截实际靠的是**空区间**——`lo = (fired + 1).max(…)` 在 `fired >= now_min` 时必然
    /// `lo > now_min`，`lo..=now_min` 为空。原来那行 `if now_min <= fired { return false }`
    /// 已按评审建议删除（它整段删除后全部用例照绿 ⇒ 不影响任何可观察行为，只误导读者）。
    /// 本用例把那个性质钉在一个位置上：谁要是把 `lo` 算错（比如退回 `fired.max(…)`），
    /// 这里立刻红。
    #[test]
    fn cron_not_claimed_at_or_before_fired_minute() {
        let dir = temp_dir("fired-guard");
        let nine = dt(2026, 8, 5, 9, 0).to_unix().div_euclid(60);
        let s = store_at(&dir, &[cron_job("a", "* * * * *")], &[("a", nine)]);
        assert_eq!(
            s.claim_due(&dt(2026, 8, 5, 9, 0)).len(),
            0,
            "fired == now_min（本分钟已认领）不得再认领"
        );
        assert_eq!(
            s.claim_due(&dt(2026, 8, 5, 8, 50)).len(),
            0,
            "时钟回拨（fired > now_min）不得去补已跑过的分钟点"
        );
        // 对照：时间正常前进一分钟必须认领——证明上面的 0 不是「表达式不匹配」造成的假绿。
        assert_eq!(s.claim_due(&dt(2026, 8, 5, 9, 1)).len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// once：认领即 at-most-once；下一次认领把它作为已完成项从定义表收走。
    #[test]
    fn once_is_claimed_at_most_once_and_then_retired() {
        let dir = temp_dir("once");
        let s = store_at(&dir, &[once_job("a", "2026-08-05 09:00")], &[]);
        assert_eq!(s.claim_due(&dt(2026, 8, 5, 8, 59)).len(), 0, "未到点");
        assert_eq!(s.claim_due(&dt(2026, 8, 5, 9, 5)).len(), 1, "过期仍补跑");
        assert_eq!(
            s.claim_due(&dt(2026, 8, 5, 9, 6)).len(),
            0,
            "已认领的 once 不得重跑（旧实现在 remove 前崩溃就会重跑）"
        );
        // 定义表已收走 → 重启也没了
        let s2 = JobStore::at(dir.join("jobs.json"));
        assert_eq!(s2.claim_due(&dt(2026, 8, 5, 9, 7)).len(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **恰好在本分钟认领**（fired == due_min）的 once 任务也必须被 retired 收走。
    ///
    /// 这是 [`is_retired_once`] 用 `>=` 而不是 `>` 的理由：变异自验实测——把 `>=` 改成
    /// `>` 后，上面那个「9:05 才认领」的用例**照样绿**（因为 9:05 > 9:00），但「9:00
    /// 准时认领」的任务就会因为 `fired == due_min` 而永远不被收走，永久滞留在
    /// jobs.json 里。本用例专钉这个等价边界。
    #[test]
    fn once_claimed_exactly_on_due_minute_is_still_retired() {
        let dir = temp_dir("once-exact");
        let s = store_at(&dir, &[once_job("a", "2026-08-05 09:00")], &[]);
        assert_eq!(
            s.claim_due(&dt(2026, 8, 5, 9, 0)).len(),
            1,
            "准时应认领（fired == due_min）"
        );
        assert_eq!(s.claim_due(&dt(2026, 8, 5, 9, 1)).len(), 0);
        // 定义表必须已收走（`>` 变异下这里仍是 1 条）
        let raw = std::fs::read_to_string(dir.join("jobs.json")).unwrap();
        let left: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap();
        assert!(
            left.is_empty(),
            "准点认领的 once 必须被 retired 收走，实测剩余 {left:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 计时账与定义分文件：写计时账不会把 CLI 新增的任务抹掉。
    #[test]
    fn state_write_does_not_clobber_jobs_file() {
        let dir = temp_dir("sep");
        let s = store_at(&dir, &[cron_job("a", "0 9 * * *")], &[]);
        assert_eq!(s.claim_due(&dt(2026, 8, 5, 9, 0)).len(), 1);
        // CLI 在另一进程追加任务（mtime 热重载路径）
        let mut jobs = vec![cron_job("a", "0 9 * * *")];
        jobs.push(cron_job("b", "0 10 * * *"));
        std::fs::write(
            dir.join("jobs.json"),
            serde_json::to_string_pretty(&jobs).unwrap(),
        )
        .unwrap();
        assert_eq!(
            s.claim_due(&dt(2026, 8, 5, 10, 0)).len(),
            1,
            "CLI 新增的任务要被认领到（计时账写盘不得覆盖定义）"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// F1：计时账**写盘失败必须留痕**且不影响认领结果。
    ///
    /// 写不下去 ⇒ 重启后重跑本轮（cron 副作用可能是发消息/跑 agent/烧 token）；旧实现把
    /// 「序列化 / 写盘 / rename」三层 `if let Ok` 静默套在一起，这种降级在日志里完全不可见。
    /// 这里把目录权限改成 0o500 造真实写失败，钉两件事：
    /// ① [`JobStore::try_save_state`] 返回 `Err`（失败被**识别到**，不是静默吞掉）；
    /// ② `claim_due` 不得 panic、必须照常返回认领结果（告警不影响调度）。
    /// 告警文案本身不过测（`crate::log!` 写 stdout，测试捕获不到）。
    #[cfg(unix)]
    #[test]
    fn state_write_failure_is_loud_but_claim_still_returns() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("state-readonly");
        let s = store_at(&dir, &[cron_job("a", "0 9 * * *")], &[]);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        // 探测权限是否真生效：root 无视目录权限位，那样本用例测不到「写失败」这一支。
        if std::fs::write(dir.join(".probe"), "x").is_ok() {
            let _ = std::fs::remove_file(dir.join(".probe"));
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::remove_dir_all(&dir).ok();
            eprintln!("跳过：当前用户可写只读目录（疑似 root），造不出写盘失败");
            return;
        }
        let t = dt(2026, 8, 5, 9, 0);
        let claimed = s.claim_due(&t);
        // 关键断言（先于任何后续清理做）：失败必须被**识别到**。若实现退回「三层 if let Ok
        // 静默吞掉」的旧写法，这一行会变成 Ok(()) 而立刻红——否则本用例就只能证明
        // 「claim_due 不 panic」，而旧静默实现下同样成立（实测照绿），等于装饰。
        let detected = s.try_save_state(&HashMap::new());
        // 先恢复权限：否则后面任一条断言失败，清理步骤都会用一个次生错误把真因淡掉。
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let err = detected.expect_err("只读目录下的写盘失败必须返回 Err（不能静默）");
        // 错误上下文要带出**可定位的信息**，才能排障。注意不能断言固定 tmp 名：
        // 现在走 `crate::atomic_write_text`（uuid 唯一 tmp，卡 NB-3），错误里带的是目标
        // 路径 + 随机 tmp。断言留在「目标文件名」上，与 tmp 命名策略解耦。
        assert!(
            err.to_string().contains("jobs-state.json"),
            "错误上下文应带出目标路径（排障靠它），实得：{err}"
        );
        assert_eq!(claimed.len(), 1, "计时账写失败不得影响认领结果");
        assert_eq!(claimed[0].id, "a");
        // 盘上计时账仍是空的（写失败了）—— 重启场景下这就是「会重跑本轮」的直接证据
        let on_disk = std::fs::read_to_string(dir.join("jobs-state.json")).unwrap();
        assert!(
            serde_json::from_str::<HashMap<String, i64>>(&on_disk)
                .unwrap()
                .is_empty(),
            "写失败的计时账不应落盘，实得 {on_disk}"
        );
        // 但内存记账仍然生效：同一进程内同一分钟不重复认领
        assert_eq!(s.claim_due(&t).len(), 0, "内存里的认领记账仍生效");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn job_targets_serde_backward_compat() {
        // 旧 jobs.json 无 targets 字段 → 空，不报错
        let text = r#"{"id":"a","kind":"cron","schedule":"0 9 * * *","prompt":"p","chat_id":"oc","note":"n"}"#;
        let j: Job = serde_json::from_str(text).unwrap();
        assert!(j.targets.is_empty());
        // 旧文件无 role 字段 → 默认 Owner（执行时走全权限，与现状一致）
        assert_eq!(j.role, crate::config::SenderRole::Owner);
        // 有 targets 时往返不丢
        let j2 = Job {
            id: "a".into(),
            kind: JobKind::Cron,
            schedule: "0 9 * * *".into(),
            prompt: "p".into(),
            chat_id: "oc".into(),
            note: "n".into(),
            targets: vec![JobTarget {
                bot_key: "feishu".into(),
                chat_id: "oc_2".into(),
            }],
            role: crate::config::SenderRole::Granted,
        };
        let s = serde_json::to_string(&j2).unwrap();
        let back: Job = serde_json::from_str(&s).unwrap();
        assert_eq!(back.targets.len(), 1);
        assert_eq!(back.targets[0].bot_key, "feishu");
        // role 往返保真（granted 任务执行时走受限分支）
        assert_eq!(back.role, crate::config::SenderRole::Granted);
    }
}
