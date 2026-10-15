//! 测试夹具的 RAII 归属：**只取名不建目录，谁创建谁管删，Drop 时删**（断言红到 panic 也会走到）。
//!
//! 为什么必须挪到栈上：单测此前一律
//! `let root = std::env::temp_dir().join(format!("<prefix>-{}", uuid::Uuid::new_v4()));`
//! 起头、用例**结尾**再 `let _ = std::fs::remove_dir_all(&root);` 收尾。收尾排在用例主体之后
//! ⇒ **断言一红就跳过**，夹具整份留在 `$TMPDIR`；而本机政策把 `TMPDIR` 指到数据卷
//! （见 AGENTS.md「Build intermediates go to /Volumes/DataExt/tmp」），于是每跑一轮红测就往
//! 数据卷堆一份目录。实测该目录积到 1.4 万个 `abb-*` 夹具、其中 7 千余个只剩空壳。
//!
//! 用法：把上面那句换成 `crate::testtmp::TempPath::new("<prefix>")`，并删掉用例结尾的手工
//! `remove_dir_all`（Drop 已经保证）。语义与原来**逐条对齐**：
//! * **不预创建**——有的用例前提就是「这个目录不存在」（如工作区解析 fail-closed 的用例），
//!   所以 `new` 只给名字，需要目录的用例照旧自己 `create_dir_all`；
//! * 名字仍是 `<prefix>-<uuid>`（uuid v4；不要退回只带 pid 的名字——并发/pid 复用会互删）；
//! * 实现了 `Deref<Target = Path>` 与 `AsRef<Path>`，`join` / `display` / `&*p` /
//!   丢给 `remove_dir_all(&p)` 这类泛型参数都照常。
#![allow(dead_code)] // 各模块按需取用；未被某个目标用到的构造器不算缺陷

use std::path::{Path, PathBuf};

/// 一份测试夹具的路径（目录或文件），出栈即删。
pub(crate) struct TempPath {
    path: PathBuf,
}

impl TempPath {
    /// 目录夹具名：`$TMPDIR/<prefix>-<uuid>`。**只取名，不建目录**。
    pub(crate) fn new(prefix: &str) -> Self {
        Self {
            path: std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4())),
        }
    }

    /// 文件夹具名：`$TMPDIR/<prefix>-<uuid>.<ext>`。同样只取名。
    pub(crate) fn file(prefix: &str, ext: &str) -> Self {
        Self {
            path: std::env::temp_dir().join(format!("{prefix}-{}.{ext}", uuid::Uuid::new_v4())),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempPath {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl std::ops::Deref for TempPath {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        // 目录、文件都收：先按目录删，失败了再按文件删（两条都不成就忽略——夹具本来可能没建过）。
        if std::fs::remove_dir_all(&self.path).is_err() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 负向对照（本模块存在的理由就是这个）：**用例 panic 时夹具也必须被删掉**。
    /// 旧写法（用例结尾手工 `remove_dir_all`）在这里必然失败——断言一红就跳过收尾，
    /// 那正是数据卷被 `abb-*` 夹具填满的根因。
    #[test]
    fn fixture_is_removed_even_when_the_test_panics() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None::<PathBuf>));
        let recorded = std::sync::Arc::clone(&seen);
        let result = std::panic::catch_unwind(move || {
            let fixture = TempPath::new("abb-fixture-probe");
            std::fs::create_dir_all(&fixture).unwrap();
            std::fs::write(fixture.join("marker"), b"x").unwrap();
            *recorded.lock().unwrap() = Some(fixture.path().to_path_buf());
            panic!("模拟断言失败：夹具不能被留在这里");
        });
        assert!(
            result.is_err(),
            "探针必须真的 panic 过，否则这条对照没验到东西"
        );
        let path = seen.lock().unwrap().clone().expect("探针必须记下夹具路径");
        assert!(
            !path.exists(),
            "panic 展开后夹具必须已删除，实际仍在：{}",
            path.display()
        );
    }

    /// 文件夹具同理（task_run 的 `*.jsonl` 记录走这条）。
    #[test]
    fn file_fixture_is_removed_on_drop() {
        let path = {
            let fixture = TempPath::file("abb-fixture-probe", "jsonl");
            std::fs::write(&fixture, b"{}\n").unwrap();
            assert!(fixture.exists(), "先确认文件真建出来了");
            fixture.path().to_path_buf()
        };
        assert!(!path.exists(), "出作用域后文件夹具必须已删除");
    }
}
