//! 渠道 MCP 配置：`<渠道工作目录>/mcp.json`（卡 `abb-channel-mcp-config-20261012`）。
//!
//! 外部 MCP server 的配置**按渠道**存，就放在该渠道自己的工作目录里
//! （`channel_dir`，即会话 cwd）。用户在会话里驱动 bot 用文件工具直接读写；
//! ABB 只在建会话时读它、把名单放进 `session/new` 的 `mcpServers`——ABB 只声明、
//! 不解释语义（不认识的 server 也能接）。
//!
//! 文件形状（顶层是数组）：
//!
//! ```json
//! [
//!   { "name": "notes", "command": "/opt/notes-mcp",
//!     "args": ["--root", "/tmp"], "env": [{ "name": "TOKEN", "value": "t1" }] }
//! ]
//! ```
//!
//! 校验 fail-visible：解析失败 / 条目非法只**跳过该条并留日志**，不静默降级成
//! 「少接一个」。`granted`（受限授权者）与 oneshot 会话**不读**本文件（见 `pool.rs`）。
//!
//! **信任口径**（承自 bot 级字段时代的评审 R3）：声明即**授权它以 agent 身份运行**——
//! MCP 子进程**不按会话档位运行**（`_meta.abbSandbox` 管不到它），它继承 agent 进程的
//! 白名单 env（含 `$ABB_BIN` = 本程序绝对路径）并以**会话工作区（= 渠道目录）**为 cwd，
//! 等于给它在工作区的执行权 + 驱动 `task`/`deliver` 等 CLI 的能力；条目自声明的 `env`
//! 会叠加在白名单之上。仍生效的闸：Q8（`--proc` 载荷对 agent 一律拒）与
//! `cross_delivery_enabled`（跨会话投递）⇒ **只声明你信任的 server**。

use std::path::{Path, PathBuf};

use crate::runtime::acp::{EnvVar, McpServer};

const FILE_NAME: &str = "mcp.json";

/// 渠道 MCP 配置文件路径。
pub fn path(channel_dir: &Path) -> PathBuf {
    channel_dir.join(FILE_NAME)
}

/// 变更检测用**内容指纹**：文件不存在 → `None`（会话据此判断"该渠道没有 MCP 配置"）。
///
/// 用内容哈希而不是 mtime：mtime 在粗粒度文件系统（ExFAT 等）上会漏掉同一时间片内的
/// 二次编辑，而"配置变了就要重建会话"不能漏。文件很小，每回合读一次可接受。
pub fn stamp(channel_dir: &Path) -> Option<u64> {
    let bytes = std::fs::read(path(channel_dir)).ok()?;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    Some(hash)
}

/// 会话建立时的名单裁决：`enabled=false`（granted / oneshot）恒空——即便渠道目录里
/// 真有 `mcp.json` 也不读（外部 server 的工具面会绕过受限档沙箱）。单一判据置此，
/// 调用方（`pool.rs`）不再自己写 if。
pub fn load_for_session(enabled: bool, channel_dir: &Path) -> Vec<McpServer> {
    if enabled {
        load(channel_dir)
    } else {
        Vec::new()
    }
}

/// 读 + 校验 → `session/new` 的名单。任何失败都退化成空名单，但一定留日志。
pub fn load(channel_dir: &Path) -> Vec<McpServer> {
    let file = path(channel_dir);
    let raw = match std::fs::read_to_string(&file) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            crate::log!(
                "[mcp] 渠道配置读失败（按空名单处理）dir={} err={error}",
                channel_dir.display()
            );
            return Vec::new();
        }
    };
    let parsed: Vec<McpServerConfig> = match serde_json::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(error) => {
            crate::log!(
                "[mcp] 渠道配置解析失败（按空名单处理）path={} err={error}",
                file.display()
            );
            return Vec::new();
        }
    };
    let mut out: Vec<McpServer> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for cfg in &parsed {
        let name = cfg.name.trim();
        if !cfg.is_usable() {
            crate::log!(
                "[mcp] 渠道配置有一项 name/command 为空，已跳过 path={} name={:?}",
                file.display(),
                cfg.name
            );
            continue;
        }
        if !seen.insert(name.to_string()) {
            crate::log!(
                "[mcp] 渠道配置有重复 server 名「{name}」，后一项已跳过 path={}",
                file.display()
            );
            continue;
        }
        out.push(McpServer {
            name: name.to_string(),
            command: cfg.command.trim().to_string(),
            args: cfg.args.clone(),
            env: cfg
                .env
                .iter()
                .map(|e| EnvVar {
                    name: e.name.clone(),
                    value: e.value.clone(),
                })
                .collect(),
        });
    }
    out
}

/// 渠道 `mcp.json` 的条目形状（与 `runtime/acp.rs::McpServer` 一一对应）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct McpServerConfig {
    /// server 名（会出现在工具名 `{server}__{tool}` 里；装配侧校验非空且唯一）。
    pub name: String,
    /// 可执行文件（与 `PATH` 解析同语义）。
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<McpEnvVar>,
}

/// 下发给 MCP 子进程的环境变量（与 `runtime/acp.rs::EnvVar` 逐字对应）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct McpEnvVar {
    pub name: String,
    pub value: String,
}

impl McpServerConfig {
    /// 形状是否可用（语义不校验——ABB 不认识具体 server）。
    pub fn is_usable(&self) -> bool {
        !self.name.trim().is_empty() && !self.command.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("abb-channel-mcp-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 内容指纹：同内容稳定、内容变了就变、文件不存在是 None。
    #[test]
    fn stamp_tracks_content_not_time() {
        let dir = tmp_dir("stamp");
        assert_eq!(stamp(&dir), None, "没有文件 = 没有指纹");
        std::fs::write(path(&dir), r#"[{"name":"a","command":"x"}]"#).unwrap();
        let first = stamp(&dir).expect("写入后应有指纹");
        assert_eq!(stamp(&dir), Some(first), "同内容必须稳定");
        std::fs::write(path(&dir), r#"[{"name":"a","command":"y"}]"#).unwrap();
        assert_ne!(stamp(&dir), Some(first), "内容变了指纹必须变（不看 mtime）");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 没有 `mcp.json` 的渠道 = 不接任何 server（不是错误）。
    #[test]
    fn missing_file_is_empty_and_has_no_stamp() {
        let dir = tmp_dir("missing");
        assert!(load(&dir).is_empty());
        assert_eq!(stamp(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 正常条目逐字透传（含 args/env）；stamp 在写入后可用（变更检测的前提）。
    #[test]
    fn valid_file_is_passed_through_verbatim() {
        let dir = tmp_dir("valid");
        std::fs::write(
            path(&dir),
            r#"[{"name":"notes","command":"/opt/notes-mcp","args":["--root","/tmp"],
                "env":[{"name":"TOKEN","value":"t1"}]}]"#,
        )
        .unwrap();
        let got = load(&dir);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "notes");
        assert_eq!(got[0].command, "/opt/notes-mcp");
        assert_eq!(got[0].args, vec!["--root", "/tmp"]);
        assert_eq!(got[0].env.len(), 1);
        assert_eq!(got[0].env[0].name, "TOKEN");
        assert!(stamp(&dir).is_some(), "写入后必须能取到 mtime");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 解析失败不是崩溃、也不是静默：退化成空名单（日志可见，见 `crate::log!`）。
    #[test]
    fn malformed_json_degrades_to_empty() {
        let dir = tmp_dir("malformed");
        std::fs::write(path(&dir), "{ not an array").unwrap();
        assert!(load(&dir).is_empty());
        // 顶层不是数组也一样
        std::fs::write(path(&dir), r#"{"name":"x"}"#).unwrap();
        assert!(load(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 非法条目只跳过自己：name/command 空、重名都不得拖垮其余条目。
    #[test]
    fn invalid_entries_are_skipped_not_fatal() {
        let dir = tmp_dir("invalid");
        std::fs::write(
            path(&dir),
            r#"[
                {"name":"","command":"/bin/true"},
                {"name":"no-cmd","command":"   "},
                {"name":"dup","command":"/bin/true"},
                {"name":"dup","command":"/bin/false"},
                {"name":"ok","command":"/bin/echo"}
            ]"#,
        )
        .unwrap();
        let got = load(&dir);
        assert_eq!(
            got.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["dup", "ok"],
            "空名/空命令跳过，重名只留第一个"
        );
        assert_eq!(got[0].command, "/bin/true", "重名保留的是第一项");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 受限/oneshot 句柄不读渠道配置：目录里**真有** `mcp.json` 也必须拿到空名单。
    #[test]
    fn disabled_session_ignores_even_a_valid_file() {
        let dir = tmp_dir("disabled");
        std::fs::write(
            path(&dir),
            r#"[{"name":"notes","command":"/opt/notes-mcp"}]"#,
        )
        .unwrap();
        assert_eq!(load(&dir).len(), 1, "前提：文件本身有效");
        assert!(
            load_for_session(false, &dir).is_empty(),
            "granted/oneshot 恒空（安全口径）"
        );
        assert_eq!(load_for_session(true, &dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 形状谓词（两条阴性 + 一条阳性）。
    #[test]
    fn usability_predicate() {
        let ok = McpServerConfig {
            name: " n ".into(),
            command: " c ".into(),
            args: Vec::new(),
            env: Vec::new(),
        };
        assert!(ok.is_usable());
        assert!(!McpServerConfig {
            name: "   ".into(),
            ..ok.clone()
        }
        .is_usable());
        assert!(!McpServerConfig {
            command: String::new(),
            ..ok.clone()
        }
        .is_usable());
    }
}
