//! ABB 渠道 MCP server —— 飞书 / 钉钉 / 微信，一个二进制。
//!
//! ```text
//! abb-channel --transport feishu     # 读 FS_APP_ID / FS_APP_SECRET
//! abb-channel --transport dingtalk   # 读 DT_APP_ID / DT_APP_SECRET / DT_ROBOT_CODE
//! abb-channel --transport wechat --dir <登录态目录>   # 读 WX_TOKEN / WX_BASE_URL / WX_USER_ID
//! ```
//!
//! ABB 侧配置（`bots[].mcp_channel`）：
//!
//! ```jsonc
//! { "command": "/path/to/abb-channel",
//!   "args": ["--transport", "feishu"],
//!   "env": [{ "name": "FS_APP_ID", "value": "…" }, …] }
//! ```
//!
//! # 为什么是一个二进制而不是三个
//!
//! 三个渠道各自编一份时，`rustls + ring + webpki-roots` 那约 4 MB 的 HTTPS
//! 地板要付三遍（实测 5.5 + 5.1 + 4.7 = **10.2 MB**）。合起来链接一次，
//! 地板只算一份 ⇒ 实测 **5.6 MB**。业务代码一行没改，契约也没动。
//!
//! 三个渠道本来还各自重复了一整套 JSON-RPC stdio 循环、`base64_decode`、
//! `split_text`、文件名清洗——合并后这些也只剩一份（`jsonrpc` 模块）。
//!
//! # 分派规则
//!
//! `--transport <name>` **必填**，不接受按环境变量猜：一个渠道进程如果连"我是
//! 哪个渠道"都要猜，配错时（比如把钉钉的凭证填进飞书）会表现为「连不上平台」，
//! 而不是「参数错了」，排障要多绕一圈。缺失或未知值 ⇒ 打 usage 到 stderr、
//! 退出码 2（与 ABB 侧 `restart` 的「拒绝」同口径：不是运行失败，是调用错了）。

mod dingtalk;
mod feishu;
mod jsonrpc;
mod wechat;

use std::process::ExitCode;

/// 三个渠道的名字。新增渠道时这里与 `dispatch` 必须同时改（`tests/transport_dispatch.rs`
/// 会对两者做双向断言）。
const TRANSPORTS: [&str; 3] = ["feishu", "dingtalk", "wechat"];

const USAGE: &str = "\
用法：abb-channel --transport <feishu|dingtalk|wechat> [渠道自己的参数]

  feishu     读 FS_APP_ID / FS_APP_SECRET
  dingtalk   读 DT_APP_ID / DT_APP_SECRET / DT_ROBOT_CODE
  wechat     读 WX_TOKEN / WX_BASE_URL / WX_USER_ID；需要 --dir <登录态目录>（二维码落地）

渠道配置由提供 MCP 的一方决定：ABB 不认识 FS_APP_ID 这类变量，只把 mcp_channel.env
原样传给本进程。";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match parse_transport(&args[1..]) {
        Ok(name) => {
            dispatch(&name);
            ExitCode::SUCCESS
        }
        Err(why) => {
            eprintln!("[abb-channel] {why}\n\n{USAGE}");
            // 2 = 调用错了（参数问题），不是渠道运行失败（那是 1）。
            // 分开的理由：ABB 侧/脚本可以据此判断「改参数」还是「查凭证/网络」。
            ExitCode::from(2)
        }
    }
}

/// 从参数里取 `--transport <name>`（也接受 `--transport=<name>`）。
///
/// 抽成纯函数是为了能直接测「缺值 / 未知值 / 两种写法」，不必起进程。
fn parse_transport(args: &[String]) -> Result<String, String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--transport" {
            return match it.next() {
                Some(v) => validate_transport(v),
                // 显式给空值也拒：`--transport ""` 与「没给」是同一个错误，
                // 不该因为拼出了空串就落到「未知渠道」的分支上绕一圈。
                None => Err("--transport 缺值".to_string()),
            };
        }
        if let Some(rest) = a.strip_prefix("--transport=") {
            return validate_transport(rest);
        }
    }
    Err(format!(
        "缺 --transport（可选值：{}）",
        TRANSPORTS.join(" / ")
    ))
}

fn validate_transport(v: &str) -> Result<String, String> {
    let v = v.trim();
    if v.is_empty() {
        return Err("--transport 的值为空".to_string());
    }
    if TRANSPORTS.contains(&v) {
        Ok(v.to_string())
    } else {
        Err(format!(
            "未知的 --transport「{v}」（可选值：{}）",
            TRANSPORTS.join(" / ")
        ))
    }
}

/// 分派到具体渠道的会话循环。每个渠道的 `run()` 都**不会返回**到有用的地方——
/// 它们一直阻塞读 stdin 直到 EOF（MCP 的正常结束方式）。
fn dispatch(name: &str) {
    match name {
        "feishu" => feishu::run(),
        "dingtalk" => dingtalk::run(),
        "wechat" => wechat::run(),
        // `validate_transport` 已经挡住了其它值；走到这里是代码不一致。
        other => unreachable!("parse_transport 只放行三个渠道，却收到 {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_both_forms() {
        assert_eq!(
            parse_transport(&a(&["--transport", "feishu"])),
            Ok("feishu".into())
        );
        assert_eq!(
            parse_transport(&a(&["--transport=wechat"])),
            Ok("wechat".into())
        );
        // 渠道自己的参数可以在前后任意位置（微信的 --dir 就是这么用的）。
        assert_eq!(
            parse_transport(&a(&["--dir", "/tmp/x", "--transport", "dingtalk"])),
            Ok("dingtalk".into())
        );
    }

    #[test]
    fn rejects_missing_and_unknown() {
        for bad in [
            a(&[]),
            a(&["--dir", "/tmp"]),
            a(&["--transport"]),
            a(&["--transport", ""]),
            a(&["--transport", "slack"]),
            a(&["--transport=slack"]),
            // 大小写不归一化：渠道名是精确标识，不是用户输入的自然语言。
            a(&["--transport", "Feishu"]),
        ] {
            assert!(
                parse_transport(&bad).is_err(),
                "必须拒绝 {bad:?}（否则会拿错凭证去连平台）"
            );
        }
    }

    /// `TRANSPORTS` 与 `dispatch` 必须同步：漏一个 ⇒ 参数校验放行但分派 panic。
    ///
    /// 判据读源码文本（真分发会阻塞读 stdin）。**反向断言**也在：
    /// `dispatch` 里出现的每个字面量渠道名都必须在 `TRANSPORTS` 里，
    /// 否则「新增了分支但忘了登记」会让 usage 里看不到它。
    #[test]
    fn transports_match_dispatch() {
        let src = include_str!("main.rs");
        let body = src
            .split("fn dispatch(name: &str) {")
            .nth(1)
            .expect("应有 dispatch");
        let body: String = body.chars().take(400).collect();
        for t in TRANSPORTS {
            assert!(
                body.contains(&format!("\"{t}\" => ")),
                "TRANSPORTS 里有 {t}，dispatch 却没有分支：{body}"
            );
        }
        // 反向：dispatch 的 match 臂必须都登记过。
        for arm in body.lines() {
            let Some(rest) = arm.trim().strip_prefix('"') else {
                continue;
            };
            let Some(name) = rest.split('"').next() else {
                continue;
            };
            if !name.chars().all(|c| c.is_ascii_lowercase()) || name.is_empty() {
                continue;
            }
            assert!(
                TRANSPORTS.contains(&name),
                "dispatch 处理了 {name:?}，但 TRANSPORTS 里没有它（usage 会漏报）"
            );
        }
    }
}
