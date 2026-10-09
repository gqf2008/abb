//! Jev 决策模型客户端（Typesafe System One，经 OpenRouter Decisions API）。
//!
//! 用途：授权者（受限）会话里，**每个工具调用执行前**问一次 Jev「该不该放行」，
//! 拿到类型化的是/否概率（`noul` 原语），低于阈值即拒绝。这是「代码兜底、语义
//! 判断交给决策模型」的语义层——代码只负责文件读写域闸（物理事实），「这条 shell
//! 命令 / 这次读取合不合理」这类判断交给 Jev。
//!
//! ## 为什么不是普通 chat provider
//!
//! Jev 不是 LLM、不产文本：它只回类型化决策 + 概率。OpenRouter 上它有两个专属面
//! （Decisions API 与 System One API），都不是 OpenAI chat completions 形状，所以
//! 不能用本包 `provider.rs` 那条 anthropic/openai chat 通道，得自己 POST。
//!
//! - 端点：`POST {JEV_BASE_URL}`（默认 `https://openrouter.ai/api/alpha/decisions`）；
//! - 鉴权：`Authorization: Bearer {JEV_API_KEY}`（OpenRouter key）；
//! - 请求：`{ model, state(上下文), questions: { <名>: { type: "noul", instructions,
//!   criteria: { true, false } } } }`；
//! - 响应：`{ answers: { <名>: { type: "noul", noul: 0.96 } }, usage }`。
//!
//! `noul` 是「yes 的概率」：1.0 = 明确该放行，0.0 = 明确该拒绝。默认阈值 0.5
//! （`JEV_ALLOW_THRESHOLD` 可调），**低于阈值 = 拒绝**（fail-closed 倾向从严）。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, ToolResultPartial};
use rpi_ai::types::Tool;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::provider::EnvSource;

/// Jev API key 环境变量名。
pub const JEV_API_KEY_ENV: &str = "JEV_API_KEY";
/// Jev Decisions API 端点（默认 OpenRouter 的 alpha decisions 端点）。
pub const JEV_BASE_URL_ENV: &str = "JEV_BASE_URL";
/// Jev 模型 id（默认 `typesafe/jev-latest` 别名，跟踪最新发布）。
pub const JEV_MODEL_ENV: &str = "JEV_MODEL";
/// 放行概率阈值环境变量名（0~1，默认 0.5；低于阈值 = 拒绝）。
pub const JEV_ALLOW_THRESHOLD_ENV: &str = "JEV_ALLOW_THRESHOLD";

pub const DEFAULT_JEV_BASE_URL: &str = "https://openrouter.ai/api/alpha/decisions";
pub const DEFAULT_JEV_MODEL: &str = "typesafe/jev-latest";
pub const DEFAULT_ALLOW_THRESHOLD: f64 = 0.5;
/// 每次决策请求的超时：决策在工具执行路径上，不能拖垮会话（fail-closed 下超时 = 拒绝）。
pub const DECISION_TIMEOUT: Duration = Duration::from_secs(10);

/// 一次工具调用前的决策结论。
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// 是否放行。
    pub allowed: bool,
    /// Jev 给出的「yes」概率（0~1）。
    pub probability: f64,
    /// 拒绝时的原因（给模型的可纠偏文本；放行时为空）。
    pub reason: String,
}

impl Decision {
    /// 拒绝结论（Jev 不可用 / 概率不足时走这里）：带可纠偏的原因文本。
    pub fn deny(reason: impl Into<String>) -> Self {
        // 概率记 0.0：既不是「Jev 说可以」，也不是「Jev 拿不准」，而是「没得到放行依据」。
        Self {
            allowed: false,
            probability: 0.0,
            reason: reason.into(),
        }
    }
}

/// Jev Decisions API 的 noul 应答（只取我们要的字段）。
#[derive(Debug, Deserialize)]
struct DecisionsResponse {
    answers: std::collections::HashMap<String, NoulAnswer>,
}

#[derive(Debug, Deserialize)]
struct NoulAnswer {
    #[serde(rename = "type")]
    _type: String,
    #[serde(default)]
    noul: Option<f64>,
}

/// Jev 决策客户端：持有端点 / key / 模型 / 阈值，`check` 发一次决策请求。
///
/// `Clone` 便宜（`reqwest::Client` 内部是 `Arc`），供 `GuardedTool` 每会话持有一份。
#[derive(Clone)]
pub struct JevClient {
    base_url: String,
    api_key: String,
    model: String,
    threshold: f64,
    http: reqwest::Client,
}

impl JevClient {
    /// 从环境源装配；缺 key / 阈值非法时 `Err`（调用方据此 fail-closed）。
    pub fn from_env(env: &dyn EnvSource) -> Result<Self, String> {
        let api_key = env
            .get(JEV_API_KEY_ENV)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("缺少 {JEV_API_KEY_ENV}（授权者会话的决策门禁不可用）"))?;
        let base_url = env
            .get(JEV_BASE_URL_ENV)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_JEV_BASE_URL.to_string());
        let model = env
            .get(JEV_MODEL_ENV)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_JEV_MODEL.to_string());
        let threshold = env
            .get(JEV_ALLOW_THRESHOLD_ENV)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .map(|raw| {
                raw.parse::<f64>()
                    .ok()
                    .filter(|t| (0.0..=1.0).contains(t))
                    .ok_or_else(|| format!("{JEV_ALLOW_THRESHOLD_ENV} 非法：{raw}"))
            })
            .transpose()?
            .unwrap_or(DEFAULT_ALLOW_THRESHOLD);
        let http = reqwest::Client::builder()
            .timeout(DECISION_TIMEOUT)
            .build()
            .map_err(|e| format!("构建 Jev HTTP 客户端失败：{e}"))?;
        Ok(Self {
            base_url,
            api_key,
            model,
            threshold,
            http,
        })
    }

    /// 问 Jev：这个工具调用该不该放行？
    ///
    /// `state` 是整个上下文（工具名 / 参数 / 会话工作区等），Jev 据此判断。
    /// 任何失败（网络 / 超时 / 非 2xx / 解析不出概率）都 `Err`——调用方按
    /// fail-closed 拒绝，绝不把「拿不到决策」当成「放行」。
    pub async fn check(&self, state: Value) -> Result<Decision, String> {
        let question = json!({
            "allowed": {
                "type": "noul",
                "instructions":
                    "Should this tool call be allowed to execute? Answer yes only if it \
                     stays within the session's permitted scope.",
                "criteria": {
                    "true": "The tool call stays within the session workspace and its \
                             allowed operations; it does not access outside paths, delete \
                             data, or perform a forbidden action.",
                    "false": "The tool call reads/writes outside the workspace, deletes \
                              data, exfiltrates credentials, or performs a forbidden action."
                }
            }
        });
        let body = json!({
            "model": self.model,
            "state": state,
            "questions": question,
        });

        let resp = self
            .http
            .post(&self.base_url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Jev 决策请求失败：{e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(format!("Jev 决策返回非 2xx：{status}"));
        }
        let parsed: DecisionsResponse = resp
            .json()
            .await
            .map_err(|e| format!("Jev 决策响应解析失败：{e}"))?;
        let answer = parsed
            .answers
            .get("allowed")
            .ok_or_else(|| "Jev 决策响应缺少 allowed 答案".to_string())?;
        let probability = answer
            .noul
            .ok_or_else(|| "Jev 决策响应缺少 noul 概率".to_string())?;
        Ok(Decision {
            allowed: probability >= self.threshold,
            probability,
            reason: if probability >= self.threshold {
                String::new()
            } else {
                format!(
                    "Jev 判定该工具调用不应放行（概率 {probability:.2} < 阈值 {:.2}）",
                    self.threshold
                )
            },
        })
    }
}

/// 决策模型门禁包装：把 Jev 的 allow/deny 强加到**任意** `AgentTool` 外面。
///
/// 为什么是包装器而不是 rpi 的 `before_tool_call` hook：rpi-agent 0.3.16 的
/// `AgentBuilder` **没有暴露** `before_tool_call` setter（`build_config` 里硬编码
/// `None`）。而包装器把同样的拦截放在 `execute` 入口，每个工具调用都会过——语义等价、
/// 不碰 rpi 库，且天然覆盖 MCP 工具 / 内置工具 / `load_skill`（都是 `Arc<dyn AgentTool>`）。
///
/// 评审意见 1 的落实：`state` 里带 `tool`（工具名）+ `args`（参数）+ `workspace`
/// （会话工作区），Jev 据此判断越界。
///
/// fail-closed：`check` 返回 `Err`（Jev 不可用）或 `deny` 时，一律返回工具错误
/// （`AgentError::Tool`，模型看到可纠偏的 reason），**绝不委托内层执行**。
pub struct GuardedTool {
    inner: Arc<dyn AgentTool>,
    jev: JevClient,
    workspace: String,
}

impl GuardedTool {
    /// 包一层决策门禁。`workspace` 是会话工作区（进 Jev 的 state）。
    pub fn new(inner: Arc<dyn AgentTool>, jev: JevClient, workspace: impl Into<String>) -> Self {
        Self {
            inner,
            jev,
            workspace: workspace.into(),
        }
    }
}

#[async_trait]
impl AgentTool for GuardedTool {
    fn schema(&self) -> &Tool {
        self.inner.schema()
    }

    fn label(&self) -> &str {
        self.inner.label()
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let state = json!({
            "tool": self.inner.schema().name,
            "args": params,
            "workspace": self.workspace,
        });
        match self.jev.check(state).await {
            Ok(decision) if decision.allowed => {
                self.inner
                    .execute(tool_call_id, params, signal, on_update)
                    .await
            }
            Ok(decision) => Err(AgentError::tool(decision.reason)),
            Err(reason) => Err(AgentError::tool(format!(
                "决策门禁不可用，已拒绝该工具调用（fail-closed）：{reason}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    struct FakeEnv(HashMap<String, String>);
    impl EnvSource for FakeEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    fn fake_env(pairs: &[(&str, &str)]) -> FakeEnv {
        FakeEnv(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
    }

    /// 起一个只服务一次请求的假 Jev server，返回给定 body + 状态码。
    /// 返回 (base_url, 收到的请求体 JSON)。
    fn spawn_fake_jev(status: &str, body: &str) -> (String, std::sync::mpsc::Receiver<Value>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = std::sync::mpsc::channel();
        let status = status.to_string();
        let body = body.to_string();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                // 找到 JSON body（HTTP 头之后的空行）。
                let req_body = req.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
                let _ = tx.send(serde_json::from_str(&req_body).unwrap_or(Value::Null));
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        (format!("http://{addr}"), rx)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn allow_when_probability_meets_threshold() {
        let (base, rx) = spawn_fake_jev(
            "200 OK",
            r#"{"model":"typesafe/jev-1.13","answers":{"allowed":{"type":"noul","noul":0.9}},"usage":{"input_tokens":1,"output_tokens":1}}"#,
        );
        let env = fake_env(&[
            ("JEV_API_KEY", "sk-test"),
            ("JEV_BASE_URL", &base),
            ("JEV_MODEL", "typesafe/jev-1.13"),
        ]);
        let client = JevClient::from_env(&env).expect("装配");
        let d = client
            .check(json!({"tool": "dev__shell", "args": {"command": "git status"}}))
            .await
            .expect("决策");
        assert!(d.allowed, "0.9 >= 0.5 应放行：{d:?}");
        assert!((d.probability - 0.9).abs() < 1e-9);

        // 断言请求体真的按 Decisions API 形状发出去。
        let sent = rx.recv().expect("收到请求体");
        assert_eq!(sent["model"], "typesafe/jev-1.13");
        assert_eq!(sent["questions"]["allowed"]["type"], "noul");
        assert!(sent["state"]["tool"] == "dev__shell");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deny_when_probability_below_threshold() {
        let (base, _rx) = spawn_fake_jev(
            "200 OK",
            r#"{"model":"typesafe/jev-1.13","answers":{"allowed":{"type":"noul","noul":0.2}},"usage":{"input_tokens":1,"output_tokens":1}}"#,
        );
        let env = fake_env(&[("JEV_API_KEY", "sk-test"), ("JEV_BASE_URL", &base)]);
        let client = JevClient::from_env(&env).expect("装配");
        let d = client
            .check(json!({"tool": "dev__shell", "args": {"command": "rm -rf ~"}}))
            .await
            .expect("决策");
        assert!(!d.allowed, "0.2 < 0.5 应拒绝：{d:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn non_2xx_is_an_error_for_fail_closed() {
        let (base, _rx) = spawn_fake_jev("500 Internal Server Error", "{}");
        let env = fake_env(&[("JEV_API_KEY", "sk-test"), ("JEV_BASE_URL", &base)]);
        let client = JevClient::from_env(&env).expect("装配");
        let err = client
            .check(json!({"tool": "dev__read"}))
            .await
            .unwrap_err();
        assert!(err.contains("500"), "非 2xx 要报错：{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_probability_is_an_error() {
        let (base, _rx) = spawn_fake_jev(
            "200 OK",
            r#"{"model":"typesafe/jev-1.13","answers":{"allowed":{"type":"noul"}},"usage":{"input_tokens":1,"output_tokens":1}}"#,
        );
        let env = fake_env(&[("JEV_API_KEY", "sk-test"), ("JEV_BASE_URL", &base)]);
        let client = JevClient::from_env(&env).expect("装配");
        let err = client
            .check(json!({"tool": "dev__read"}))
            .await
            .unwrap_err();
        assert!(err.contains("noul"), "缺概率要报错：{err}");
    }

    #[test]
    fn missing_key_is_an_error() {
        let env = fake_env(&[]);
        match JevClient::from_env(&env) {
            Err(err) => assert!(err.contains("JEV_API_KEY"), "{err}"),
            Ok(_) => panic!("缺 key 不应装配成功"),
        }
    }

    #[test]
    fn bad_threshold_is_an_error() {
        let env = fake_env(&[("JEV_API_KEY", "sk"), ("JEV_ALLOW_THRESHOLD", "1.5")]);
        match JevClient::from_env(&env) {
            Err(err) => assert!(err.contains("JEV_ALLOW_THRESHOLD"), "{err}"),
            Ok(_) => panic!("非法阈值不应装配成功"),
        }
    }

    #[test]
    fn defaults_are_applied_when_env_absent() {
        let env = fake_env(&[("JEV_API_KEY", "sk")]);
        let client = JevClient::from_env(&env).expect("装配");
        assert_eq!(client.base_url, DEFAULT_JEV_BASE_URL);
        assert_eq!(client.model, DEFAULT_JEV_MODEL);
        assert_eq!(client.threshold, DEFAULT_ALLOW_THRESHOLD);
    }

    // ── GuardedTool ────────────────────────────────────────────────

    /// 假内层工具：记录「execute 是否被调用」，并返回固定文本。
    struct ProbeTool {
        name: &'static str,
        executed: std::sync::atomic::AtomicBool,
    }

    impl ProbeTool {
        fn new(name: &'static str) -> Arc<Self> {
            Arc::new(Self {
                name,
                executed: std::sync::atomic::AtomicBool::new(false),
            })
        }
    }

    #[async_trait]
    impl AgentTool for ProbeTool {
        fn schema(&self) -> &Tool {
            // 借用临时值的生命周期问题：这里用 Box::leak 制造一个 'static schema。
            Box::leak(Box::new(Tool {
                name: self.name.to_string(),
                description: String::new(),
                parameters: rpi_ai::types::Schema(serde_json::json!({})),
                constrained_sampling: None,
            }))
        }

        fn label(&self) -> &str {
            self.name
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            _params: Value,
            _signal: CancellationToken,
            _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
        ) -> Result<AgentToolResult, AgentError> {
            self.executed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(AgentToolResult::text("probe executed"))
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn guarded_allow_delegates_to_inner() {
        let (base, _rx) = spawn_fake_jev(
            "200 OK",
            r#"{"model":"typesafe/jev-1.13","answers":{"allowed":{"type":"noul","noul":0.9}},"usage":{"input_tokens":1,"output_tokens":1}}"#,
        );
        let env = fake_env(&[("JEV_API_KEY", "sk-test"), ("JEV_BASE_URL", &base)]);
        let jev = JevClient::from_env(&env).expect("装配");
        let probe = ProbeTool::new("dev__probe");
        let guarded = GuardedTool::new(probe.clone(), jev, "/ws");
        let result = guarded
            .execute(
                "id-1",
                json!({}),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .expect("放行应委托执行");
        assert!(probe.executed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            matches!(&result.content[0], rpi_agent::types::TextContentOrImage::Text(t) if t.text == "probe executed")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn guarded_deny_blocks_inner() {
        let (base, _rx) = spawn_fake_jev(
            "200 OK",
            r#"{"model":"typesafe/jev-1.13","answers":{"allowed":{"type":"noul","noul":0.1}},"usage":{"input_tokens":1,"output_tokens":1}}"#,
        );
        let env = fake_env(&[("JEV_API_KEY", "sk-test"), ("JEV_BASE_URL", &base)]);
        let jev = JevClient::from_env(&env).expect("装配");
        let probe = ProbeTool::new("dev__probe");
        let guarded = GuardedTool::new(probe.clone(), jev, "/ws");
        let err = guarded
            .execute(
                "id-1",
                json!({}),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .unwrap_err();
        assert!(
            !probe.executed.load(std::sync::atomic::Ordering::SeqCst),
            "deny 时内层不得执行"
        );
        assert!(err.to_string().contains("不应放行"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn guarded_unavailable_blocks_inner_fail_closed() {
        let (base, _rx) = spawn_fake_jev("500 Internal Server Error", "{}");
        let env = fake_env(&[("JEV_API_KEY", "sk-test"), ("JEV_BASE_URL", &base)]);
        let jev = JevClient::from_env(&env).expect("装配");
        let probe = ProbeTool::new("dev__probe");
        let guarded = GuardedTool::new(probe.clone(), jev, "/ws");
        let err = guarded
            .execute(
                "id-1",
                json!({}),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .unwrap_err();
        assert!(
            !probe.executed.load(std::sync::atomic::Ordering::SeqCst),
            "Jev 不可用时内层不得执行（fail-closed）"
        );
        assert!(err.to_string().contains("fail-closed"), "{err}");
    }
}
