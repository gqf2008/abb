//! 单实例锁（flock）。等价 Windows 命名互斥锁：内核态、**进程退出/崩溃自动释放**（fd 被内核回收），
//! 不会像 pid 文件那样残留假死。GUI 与 service 各用一把锁文件，互不干扰。
//!
//! 用法：`let _guard = SingleInstance::acquire("service")?;` —— 返回值必须持有到进程结束
//! （drop 即 flock(LOCK_UN) + 关 fd；但正常路径是进程结束由内核回收）。

use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

pub struct SingleInstance {
    _file: File, // 持有 fd 即持有锁；drop 时内核释放
    name: String,
}

/// 重试间隔下限。
///
/// `interval = 0` 时 `sleep(0)` 立即返回，重试循环会在 `timeout` 用尽前**忙等自旋烧满一核**
/// （复核 reviewer-38 的 F2）。调用点传 0 属编程错误，但代价不该是 CPU 满载，所以在入口夹一个
/// 1ms 地板：既不影响正常调用（250ms），也让 0 退化成「尽力重试」而不是「空转」。
fn retry_interval(interval: Duration) -> Duration {
    interval.max(Duration::from_millis(1))
}

impl SingleInstance {
    /// 尝试对 ~/.agent-bridge/.<name>.lock 拿排他非阻塞锁。
    /// 成功返回 guard；**已有实例在跑返回 Err**（调用方应退出）。
    pub fn acquire(name: &str) -> Result<SingleInstance> {
        Self::acquire_at(&crate::bridge_dir(), name)
    }

    /// 带重试地拿锁，给「升级重启」路径用（安装器 `[Run]` 段拉起新实例时加 `--wait-lock`）。
    ///
    /// 为什么需要：普通 [`acquire`](Self::acquire) 是「已有实例在跑 → 本实例立刻退出」的
    /// 语义，而升级重启瞬间旧实例刚被安装器关掉或自己 quit——**锁句柄释放与进程彻底退出
    /// 之间还有一小段**（Windows 侧 `share_mode(0)` 独占句柄要等内核回收）。即退会把这次
    /// 重启静默吞掉：用户看到「升级装完但 ABB 没起来」，日志只有一行「已有一个实例在运行」。
    /// 这里按 `interval` 重试直到 `timeout` 用尽，把那一小段等过去。
    ///
    /// 只在升级重启的显式路径上启用：用户手点第二份图标仍然立刻退出（不排队、不等待）。
    pub fn acquire_with_retry(
        name: &str,
        timeout: Duration,
        interval: Duration,
    ) -> Result<SingleInstance> {
        Self::acquire_at_with_retry(&crate::bridge_dir(), name, timeout, interval)
    }

    /// [`acquire_with_retry`](Self::acquire_with_retry) 的目录可注入版（单测用）。
    fn acquire_at_with_retry(
        dir: &Path,
        name: &str,
        timeout: Duration,
        interval: Duration,
    ) -> Result<SingleInstance> {
        let deadline = std::time::Instant::now() + timeout;
        let interval = retry_interval(interval);
        let mut waited = false;
        loop {
            match Self::acquire_at(dir, name) {
                Ok(g) => {
                    if waited {
                        crate::log!("[single-instance] 等到 {name} 锁释放（升级重启路径）");
                    }
                    return Ok(g);
                }
                Err(e) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(e.context(format!(
                            "等待 {name} 锁超时（{}s 内旧实例没退出），本实例退出",
                            timeout.as_secs()
                        )));
                    }
                    if !waited {
                        waited = true;
                        crate::log!(
                            "[single-instance] {name} 锁被占用（升级重启：等旧实例退干净，最多 {}s）：{e:#}",
                            timeout.as_secs()
                        );
                    }
                    std::thread::sleep(interval);
                }
            }
        }
    }

    /// 指定目录的锁实现；生产入口传 `~/.agent-bridge`，测试传唯一 temp 目录。
    fn acquire_at(dir: &Path, name: &str) -> Result<SingleInstance> {
        std::fs::create_dir_all(dir).ok();
        let path = dir.join(format!(".{name}.lock"));
        // 锁文件只是 flock 的锚点，从不读写内容——无需 truncate/append
        #[cfg(unix)]
        #[allow(clippy::suspicious_open_options)]
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("打开锁文件失败: {}", path.display()))?;
        // Windows：share_mode(0) 独占打开 = flock(LOCK_EX|LOCK_NB) 等价物；
        // 关句柄或进程崩溃即由内核释放，不残留假锁。
        #[cfg(windows)]
        #[allow(clippy::suspicious_open_options)]
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .share_mode(0)
            .open(&path)
            .with_context(|| {
                format!(
                    "已有一个 agent-bridge「{name}」实例在运行（无法独占锁文件 {}）。本实例退出。",
                    path.display()
                )
            })?;
        #[cfg(unix)]
        {
            let fd = file.as_raw_fd();
            // LOCK_EX 排他 + LOCK_NB 非阻塞（拿不到立刻失败，不等待）
            let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                anyhow::bail!(
                    "已有一个 agent-bridge「{name}」实例在运行（flock 拿不到锁: {err}）。本实例退出。"
                );
            }
        }
        crate::log!("[single-instance] 拿到 {name} 锁: {}", path.display());
        Ok(SingleInstance {
            _file: file,
            name: name.to_string(),
        })
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // 显式解锁（正常退出路径）；异常崩溃由内核回收 fd 自动释放
        #[cfg(unix)]
        unsafe {
            libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
        }
        // Windows 无需显式解锁：Drop 关句柄即释放独占锁
        crate::log!("[single-instance] 释放 {} 锁", self.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_fails() {
        let dir = std::env::temp_dir().join(format!("abb-single-{}", uuid::Uuid::new_v4()));
        let g1 = SingleInstance::acquire_at(&dir, "test").expect("第一次应拿到");
        let g2 = SingleInstance::acquire_at(&dir, "test");
        assert!(g2.is_err(), "第二次拿同名锁应失败");
        drop(g1);
        let g3 = SingleInstance::acquire_at(&dir, "test");
        assert!(g3.is_ok(), "释放后应能再拿到");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn different_names_independent() {
        let dir = std::env::temp_dir().join(format!("abb-single-{}", uuid::Uuid::new_v4()));
        let _a = SingleInstance::acquire_at(&dir, "test-a").expect("a");
        let _b = SingleInstance::acquire_at(&dir, "test-b").expect("b 与 a 不同名，应独立拿到");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 升级重启路径：持有者正在退出（还没释放锁）时，重试版必须等到它放锁再拿到。
    #[test]
    fn retry_acquire_takes_over_after_holder_releases() {
        let dir = std::env::temp_dir().join(format!("abb-single-{}", uuid::Uuid::new_v4()));
        let holder = SingleInstance::acquire_at(&dir, "test").expect("旧实例持锁");
        // 模拟旧实例收尾：300ms 后才退出（drop guard = 释放锁）
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(holder);
        });
        let g = SingleInstance::acquire_at_with_retry(
            &dir,
            "test",
            Duration::from_secs(5),
            Duration::from_millis(50),
        )
        .expect("等旧实例退出后必须拿到锁（否则升级重启被静默吞掉）");
        t.join().unwrap();
        drop(g);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 反面：持有者一直不放锁时必须超时失败，不能无限等（否则托盘会挂住不退出）。
    #[test]
    fn retry_acquire_gives_up_after_timeout() {
        let dir = std::env::temp_dir().join(format!("abb-single-{}", uuid::Uuid::new_v4()));
        let _holder = SingleInstance::acquire_at(&dir, "test").expect("持锁");
        let t = std::time::Instant::now();
        let r = SingleInstance::acquire_at_with_retry(
            &dir,
            "test",
            Duration::from_millis(200),
            Duration::from_millis(50),
        );
        assert!(r.is_err(), "持有者不放锁必须超时失败");
        assert!(
            t.elapsed() < Duration::from_secs(3),
            "超时后必须尽快返回，实际 {:?}",
            t.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F2 复核项：`interval=0` 必须被抬到 1ms 地板，否则重试循环在 deadline 前忙等自旋。
    #[test]
    fn zero_interval_is_floored_to_avoid_busy_spin() {
        assert_eq!(
            retry_interval(Duration::ZERO),
            Duration::from_millis(1),
            "interval=0 必须被抬到 1ms（sleep(0) 会让重试循环烧满一核）"
        );
        assert_eq!(
            retry_interval(Duration::from_millis(250)),
            Duration::from_millis(250),
            "正常间隔不得被改动"
        );
        assert!(retry_interval(Duration::ZERO) > Duration::ZERO);
    }

    /// 端到端：传 `interval=0` 时仍必须**有界**返回（既不忙等到底，也不提前放弃）。
    #[test]
    fn zero_interval_still_terminates_within_timeout() {
        let dir = std::env::temp_dir().join(format!("abb-single-{}", uuid::Uuid::new_v4()));
        let _holder = SingleInstance::acquire_at(&dir, "test").expect("持锁");
        let t = std::time::Instant::now();
        let r = SingleInstance::acquire_at_with_retry(
            &dir,
            "test",
            Duration::from_millis(150),
            Duration::ZERO,
        );
        assert!(r.is_err(), "持有者不放锁必须超时失败");
        let took = t.elapsed();
        assert!(
            took >= Duration::from_millis(100),
            "应当真的等到 deadline（实际 {took:?}），否则说明 0 间隔被当成「只试一次」"
        );
        assert!(took < Duration::from_secs(3), "有界返回，实际 {took:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
