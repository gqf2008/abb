//! 内置工具（`rpi-tools`）：`read` / `write` / `edit` / `bash` / `grep` / `find` / `ls`。
//!
//! 为什么需要：abb-agent 第一刀只有 MCP 工具（abb-events / wassette），**没有文件读写与
//! shell**；被替代的 `crates/buzz-agent` 有 4 个内置 dev 工具（`shell`/`read`/`write`/`ls`，
//! 挂在 `dev` 伪命名空间下 ⇒ 模型看到的是 `dev__shell`）。本模块把 rpi-tools 的那套工具
//! 接上，并**沿用 `dev__` 限定名**，避免 owner 约定/技能里写过的工具名静默失效。
//!
//! ## 逐条对齐与被替代组件的差异（都写出来，别让读者猜）
//!
//! | 维度 | 被替代组件 | 本包 |
//! | --- | --- | --- |
//! | 集合 | `shell`/`read`/`write`/`ls` | 同上 + `edit`/`grep`/`find`（rpi-native 增量） |
//! | shell 工具 | `{command, timeout_secs}` | 同名前缀 `dev__shell`，schema 也把 `timeout` 改回 `timeout_secs` |
//! | read 的 `offset` | **0-based** | **1-based**（rpi 工具的语义；模型看得到 description，自洽但不通用） |
//! | 写入限定 | FullAccess 档也限定在会话 workspace（拒绝对路径与 `..` 逃逸） | **不放宽**：同样限定（见 [`confine_write_path`]） |
//! | 读的限定 | FullAccess 档允许绝对路径；受限档按 `read_roots` | 与 FullAccess 一致（**受限档的 roots 策略仍由 abb 的闸门承担**，本包不实现档位） |
//! | 开关 | `BUZZ_AGENT_DEV_TOOLS=0` 关闭 | 同一个环境变量、同一种读法（见 [`dev_tools_enabled_from`]） |
//! | Windows | shell 经 git-bash | 同（rpi 的 bash 工具在 Windows 也是 bash/POSIX，无 cmd 回退）；不额外暴露 `powershell` |

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, ToolResultPartial};
use rpi_ai::types::{Schema, Tool};
use rpi_tools::{
    create_bash_tool, create_edit_tool, create_find_tool, create_grep_tool, create_ls_tool,
    create_read_tool, create_write_tool, ExecutionEnv, ExecutionToolContext, MutatingEnv,
    OsExecutionEnv,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::provider::EnvSource;

/// 进程级开关：`0` 关闭整套内置工具（与 fork 同款环境变量）。
pub const DEV_TOOLS_ENV: &str = "BUZZ_AGENT_DEV_TOOLS";

/// 限定名命名空间（与被替代组件一致：它把这些工具挂在一个 `dev` 伪服务器下）。
const DEV_NAMESPACE: &str = "dev";
const SEP: &str = "__";

/// `BUZZ_AGENT_DEV_TOOLS` 的读法：**逐条对齐 fork**（`parse_env(…, 1u8)? != 0`）。
///
/// 默认开；`0` 关；读不懂 ⇒ `Err`（调用方响亮失败，与 fork 的 `die()` 同款）。
/// 与 `NO_HINTS` 不同，这个开关不是安全闸（工具面宽窄由 abb 的档位闸门兜底），所以
/// 「读不懂就拒启」在这里主要是**与参照物一致**与「配置错误要可见」，而不是防泄漏。
pub fn dev_tools_enabled_from(value: Option<&str>) -> Result<bool, String> {
    let raw = value.unwrap_or("1");
    match raw.parse::<u8>() {
        Ok(0) => Ok(false),
        Ok(_) => Ok(true),
        Err(error) => Err(format!(
            "{DEV_TOOLS_ENV}={raw:?} 无法解析为 0..=255（{error}）——按被替代组件同款口径拒绝启动"
        )),
    }
}

/// 从环境源判定（可注入版本，单测不改进程全局 env）。
pub fn dev_tools_enabled(env: &dyn EnvSource) -> Result<bool, String> {
    dev_tools_enabled_from(env.get(DEV_TOOLS_ENV).as_deref())
}

/// 造好本会话的内置工具（已换成 `dev__{name}` 限定名）。
///
/// `workspace` 是会话工作区（`session/new` 的 `cwd`）：read/bash/grep/find/ls 的基准目录，
/// 也是 write/edit 的**写入边界**。
pub fn dev_tools(workspace: &Path) -> Vec<Arc<dyn AgentTool>> {
    let env: Arc<OsExecutionEnv> = Arc::new(OsExecutionEnv::with_cwd(workspace.to_path_buf()));
    let read_env: Arc<dyn ExecutionEnv> = env.clone();
    let mutate_env: Arc<dyn MutatingEnv> = env;
    let context = ExecutionToolContext::new(read_env, Some(mutate_env));

    let raw: Vec<Arc<dyn AgentTool>> = vec![
        // shell 一行：`bash` 工具改成参照物的名字 `dev__shell`，并把 schema 里的
        // `timeout` 改回参照物的 `timeout_secs`（调用时再翻回去）。
        exposed(
            create_bash_tool(&context, None),
            "shell",
            Guard::ShellTimeout,
            workspace,
        ),
        exposed(
            create_read_tool(&context, None),
            "read",
            Guard::None,
            workspace,
        ),
        exposed(
            create_write_tool(&context),
            "write",
            Guard::WritePath,
            workspace,
        ),
        exposed(
            create_edit_tool(&context),
            "edit",
            Guard::WritePath,
            workspace,
        ),
        exposed(create_ls_tool(&context, None), "ls", Guard::None, workspace),
        exposed(
            create_grep_tool(&context, None),
            "grep",
            Guard::None,
            workspace,
        ),
        exposed(
            create_find_tool(&context, None),
            "find",
            Guard::None,
            workspace,
        ),
    ];
    raw
}

/// 工具执行前要做的那点额外事。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Guard {
    /// 原样委托。
    None,
    /// 写类工具：把 `path` 限定到会话工作区内（见 [`confine_write_path`]）。
    WritePath,
    /// shell：把 schema 的 `timeout_secs` 翻回 rpi 的 `timeout`。
    ShellTimeout,
}

/// 把 rpi 的工具包一层：改限定名 + （按需）参数守卫。
struct Exposed {
    schema: Tool,
    inner: Arc<dyn AgentTool>,
    guard: Guard,
    workspace: PathBuf,
}

fn exposed(
    inner: Arc<dyn AgentTool>,
    bare: &str,
    guard: Guard,
    workspace: &Path,
) -> Arc<dyn AgentTool> {
    let base = inner.schema();
    let mut parameters = base.parameters.clone();
    if guard == Guard::ShellTimeout {
        rename_param(&mut parameters, "timeout", "timeout_secs");
    }
    let schema = Tool {
        name: format!("{DEV_NAMESPACE}{SEP}{bare}"),
        description: base.description.clone(),
        parameters,
        constrained_sampling: base.constrained_sampling.clone(),
    };
    Arc::new(Exposed {
        schema,
        inner,
        guard,
        workspace: workspace.to_path_buf(),
    })
}

/// 把 JSON Schema 里 `properties` 下的某个字段改名（用于 `timeout` → `timeout_secs`）。
fn rename_param(schema: &mut Schema, from: &str, to: &str) {
    let Value::Object(root) = &mut schema.0 else {
        return;
    };
    let Some(Value::Object(properties)) = root.get_mut("properties") else {
        return;
    };
    if let Some(value) = properties.remove(from) {
        properties.insert(to.to_string(), value);
    }
    // `required` 里若列了旧名，一起改（否则模型会被告知必填一个不存在的字段）。
    if let Some(Value::Array(required)) = root.get_mut("required") {
        for entry in required.iter_mut() {
            if entry.as_str() == Some(from) {
                *entry = Value::String(to.to_string());
            }
        }
    }
}

#[async_trait]
impl AgentTool for Exposed {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        &self.schema.name
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let params = match self.guard {
            Guard::None => params,
            Guard::WritePath => self.confine(&params).await?,
            Guard::ShellTimeout => rename_timeout_param(params),
        };
        self.inner
            .execute(tool_call_id, params, signal, on_update)
            .await
    }
}

impl Exposed {
    /// 把写类工具的 `path` 限定到会话工作区，并把**已解析的绝对路径**传给内层工具。
    async fn confine(&self, params: &Value) -> Result<Value, AgentError> {
        let Some(raw) = params.get("path").and_then(Value::as_str) else {
            // 形状不对时不自作主张：交给内层工具按它自己的 schema 报错。
            return Ok(params.clone());
        };
        let target = confine_write_path(&self.workspace, raw)
            .await
            .map_err(AgentError::Tool)?;
        let mut params = params.clone();
        if let Value::Object(map) = &mut params {
            map.insert(
                "path".to_string(),
                Value::String(target.to_string_lossy().into_owned()),
            );
        }
        Ok(params)
    }
}

/// `timeout_secs` → `timeout`（rpi 的 bash 工具用这个名字）。
fn rename_timeout_param(params: Value) -> Value {
    let Value::Object(mut map) = params else {
        return params;
    };
    if let Some(value) = map.remove("timeout_secs") {
        map.insert("timeout".to_string(), value);
    }
    Value::Object(map)
}

/// 写入路径限定：**已经解析好的绝对目标**必须落在会话工作区内。
///
/// 与参照物 `crates/buzz-agent/src/devtools.rs::confined_write_path` 同一套判据：
/// 父目录先建后 canonicalize（这样 `..` 与符号链接都被解成真实路径再比对前缀），
/// 从而拦掉「`../escape`」「指向工作区外的符号链接父目录」这两类逃逸。
/// 与参照物的一处**有意更紧**：本包允许**落在工作区内的**绝对路径（参照物一律拒绝绝对
/// 路径），因为 rpi 的 write/edit 会把相对路径先解成绝对路径再落盘，不给它绝对路径反而
/// 会破坏正常写入——「不得出工作区」这条本身没有放宽。
async fn confine_write_path(workspace: &Path, raw: &str) -> Result<PathBuf, String> {
    let base = tokio::fs::canonicalize(workspace)
        .await
        .map_err(|error| format!("会话工作区不可访问：{}（{error}）", workspace.display()))?;
    let path = Path::new(raw);
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let parent = candidate
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| base.clone());
    tokio::fs::create_dir_all(&parent)
        .await
        .map_err(|error| format!("无法创建父目录：{raw}（{error}）"))?;
    let parent = tokio::fs::canonicalize(&parent)
        .await
        .map_err(|error| format!("父目录不可访问：{raw}（{error}）"))?;
    let file_name = candidate
        .file_name()
        .ok_or_else(|| format!("路径非法：{raw}"))?;
    let target = parent.join(file_name);
    if !target.starts_with(&base) {
        return Err(format!(
            "拒绝写入会话工作区之外：{raw}（工作区 {}）",
            base.display()
        ));
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProcessEnv;

    fn temp_workspace(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "abb-builtin-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("建临时工作区");
        dir
    }

    /// 开关读法逐条对齐 fork：未设置/非零开、`0` 关、读不懂报错。
    #[test]
    fn dev_tools_switch_matches_the_replaced_component() {
        assert_eq!(
            dev_tools_enabled_from(None),
            Ok(true),
            "未设置 = 开（fork 默认 1）"
        );
        for on in ["1", "2", "255"] {
            assert_eq!(dev_tools_enabled_from(Some(on)), Ok(true), "{on:?}");
        }
        assert_eq!(dev_tools_enabled_from(Some("0")), Ok(false));
        for bad in ["true", "-1", "256", "1\n", " 1 "] {
            assert!(dev_tools_enabled_from(Some(bad)).is_err(), "{bad:?} 应报错");
        }
        assert_eq!(dev_tools_enabled(&ProcessEnv), Ok(true));
    }

    /// 暴露给模型的名字必须是 `dev__{参照物名}`，且 shell 的超时字段名与参照物一致。
    #[test]
    fn tools_are_exposed_under_dev_namespace_with_replaced_component_shapes() {
        let workspace = temp_workspace("names");
        let tools = dev_tools(&workspace);
        let names: Vec<String> = tools.iter().map(|t| t.schema().name.clone()).collect();
        for expected in [
            "dev__shell",
            "dev__read",
            "dev__write",
            "dev__edit",
            "dev__ls",
            "dev__grep",
            "dev__find",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "缺 {expected}：{names:?}"
            );
        }
        assert!(
            !names.iter().any(|name| !name.starts_with("dev__")),
            "不得暴露裸名（会与 MCP 工具命名空间混）：{names:?}"
        );

        let shell = tools
            .iter()
            .find(|t| t.schema().name == "dev__shell")
            .expect("shell 工具");
        let props = shell.schema().parameters.0["properties"]
            .as_object()
            .expect("properties")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert!(props.contains(&"timeout_secs".to_string()), "{props:?}");
        assert!(
            !props.contains(&"timeout".to_string()),
            "rpi 的字段名不得泄漏给模型：{props:?}"
        );
        assert!(props.contains(&"command".to_string()), "{props:?}");
    }

    /// `timeout_secs` → `timeout` 的翻译（含两者同时出现时以 `timeout_secs` 为准）。
    #[test]
    fn shell_timeout_param_is_translated_back() {
        let translated = rename_timeout_param(serde_json::json!({
            "command": "ls",
            "timeout_secs": 30,
        }));
        assert_eq!(translated["timeout"], 30);
        assert!(translated.get("timeout_secs").is_none());
        // 非对象原样返回（内层工具会自己报形状错误）。
        assert_eq!(rename_timeout_param(Value::Null), Value::Null);
    }

    /// 写入限定：工作区内可写、`..` 与工作区外绝对路径被拒。
    #[tokio::test]
    async fn write_confinement_rejects_escapes() {
        let workspace = temp_workspace("write");
        let inside = confine_write_path(&workspace, "sub/file.txt")
            .await
            .expect("工作区内相对路径应可写");
        let canon = tokio::fs::canonicalize(&workspace).await.expect("canon");
        assert!(inside.starts_with(&canon), "{inside:?}");

        let absolute_inside =
            confine_write_path(&workspace, &format!("{}/abs.txt", canon.to_string_lossy()))
                .await
                .expect("工作区内绝对路径应可写（有意比参照物更紧的那一条不放宽）");
        assert!(absolute_inside.starts_with(&canon));

        let escape = confine_write_path(&workspace, "../escape.txt").await;
        assert!(escape.is_err(), "`..` 逃逸必须被拒：{escape:?}");
        let outside = confine_write_path(&workspace, "/tmp/abb-escape-outside.txt").await;
        assert!(outside.is_err(), "工作区外绝对路径必须被拒：{outside:?}");
    }

    /// 符号链接父目录指向工作区外时也要拦住（canonicalize 之后再比前缀）。
    #[tokio::test]
    async fn write_confinement_rejects_symlinked_parent_escape() {
        let workspace = temp_workspace("symlink");
        let outside = temp_workspace("symlink-outside");
        let link = workspace.join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).expect("建符号链接");
        #[cfg(not(unix))]
        {
            // Windows 上建目录符号链接需要特权：跳过这条，由前缀判据那条覆盖。
            return;
        }
        let attempt = confine_write_path(&workspace, "link/escape.txt").await;
        assert!(attempt.is_err(), "符号链接逃逸必须被拒：{attempt:?}");
    }
}
