//! 子进程环境白名单：**先清空、再按表注入**。
//!
//! 两个消费者共用这一份：
//! - MCP server 子进程（`mcp.rs`）——它们是我们 spawn 的第三个组件宿主；
//! - 内置工具里的 shell（`builtin.rs`，经 `OsExecutionEnv` 的 `inherit_env=false`）。
//!
//! 为什么必须清空：abb 把供应商凭据（`ANTHROPIC_API_KEY` / `OPENAI_COMPAT_API_KEY` 等）注入
//! **agent 进程**，那是 agent 自己要用的；但 MCP server 与 shell 都不是 abb 的 agent，默认继承
//! 就等于把用户的付费 key 交给每一个被 spawn 的组件与每一条 shell 命令。
//! 白名单内容与被替代的 `crates/buzz-agent/src/mcp.rs::PASSTHROUGH_ENV` 同源
//! （去掉那边 buzz 专属的几条）：子工具需要的是「能不能出网、能不能用 git、临时目录在哪」。

/// agent 上下文标记：**必须**由我们写入，不能指望从宿主继承。
///
/// abb 的真实 ACP 路径不会把 `AGENT_BRIDGE_BOT_KEY` / `CHAT_ID` / `SENDER_ROLE` 透传给
/// shell（白名单把它们剥掉了），于是 abb 的 proc 闸（Q8：`--proc` 只允许 GUI/人工入口）
/// 以这个变量作为**主判据**（`src/task_proc.rs::ACP_AGENT_CONTEXT_ENV`）。少了它，
/// 「agent 派生的 shell 去建 `--proc` 任务」就会 fail-open 地通过。
/// 被替代组件在同一个共享入口（`mcp::apply_passthrough_env`）里有这一步，本包照做，
/// 且**两个消费者都写**（MCP 子进程与内置工具的 shell）。
///
/// 它**不是**对敌意 owner 的安全边界（owner 本来 FullAccess、能清环境、能直接改
/// `tasks.json`）：这是合规/纵深防御闸。
pub const AGENT_CONTEXT_ENV: &str = "ABB_AGENT_CONTEXT";

/// 传给子进程的环境变量白名单。
pub const PASSTHROUGH_ENV: &[&str] = &[
    // 基础
    "PATH",
    "HOME",
    "TERM",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "XDG_CONFIG_HOME",
    // SSH：git clone/push over SSH（git@github.com:…）要用
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    // Git：运维配置的 helper 与传输覆盖
    "GIT_ASKPASS",
    "GIT_SSH_COMMAND",
    "GIT_CONFIG_GLOBAL",
    // 代理：唯一出口是 CONNECT 代理的机器上，丢掉这几条不是「降级」而是让工具瞎掉
    // （apt/curl/pip/git 会直连，然后被出口防火墙 reset，看起来像「没有网络」）。
    // 大小写都要：curl/git 读小写，多数 Go/Python 工具读大写，而 libcurl 故意忽略
    // 大写的 `HTTP_PROXY`——只留一种会静默坏掉半个工具链。
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
    // TLS 信任：终止 TLS 的代理自带 CA，镜像的信任库没有它 ⇒ 每次 https 都验签失败
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    // 编辑器/分页器：git commit 等命令在没有它们时行为会变
    "EDITOR",
    "VISUAL",
    "PAGER",
    "GIT_PAGER",
];

/// Windows 没有 $TMPDIR/$HOME：`std::env::temp_dir()` 看 TMP/TEMP，缺了就回落到子进程
/// 写不进去的 `C:\Windows`；USERPROFILE 是恒存在的兜底，APPDATA 带子工具配置。
#[cfg(windows)]
pub const PASSTHROUGH_ENV_WINDOWS: &[&str] = &["TMP", "TEMP", "USERPROFILE", "APPDATA"];

/// 按白名单收集当前进程里**存在**的那些变量（给需要 map 的调用方：`ShellExecOptions`）。
pub fn passthrough_map() -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    map.insert(AGENT_CONTEXT_ENV.to_string(), "1".to_string());
    for key in PASSTHROUGH_ENV {
        if let Ok(value) = std::env::var(key) {
            map.insert((*key).to_string(), value);
        }
    }
    #[cfg(windows)]
    for key in PASSTHROUGH_ENV_WINDOWS {
        if let Ok(value) = std::env::var(key) {
            map.insert((*key).to_string(), value);
        }
    }
    map
}

/// `env_clear()` + 白名单（给 `tokio::process::Command` 的调用方）。
pub fn apply_passthrough_env(cmd: &mut tokio::process::Command) {
    cmd.env_clear();
    for (key, value) in passthrough_map() {
        cmd.env(key, value);
    }
    // `passthrough_map()` 已含标记；这里再显式写一次，免得将来有人「顺手」把标记从
    // 白名单逻辑里挪走而没人发现（两处同值，测试断言覆盖）。
    cmd.env(AGENT_CONTEXT_ENV, "1");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitelist_excludes_provider_credentials() {
        for key in [
            "ANTHROPIC_API_KEY",
            "OPENAI_COMPAT_API_KEY",
            "BUZZ_AGENT_PROVIDER",
            "ANTHROPIC_MODEL",
            "OPENAI_COMPAT_MODEL",
            "RUST_LOG",
        ] {
            assert!(
                !PASSTHROUGH_ENV.contains(&key),
                "{key} 不该出现在子进程白名单里"
            );
        }
    }

    #[test]
    fn whitelist_keeps_what_tools_need() {
        for key in ["PATH", "HOME", "TMPDIR", "SSH_AUTH_SOCK", "HTTPS_PROXY"] {
            assert!(PASSTHROUGH_ENV.contains(&key), "缺 {key}：工具会瞎");
        }
    }

    /// agent 上下文标记必须由我们写入（abb 的 proc 闸主判据；少了它会 fail-open）。
    #[test]
    fn agent_context_marker_is_written_by_us() {
        assert_eq!(
            passthrough_map().get(AGENT_CONTEXT_ENV),
            Some(&"1".to_string())
        );
        // 宿主里若已有别的值，也必须被我们覆盖成 "1"（我们才是这个子进程的来源）。
        std::env::set_var(AGENT_CONTEXT_ENV, "host-value");
        assert_eq!(
            passthrough_map().get(AGENT_CONTEXT_ENV),
            Some(&"1".to_string())
        );
        std::env::remove_var(AGENT_CONTEXT_ENV);
    }

    /// map 只收「当前进程里真的有」的那些，且不含凭据。
    #[test]
    fn map_only_contains_present_keys_and_no_credentials() {
        std::env::set_var("ANTHROPIC_API_KEY", "sk-should-not-leak");
        let map = passthrough_map();
        assert!(map.values().all(|value| value != "sk-should-not-leak"));
        assert!(!map.contains_key("ANTHROPIC_API_KEY"));
        std::env::remove_var("ANTHROPIC_API_KEY");
    }
}
