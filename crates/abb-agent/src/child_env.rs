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
