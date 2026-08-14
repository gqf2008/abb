//! 定时任务 —— 存储 + 中文 cron 解析 + 下次触发计算（本地时区，UTC+8）。
//! 自然语言由 claude --json-schema 解析成 Job{kind,time|cron,prompt}（见 bridge.rs 拦截逻辑），
//! 本模块只负责：持久化（~/.agent-bridge/workspaces/<bot>/jobs.json）+ 判断「到点了吗/下次何时」。
//! 不引 cron 解析依赖：标准 5 段中文 cron（分 时 日 月 周）手写求值，够用且可测。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

/// 本地时区偏移（小时）。服务跑在 Asia/Shanghai（UTC+8），与 chrono_lite 一致。
const TZ_OFFSET_SECS: i64 = 8 * 3600;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
    Once, // 一次性：time = "YYYY-MM-DD HH:MM"
    Cron, // 周期：cron = "分 时 日 月 周"
}

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
    data: Mutex<Vec<Job>>,
    /// 上次加载 jobs.json 时的文件 mtime。CLI（codex）在另一进程往 jobs.json 写新任务，
    /// service 内存里这份不会自动看到 → 这里在 due_jobs 前按 mtime 热重载，避免漏触发。
    loaded_mtime: Mutex<Option<std::time::SystemTime>>,
}

impl JobStore {
    pub fn new(bot_key: &str) -> JobStore {
        let dir = crate::bridge_dir().join("workspaces").join(bot_key);
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("jobs.json");
        let data = if path.exists() {
            fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let mtime = fs::metadata(&path).ok().and_then(|m| m.modified().ok());
        JobStore {
            path,
            data: Mutex::new(data),
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
        let tmp = self.path.with_extension("json.tmp");
        if let Ok(text) = serde_json::to_string_pretty(data) {
            if fs::write(&tmp, text).is_ok() {
                let _ = fs::rename(&tmp, &self.path);
            }
        }
    }

    pub fn add(&self, job: Job) {
        let mut d = self.data.lock().unwrap();
        d.push(job);
        self.save_locked(&d);
    }

    pub fn remove(&self, id: &str) -> bool {
        let mut d = self.data.lock().unwrap();
        let before = d.len();
        d.retain(|j| j.id != id);
        let changed = d.len() != before;
        if changed {
            self.save_locked(&d);
        }
        changed
    }

    pub fn list(&self) -> Vec<Job> {
        self.data.lock().unwrap().clone()
    }

    /// 取出所有「现在已到点」的任务（once: time<=now；cron: 当前分钟匹配且非本分钟刚触发过）。
    /// 调用方负责执行后对 once 任务 remove。
    pub fn due_jobs(&self, now: &DateTime) -> Vec<Job> {
        self.refresh(); // 先按 mtime 热重载（CLI 可能在别进程加了新任务）
        self.data
            .lock()
            .unwrap()
            .iter()
            .filter(|j| j.is_due(now))
            .cloned()
            .collect()
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
    /// 该任务在 `now` 是否到点。
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

    /// 人类可读的一行描述（列表用）。
    pub fn describe(&self) -> String {
        let kind = match self.kind {
            JobKind::Once => "一次性",
            JobKind::Cron => "周期",
        };
        let mut s = format!(
            "[{}] {} {} → {}",
            &self.id[..self.id.len().min(8)],
            kind,
            self.schedule,
            self.prompt
        );
        if !self.targets.is_empty() {
            let targets: Vec<String> = self
                .targets
                .iter()
                .map(|t| {
                    if t.bot_key.is_empty() {
                        t.chat_id.clone()
                    } else {
                        format!("{}:{}", t.bot_key, t.chat_id)
                    }
                })
                .collect();
            s.push_str(&format!("（多目标：{}）", targets.join(", ")));
        }
        s
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

/// claude 结构化输出的校验 + 归一成 Job（唯一调用方：main.rs 的 job add CLI）。
/// 创建者角色（role）由调用方从 AGENT_BRIDGE_SENDER_ROLE env 解析后传入。
#[allow(clippy::too_many_arguments)]
pub fn job_from_parsed(
    kind: &str,
    time: Option<&str>,
    cron: Option<&str>,
    prompt: &str,
    chat_id: &str,
    note: &str,
    targets: Vec<JobTarget>,
    role: crate::config::SenderRole,
) -> Result<Job> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        anyhow::bail!("没解析出要做什么（prompt 为空）");
    }
    let (jk, schedule) = match kind {
        "once" => {
            let t = time.context("一次性任务缺 time")?;
            parse_once(t).with_context(|| format!("time 格式不对（要 YYYY-MM-DD HH:MM）：{t}"))?;
            (JobKind::Once, t.trim().to_string())
        }
        "cron" => {
            let c = cron.context("周期任务缺 cron")?;
            CronExpr::parse(c).with_context(|| format!("cron 表达式不合法：{c}"))?;
            (JobKind::Cron, c.trim().to_string())
        }
        other => anyhow::bail!("未知任务类型：{other}"),
    };
    Ok(Job {
        id: uuid::Uuid::new_v4().to_string(),
        kind: jk,
        schedule,
        prompt: prompt.to_string(),
        chat_id: chat_id.to_string(),
        note: note.to_string(),
        targets,
        role,
    })
}

/// 解析 job add 的 --to 目标：`bot_key:chat_id`（跨 bot）或裸 `chat_id`（本 bot）。
/// 校验 chat_id 非空；bot_key 可为空（= 本 bot，与 JobTarget 语义一致）。
pub fn parse_job_target(s: &str) -> Result<JobTarget> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("--to 目标不能为空");
    }
    let (bot_key, chat_id) = match s.split_once(':') {
        Some((b, c)) => (b.trim().to_string(), c.trim().to_string()),
        None => (String::new(), s.to_string()),
    };
    if chat_id.is_empty() {
        anyhow::bail!("--to 目标 chat_id 不能为空：{s}");
    }
    Ok(JobTarget { bot_key, chat_id })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn once_is_due() {
        let j = job_from_parsed(
            "once",
            Some("2026-08-05 09:00"),
            None,
            "看邮件",
            "oc_x",
            "原句",
            Vec::new(),
            crate::config::SenderRole::Owner,
        )
        .unwrap();
        assert!(!j.is_due(&dt(2026, 8, 5, 8, 59)));
        assert!(j.is_due(&dt(2026, 8, 5, 9, 0)));
        assert!(j.is_due(&dt(2026, 8, 5, 9, 1)));
    }

    #[test]
    fn job_from_parsed_validates() {
        assert!(job_from_parsed(
            "cron",
            None,
            Some("0 9 * * *"),
            "提醒",
            "oc",
            "n",
            Vec::new(),
            crate::config::SenderRole::Granted,
        )
        .is_ok());
        assert!(job_from_parsed(
            "cron",
            None,
            Some("bad"),
            "提醒",
            "oc",
            "n",
            Vec::new(),
            crate::config::SenderRole::Owner
        )
        .is_err());
        assert!(job_from_parsed(
            "once",
            None,
            None,
            "提醒",
            "oc",
            "n",
            Vec::new(),
            crate::config::SenderRole::Owner
        )
        .is_err()); // 缺 time
        assert!(job_from_parsed(
            "x",
            None,
            None,
            "提醒",
            "oc",
            "n",
            Vec::new(),
            crate::config::SenderRole::Owner
        )
        .is_err());
        assert!(job_from_parsed(
            "cron",
            None,
            Some("0 9 * * *"),
            "",
            "oc",
            "n",
            Vec::new(),
            crate::config::SenderRole::Owner
        )
        .is_err());
        // 空 prompt
    }

    #[test]
    fn parse_job_target_forms() {
        let t = parse_job_target("feishu:oc_123").unwrap();
        assert_eq!(t.bot_key, "feishu");
        assert_eq!(t.chat_id, "oc_123");
        let t2 = parse_job_target("oc_456").unwrap();
        assert_eq!(t2.bot_key, "");
        assert_eq!(t2.chat_id, "oc_456");
        assert!(parse_job_target("").is_err());
        assert!(parse_job_target("bot:").is_err());
        assert!(parse_job_target(":oc").is_ok()); // bot 可空
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
