//! `--transport` 分派：用**真的子进程**验参数契约。
//!
//! 为什么不能只靠 `src/main.rs` 里的单测：那些测的是 `parse_transport` 返回值，
//! 测不到「进程真的按退出码 2 拒绝」「拒绝时打 usage 而不是静默退出」。
//! 合并成一个二进制之后，这三条比过去更重要——过去渠道搞错是「起错了二进制」，
//! 现在渠道搞错是「同一个二进制选错了分支」，静默起来就只是凭证不对、连不上平台。

use std::process::{Command, Stdio};

fn bin() -> String {
    env!("CARGO_BIN_EXE_abb-channel").to_string()
}

/// 起进程、塞 stdin、收退出码 + stderr。
///
/// stdin 给空（立刻 EOF）：渠道的 `run()` 读 stdin 循环，EOF 即正常退出。
/// 这样「参数正确」的路径也能验，不必真去连平台。
fn run(args: &[&str]) -> (Option<i32>, String) {
    let out = Command::new(bin())
        .args(args)
        // 空凭证：三个渠道都只打警告、不启长连接（各自 `run()` 里有判断）。
        .env("FS_APP_ID", "")
        .env("FS_APP_SECRET", "")
        .env("DT_APP_ID", "")
        .env("DT_APP_SECRET", "")
        .env("WX_TOKEN", "")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("起不来 abb-channel");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// 三个渠道名都必须能起、且**正常退出**（退出码 0）。
///
/// 判据的另一半在下面：错的走退出码 2。两条合起来才说明「分派真的分到了」，
/// 只验「不 panic」的话，把 dispatch 写成恒 `feishu::run()` 也照样过。
#[test]
fn every_declared_transport_starts_and_exits_cleanly() {
    for t in ["feishu", "dingtalk", "wechat"] {
        let (code, err) = run(&["--transport", t]);
        assert_eq!(
            code,
            Some(0),
            "{t} 应当正常退出（EOF 即结束）；stderr={err}"
        );
        // 每个渠道的启动横幅必须报**自己**的名字：这是「分派分对了」的直接证据。
        let who = match t {
            "feishu" => "[feishu-mcp]",
            "dingtalk" => "[dingtalk-mcp]",
            _ => "[wechat-mcp]",
        };
        assert!(
            err.contains(who),
            "{t} 的 stderr 里应能看到自己的启动横幅 {who}（实际：{err}）"
        );
    }
}

/// 请求体不同：错参数必须退出码 2，且把 usage 打出来。
///
/// 为什么单独要退出码 2（不是 1）：ABB 侧/脚本据此区分「改参数」与「查凭证/网络」。
/// 混成一个码的话，配错 transport 会被当成「渠道挂了」反复重试。
#[test]
fn bad_transport_exits_2_with_usage() {
    for bad in [
        vec![],
        vec!["--transport"],
        vec!["--transport", ""],
        vec!["--transport", "slack"],
        vec!["--transport=slack"],
    ] {
        let (code, err) = run(&bad);
        assert_eq!(
            code,
            Some(2),
            "{bad:?} 应当以 2 退出（调用错，不是运行失败）"
        );
        assert!(
            err.contains("用法：abb-channel --transport"),
            "{bad:?} 必须打 usage 指路（实际：{err}）"
        );
    }
}

/// `--dir` 这类渠道自己的参数不许干扰 `--transport` 的解析。
///
/// 微信要 `--dir`，且历史上是在 `std::env::args()` 里自己扫的；合并后两者共存于
/// 同一个 argv，顺序不能有隐含依赖。
#[test]
fn channel_specific_args_do_not_confuse_transport() {
    let tmp = std::env::temp_dir();
    let tmp = tmp.to_str().expect("临时目录必须是 UTF-8");
    for args in [
        vec!["--dir", tmp, "--transport", "wechat"],
        vec!["--transport", "wechat", "--dir", tmp],
    ] {
        let (code, err) = run(&args);
        assert_eq!(code, Some(0), "{args:?} 应当正常跑；stderr={err}");
        assert!(err.contains("[wechat-mcp]"), "{args:?} 应分派到微信：{err}");
    }
}
