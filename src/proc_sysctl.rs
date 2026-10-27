//! macOS 原生进程查询（`sysctl` / `kinfo_proc`）：**不 exec 任何外部二进制**。
//!
//! ## 为什么需要它（卡 `abb-task-identity-setuid-20261028`）
//!
//! 执行层（ki）给每个 bash 子进程套 `sandbox-exec`，策略虽说是 allow-default，但实测
//! 沙箱内**任何 setuid 二进制都无法 exec**（`/bin/ps`、`/usr/bin/top`、`/usr/bin/sudo`
//! 一律 126 EPERM），而 macOS 的 `/bin/ps` 正是 setuid root。于是「这个 pid 还是不是
//! 同一个进程」「它是不是僵尸」这些本该无条件可用的能力，在沙箱里全灭——`install.rs` 与
//! `task_identity.rs` 的 3 条单测因此必红（`pid_alive_detects_zombie_as_dead`、
//! `terminate_escalates_to_sigkill_when_graceful_exit_stalls`、
//! `captured_identity_matches_live_process_and_rejects_stale_token`）。
//!
//! `sysctl(2)` 是内核接口，不经过 exec，沙箱内可正常调用；本模块只用它。
//!
//! ## 为什么选 `sysctl(KERN_PROC, KERN_PROC_PID)` 而不用 `proc_pidinfo`
//!
//! 实测（macOS 26 / arm64，同为沙箱内）：
//!
//! | 查询 | 活进程 | 僵尸（已退出未 reap） | 不存在的 pid |
//! | --- | --- | --- | --- |
//! | `proc_pidinfo(PROC_PIDTBSDINFO)` | 136 字节 ✓ | **0 字节，errno=ESRCH** | 0 字节 |
//! | `sysctl(KERN_PROC, KERN_PROC_PID)` | 648 字节 ✓ | **648 字节，`p_stat`=SZOMB** ✓ | 0 字节 |
//!
//! 僵尸判据（`install.rs::pid_alive` 的既有语义要求「僵尸算不存活」）只能靠后者：
//! `proc_pidinfo` 对僵尸直接报 ESRCH，与「进程不存在」不可区分。所以本模块用
//! `kinfo_proc` 一条路取全 `p_stat` + `p_starttime`，命令行另走 `KERN_PROCARGS2`。
//!
//! ## `kinfo_proc` 布局
//!
//! libc 没给 Apple 的 `kinfo_proc`，这里只声明**前缀**（`kp_proc` 到 `p_pid` 为止）。
//! 依据 `<sys/proc.h>` 的 `struct extern_proc`（LP64：arm64 与 x86_64 同布局）：
//!
//! ```c
//! union { struct {...} p_st1; struct timeval __p_starttime; } p_un; // 0..16
//! struct vmspace *p_vmspace;   // 16..24
//! struct sigacts *p_sigacts;   // 24..32
//! int  p_flag;                 // 32..36
//! char p_stat;                 // 36
//! pid_t p_pid;                 // 40（char 后 3 字节对齐填充）
//! ```
//!
//! 该布局有两条自校验：单测用 `offset_of!` 钉死 36/40；运行期用「内核回填的
//! `p_pid` 必须等于我们查的 pid」兜底——布局一旦漂移，`p_pid` 先对不上，函数返回
//! `None`（→ 调用方 fail-closed 的 `Unknown`），而不是把错位的字节当成状态。

use std::ffi::c_void;

/// 取 `kinfo_proc` 用的缓冲区字数（8 字节对齐；1024 字节 ≥ 实测 `sizeof(kinfo_proc)`=648）。
const KINFO_BUF_WORDS: usize = 128;

/// `kern.argmax` 读不到时的保守回退（macOS 默认 1 MiB）。
const DEFAULT_ARG_MAX: usize = 1024 * 1024;

/// 进程状态快照——原生路径能拿到、且本仓用得上的那部分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcStatus {
    /// `kp_proc.p_stat`：`SIDL`=1 / `SRUN`=2 / `SSLEEP`=3 / `SSTOP`=4 / `SZOMB`=5。
    pub state: u8,
    /// `kp_proc.p_un.__p_starttime`（进程启动时刻）。
    pub start_sec: i64,
    pub start_usec: i32,
}

/// `struct extern_proc` 的前缀部分（`kinfo_proc` 的首字段）。
#[repr(C)]
struct KinfoProcPrefix {
    start_tv: libc::timeval,
    p_vmspace: *mut c_void,
    p_sigacts: *mut c_void,
    p_flag: libc::c_int,
    p_stat: u8,
    _pad_stat: [u8; 3],
    p_pid: libc::pid_t,
}

/// 查进程状态快照。`None` = 查不到（进程不存在 / 无权 / 内核没回填任何数据）。
pub(crate) fn status(pid: u32) -> Option<ProcStatus> {
    // 0 不是可查询的 pid；超过 pid_t 上界的值直接拒，避免 as 截断后查错对象。
    if pid == 0 || pid > libc::pid_t::MAX as u32 {
        return None;
    }
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROC,
        libc::KERN_PROC_PID,
        pid as libc::c_int,
    ];
    let mut buf = [0u64; KINFO_BUF_WORDS];
    let mut len = std::mem::size_of_val(&buf);
    // SAFETY: mib 长度与数组一致；buf 按 u64 对齐且长度由 len 精确给出；newp/newlen 为空。
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buf.as_mut_ptr() as *mut c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    // 不存在的 pid：内核返回成功但**回填 0 字节**（实测），必须自己挡掉。
    if len < std::mem::size_of::<KinfoProcPrefix>() {
        return None;
    }
    // SAFETY: 上面已确认内核至少回填了 sizeof(KinfoProcPrefix) 字节，且 buf 对齐足够。
    let kp = unsafe { std::ptr::read(buf.as_ptr() as *const KinfoProcPrefix) };
    // 布局自校验：内核回填的 p_pid 必须就是我们查的那个（见模块头「布局」段）。
    if kp.p_pid != pid as libc::pid_t {
        return None;
    }
    Some(ProcStatus {
        state: kp.p_stat,
        start_sec: kp.start_tv.tv_sec as i64,
        start_usec: kp.start_tv.tv_usec as i32,
    })
}

/// 是不是僵尸（已退出、父进程尚未 reap）。
///
/// **语义与旧实现（`ps -o stat=` 含 'Z'）逐字对齐**：查不到一律返回 `false`（fail-open），
/// 让调用方回落到「`kill(pid, 0)` 成功即存活」——把「读不到」当成僵尸会让看门狗对活着的
/// service 反复重拉。
pub(crate) fn is_zombie(pid: u32) -> bool {
    status(pid).is_some_and(|s| s.state == libc::SZOMB as u8)
}

/// `start_token` 的格式前缀。`task_identity::verify()` 靠它区分「本代采集值（可比对）」与
/// 「升级前落盘的老值（`ps -o lstart=` 文本，无从比对）」：老值一律判 `Unknown`，
/// 不能判 `Dead`——后者意味着「旧代际确实不存在」，而我们只是验不了它。
pub(crate) const TOKEN_PREFIX: &str = "macos-";

/// 进程启动标记（跨重启持久化进 `task-state.json`，用来识别 PID 复用）。
///
/// 格式 `macos-<tv_sec>.<tv_usec:06>`，**取代**了旧的 `ps -o lstart=` 文本
/// （形如 `Fri Nov 15 10:23:45 2024`）：
///
/// - 旧格式只有**秒**粒度，同一秒内 PID 被复用会被误认成同一代际；微秒级把这条消掉。
/// - 旧格式依赖**本地时区**（`localtime`），TZ 变了就会假死；epoch 数字不受 TZ 影响。
/// - 前缀自带来源标签，落盘后一眼能看出是哪条采集路径写的。
///
/// ⚠️ 迁移后果（选定方案 b：接受格式变更，不复刻 `ps` 文本）：老记录与本格式**无法互译**。
/// 处理方式见 `task_identity::verify()` 的迁移守门：
///
/// - 旧进程**已不在**（升级前就崩掉了，最常见）→ `pid_exists` 先判 `Dead` → 自动恢复，
///   升级完全无感知。
/// - 旧进程**还在**（service 硬杀/崩溃后重启，旧 keepalive 作为独立进程组残存）→ 判
///   `Unknown`：不 adopt、**不重拉**，任务落 `Interrupted` 并在日志里写明原因，需要用户
///   重新启用。这是刻意选的一侧：无根据的自动重拉会造出**双实例**（重复投递/重复计费）。
///
/// 正常关停（`finalize_keepalives_on_shutdown` 把 keepalive 落 `Cancelled`）不走这条路径。
pub(crate) fn start_token(pid: u32) -> Option<String> {
    let s = status(pid)?;
    Some(format!("{TOKEN_PREFIX}{}.{:06}", s.start_sec, s.start_usec))
}

/// 完整命令行快照（`KERN_PROCARGS2`），参数以 `\u{1f}` 连接。
///
/// 分隔符刻意与 Linux 分支（`/proc/<pid>/cmdline`）一致：空格连接是有损的（参数本身
/// 含空格时无法还原边界），而这份字符串是**逐字比对**的判据。旧 macOS 实现用的是
/// `ps -o command=`（空格连接），因此升级后老记录同样会对不上——但由于老记录的
/// `start_token` 先被 `task_identity::verify()` 截成 `Unknown`，这里的格式差异不会
/// 变成「假 `Dead`」。
pub(crate) fn command_line(pid: u32) -> Option<String> {
    if pid == 0 || pid > libc::pid_t::MAX as u32 {
        return None;
    }
    let size = arg_max();
    let mut buf = vec![0u8; size];
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut len = size;
    // SAFETY: buf 长度即 len；mib 长度与数组一致；newp/newlen 为空。
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buf.as_mut_ptr() as *mut c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len == 0 || len > size {
        // 别人的进程（EINVAL/EIO）、僵尸、刚退出 → 一律 None，由调用方判 `Unknown`。
        return None;
    }
    buf.truncate(len);
    parse_procargs2(&buf)
}

/// `kern.argmax`：`KERN_PROCARGS2` 的缓冲区上限（sysctl 要求足够大，否则 ENOMEM）。
fn arg_max() -> usize {
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of_val(&value);
    // SAFETY: mib 长度与数组一致；value 是可写的 c_int。
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            &mut value as *mut libc::c_int as *mut c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || value <= 0 {
        return DEFAULT_ARG_MAX;
    }
    value as usize
}

/// 解析 `KERN_PROCARGS2` 的返回块：`int argc` + `exec_path\0` + 填充 `\0` + `argc` 个
/// `\0` 结尾的 argv 字符串 + environ。
///
/// 拆分出来是为了能脱机测边界（argc=0、块被截断、参数含空格/空格字节）。
fn parse_procargs2(buf: &[u8]) -> Option<String> {
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?) as usize;
    // argc=0 无意义（进程至少有 argv[0]）；上界防作弊值把我们带进 environ 区。
    if argc == 0 || argc > 65536 {
        return None;
    }
    let mut i = 4;
    // 跳过 exec path（到第一个 NUL）。
    while i < buf.len() && buf[i] != 0 {
        i += 1;
    }
    // 跳过 exec path 之后的填充 NUL（内核在 path 与 argv 之间留对齐填充）。
    while i < buf.len() && buf[i] == 0 {
        i += 1;
    }
    let mut args = Vec::with_capacity(argc.min(64));
    for _ in 0..argc {
        if i >= buf.len() {
            break;
        }
        let start = i;
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        args.push(String::from_utf8_lossy(&buf[start..i]).into_owned());
        i += 1; // 跨过终止 NUL
    }
    if args.is_empty() {
        return None;
    }
    Some(args.join("\u{1f}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 布局守卫：`kinfo_proc` 前缀的字段偏移必须与 Darwin LP64 的 `extern_proc` 一致。
    /// 判别力：把 `_pad_stat` 去掉，`p_pid` 会落到 37 ⇒ 这条必红（真机则表现为查错对象）。
    #[test]
    fn kinfo_proc_prefix_offsets_match_darwin_abi() {
        assert_eq!(std::mem::offset_of!(KinfoProcPrefix, start_tv), 0);
        assert_eq!(std::mem::offset_of!(KinfoProcPrefix, p_flag), 32);
        assert_eq!(std::mem::offset_of!(KinfoProcPrefix, p_stat), 36);
        assert_eq!(std::mem::offset_of!(KinfoProcPrefix, p_pid), 40);
        assert!(
            std::mem::size_of::<KinfoProcPrefix>() <= std::mem::size_of::<[u64; KINFO_BUF_WORDS]>()
        );
    }

    /// 自身进程：查得到、状态是活的、启动时刻在过去（且不像 1970 年）。
    #[test]
    fn status_of_self_is_alive() {
        let s = status(std::process::id()).expect("本进程必须可查");
        assert_ne!(s.state, libc::SZOMB as u8, "自己不是僵尸");
        assert!(
            (libc::SRUN as u8..=libc::SSTOP as u8).contains(&s.state),
            "p_stat 应是活的档位（SRUN/SSLEEP/SSTOP），实际 {}",
            s.state
        );
        assert!(s.start_sec > 1_600_000_000, "启动时刻应是真实 epoch 秒");
        assert!(!is_zombie(std::process::id()));
    }

    /// 僵尸必须判出来——这是 `install.rs::pid_alive`「僵尸算不存活」的地基。
    ///
    /// 判别力：把 `is_zombie` 改成恒 `false`（或退回 `proc_pidinfo`，它对僵尸报 ESRCH）
    /// ⇒ 循环 5s 后 `false` 断言必红。
    #[test]
    fn zombie_child_reports_szomb() {
        let child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        std::mem::forget(child); // 不 reap：退出后保持 zombie
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen = None;
        while std::time::Instant::now() < deadline {
            if let Some(s) = status(pid) {
                if s.state == libc::SZOMB as u8 {
                    seen = Some(s);
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(seen.is_some(), "僵尸进程必须仍可查询且 p_stat=SZOMB");
        assert!(is_zombie(pid), "僵尸必须被判为僵尸");
        // 顺手钉住「僵尸也能拿到启动标记」（capture 的前半段不因僵尸而断）。
        assert!(start_token(pid).is_some(), "僵尸也应有启动标记");
        // 不 kill（已死），但要让编译器知道 pid 有用
        assert_eq!(seen.unwrap().state, libc::SZOMB as u8);
    }

    /// 命令行：argv 逐字取回，且用 `\u{1f}` 连接（与 Linux 分支同一形状）。
    #[test]
    fn command_line_of_child_is_argv() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("2")
            .spawn()
            .unwrap();
        let pid = child.id();
        let cl = command_line(pid).expect("子进程命令行必须可读");
        assert_eq!(cl, "/bin/sleep\u{1f}2", "argv 用 \\u{{1f}} 连接");
        let _ = child.kill();
        let _ = child.wait();
    }

    /// 启动标记：格式固定、可重复、带微秒（这正是它比 `ps lstart` 强的地方）。
    #[test]
    fn start_token_is_stable_and_microsecond_precise() {
        let pid = std::process::id();
        let a = start_token(pid).expect("本进程必须有启动标记");
        let b = start_token(pid).unwrap();
        assert_eq!(a, b, "同一次启动的标记必须稳定");
        let rest = a.strip_prefix(TOKEN_PREFIX).expect("新格式带 macos- 前缀");
        let (sec, usec) = rest.split_once('.').expect("格式 macos-<sec>.<usec>");
        assert_eq!(sec.parse::<i64>().unwrap().to_string(), sec);
        assert_eq!(usec.len(), 6, "微秒零填充 6 位：{usec}");
    }

    /// 不存在的 pid：`status`/`command_line` 必须给 `None`（调用方据此判 `Unknown`，
    /// 绝不能因为「没数据」就把活进程判死）。
    #[test]
    fn missing_pid_yields_none() {
        let bogus = libc::pid_t::MAX as u32; // 远大于 macOS 的 pid 上界
        assert!(status(bogus).is_none());
        assert!(!is_zombie(bogus), "查不到不当僵尸（fail-open）");
        assert!(start_token(bogus).is_none());
        assert!(command_line(bogus).is_none());
    }

    /// 脱机解析：正常块、填充 NUL、参数含空格、argc=0、块被截断。
    #[test]
    fn parse_procargs2_handles_layout_edges() {
        let build = |argc: i32, exe: &str, args: &[&str]| {
            let mut v = argc.to_ne_bytes().to_vec();
            v.extend_from_slice(exe.as_bytes());
            v.push(0);
            v.extend_from_slice(&[0, 0, 0]); // 填充
            for a in args {
                v.extend_from_slice(a.as_bytes());
                v.push(0);
            }
            v.extend_from_slice(b"HOME=/tmp\0"); // environ 必须被忽略
            v
        };
        assert_eq!(
            parse_procargs2(&build(2, "/bin/sleep", &["/bin/sleep", "2"])).unwrap(),
            "/bin/sleep\u{1f}2"
        );
        assert_eq!(
            parse_procargs2(&build(1, "/x/sh", &["sh -c echo hi"])).unwrap(),
            "sh -c echo hi",
            "参数内部的空格原样保留（这正是 \\u{{1f}} 分隔的理由）"
        );
        assert!(
            parse_procargs2(&build(0, "/bin/x", &[])).is_none(),
            "argc=0 无效"
        );
        assert!(parse_procargs2(&[]).is_none(), "空块无效");
        assert!(
            parse_procargs2(&build(5, "/bin/x", &["/bin/x"])).is_some(),
            "argc 大于实际剩余时按块尾截断，不 panic"
        );
        assert!(
            parse_procargs2(&build(-1, "/bin/x", &["/bin/x"])).is_none(),
            "负 argc 无效"
        );
    }
}
