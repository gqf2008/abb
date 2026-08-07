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

pub struct SingleInstance {
    _file: File, // 持有 fd 即持有锁；drop 时内核释放
    name: String,
}

impl SingleInstance {
    /// 尝试对 ~/.agent-bridge/.<name>.lock 拿排他非阻塞锁。
    /// 成功返回 guard；**已有实例在跑返回 Err**（调用方应退出）。
    pub fn acquire(name: &str) -> Result<SingleInstance> {
        let dir = crate::bridge_dir();
        std::fs::create_dir_all(&dir).ok();
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
        let g1 = SingleInstance::acquire("test").expect("第一次应拿到");
        let g2 = SingleInstance::acquire("test");
        assert!(g2.is_err(), "第二次拿同名锁应失败");
        drop(g1);
        let g3 = SingleInstance::acquire("test");
        assert!(g3.is_ok(), "释放后应能再拿到");
    }

    #[test]
    fn different_names_independent() {
        let _a = SingleInstance::acquire("test-a").expect("a");
        let _b = SingleInstance::acquire("test-b").expect("b 与 a 不同名，应独立拿到");
    }
}
