//! provider 装配：把 abb 注入的供应商 env 翻译成 rpi provider + model。
//!
//! ## env 契约以 abb 侧真值为准
//!
//! 来源：`src/agent.rs::buzz_provider_env`（唯一注入点，经 `service.rs` 组装进子进程 env）。
//!
//! | 供应商 | `BUZZ_AGENT_PROVIDER` | 其余 |
//! | --- | --- | --- |
//! | anthropic | `"anthropic"` | `ANTHROPIC_API_KEY`、`ANTHROPIC_BASE_URL`、`ANTHROPIC_MODEL` |
//! | openai 家族 | `"openai"` | `OPENAI_COMPAT_API_KEY`、`OPENAI_COMPAT_BASE_URL`、`OPENAI_COMPAT_MODEL`、`OPENAI_COMPAT_API`(`chat`/`responses`) |
//!
//! `openai-chat` / `openai-responses` / `openrouter` / `deepseek` 是**配置里的 kind**，
//! 不是 env 里 `BUZZ_AGENT_PROVIDER` 的取值——它们都会被 abb 归并成 `"openai"`
//! （API 形态另走 `OPENAI_COMPAT_API`）。
//!
//! ## 模型是「显式构造」的，不查任何内置目录
//!
//! 这是 rpi 的官方做法：`rpi-cli/src/config.rs::provider_to_models` 对 models.json
//! 条目就是 `Model::new(id, name, api, provider, base_url)`——**模型 id 由调用方给，
//! rpi 不限制命名空间**（`rpi --help` 的 `--model` 也写着支持 `models.json id`）。
//!
//! 本包第一版却去查 `AnthropicProvider` 内置的 7 项模型表，于是自定义网关（one-api 等）
//! 用厂商原生 id 就会失败、缺失时还会静默落到表首（Opus 档）。**那是我实现里的偷懒，
//! 不是 rpi 的能力边界**——现已改为显式构造。
//!
//! 另有一个必须绕开的库内行为：`clamp_max_tokens_to_context` 在
//! `model.context_window == 0` 时返回 `max(MIN_MAX_TOKENS=1, max_tokens)`，
//! 于是 `max_tokens = 0` 会变成「只准回 1 个 token」。abb 的契约里没有这个字段，
//! 故 [`custom_model`] 给一个保守默认（见 [`DEFAULT_MAX_OUTPUT_TOKENS`]）。
//!
//! `ANTHROPIC_API_KEY` / `OPENAI_COMPAT_API_KEY` 都是**显式取用**的（不依赖 rpi 的
//! env 兜底）：`with_models_without_env_api_key` 关掉了兜底，避免把机器上无关的
//! `OPENAI_API_KEY` 之类的凭据发给自定义网关。
//!
//! 另有**离线**通道 `ABB_AGENT_FAUX_TEXT=<文本>`：命中即用 rpi 的 faux provider
//! （不联网、无凭据、逐字可复现），供 smoke 与集成测试用。它是本包自己的调试开关，
//! 不属于 abb 的契约。

use std::sync::Arc;

use rpi_agent::StreamFn;
use rpi_ai::providers::anthropic::AnthropicProvider;
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_ai::providers::openai_completions::OpenAiCompletionsProvider;
use rpi_ai::providers::openai_responses::OpenAiResponsesProvider;
use rpi_ai::{Api, Model, Provider};

/// abb 回合作数的环境变量名（abb 恒送，默认 200）。
pub const MAX_ROUNDS_ENV: &str = "BUZZ_AGENT_MAX_ROUNDS";

/// abb 未指定时我们采用的回合上界，与其 `BUZZ_AGENT_MAX_ROUNDS` 默认值一致。
pub const DEFAULT_MAX_ROUNDS: u64 = 200;

const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com";

/// 单回合输出上限的环境变量名。
///
/// abb 的 `buzz_provider_env` **今天不注入**它，但被替代的 fork 会读它
/// （`crates/buzz-agent/src/config.rs` 的 `parse_env("BUZZ_AGENT_MAX_OUTPUT_TOKENS", 65_536)`），
/// 所以这里同样认——将来 abb 一旦注入就自动生效。
pub const MAX_OUTPUT_TOKENS_ENV: &str = "BUZZ_AGENT_MAX_OUTPUT_TOKENS";

/// 未指定时的单回合输出上限。
///
/// **必须对齐被替代组件的默认值**：fork 的默认就是 65_536，而我第一版写成 8_192——
/// 把输出上限悄悄缩了 8 倍，而且超限的失败形态是**静默截断**（rpi 把 `StopReason::Length`
/// 归入 `Completed`、`error_message` 为空 ⇒ 上报成 `end_turn`，abb 会 `record_success`）。
/// 长回答被截半截而系统记成功，是静默失败。
///
/// 另：不能给 0。`clamp_max_tokens_to_context` 在 `context_window == 0`（我们不知道上下文
/// 窗口）时返回 `max(1, max_tokens)`，所以 `0` 会变成「只准回 1 个 token」。
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 65_536;

/// 离线通道：固定文本回复。
pub const FAUX_TEXT_ENV: &str = "ABB_AGENT_FAUX_TEXT";

/// 离线通道：先发一次工具调用（JSON `{"name":…,"arguments":…}`），再把
/// [`FAUX_TEXT_ENV`] 当作文本回复。
pub const FAUX_TOOL_ENV: &str = "ABB_AGENT_FAUX_TOOL";

/// 环境读取缝。
///
/// **为什么不在测试里 `set_var`**：env 是进程全局的，并行用例会互相踩
/// （第一版实测就踩了：一个设 `ABB_AGENT_FAUX_TEXT`、另一个删它，结果随机失败）。
/// 把读取收成 trait，测试用假环境注入，就不必串行化、也不必碰全局状态。
pub trait EnvSource {
    fn get(&self, key: &str) -> Option<String>;
}

/// 真实进程环境。
pub struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// 选中的执行后端（provider + 已构造好的 model）。
///
/// 刻意用**具体类型**而不是 `Arc<dyn Provider>`：`Provider::stream_simple` 是
/// `async fn`（AFIT），直接做 trait 对象不稳；这里用泛型助手构造 `StreamFn`，
/// 把对象安全问题挡在外面。
pub enum Backend {
    Faux {
        provider: Arc<FauxProvider>,
        model: Model,
    },
    Anthropic {
        provider: Arc<AnthropicProvider>,
        model: Model,
    },
    OpenAiCompletions {
        provider: Arc<OpenAiCompletionsProvider>,
        model: Model,
    },
    OpenAiResponses {
        provider: Arc<OpenAiResponsesProvider>,
        model: Model,
    },
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 手写而非 derive：provider 内部（reqwest client 等）没有 Debug，
        // 而我们真正想看到的就是「选了哪个后端、哪个模型、打到哪」。
        f.debug_struct("Backend")
            .field("id", &self.id())
            .field("model", &self.model().id)
            .field("base_url", &self.model().base_url)
            .finish()
    }
}

impl Backend {
    pub fn id(&self) -> &'static str {
        match self {
            Backend::Faux { .. } => "faux",
            Backend::Anthropic { .. } => "anthropic",
            Backend::OpenAiCompletions { .. } | Backend::OpenAiResponses { .. } => "openai",
        }
    }

    pub fn model(&self) -> &Model {
        match self {
            Backend::Faux { model, .. }
            | Backend::Anthropic { model, .. }
            | Backend::OpenAiCompletions { model, .. }
            | Backend::OpenAiResponses { model, .. } => model,
        }
    }

    /// 本后端的 `StreamFn`（agent loop 每次请求都会调它）。
    pub fn stream_fn(&self) -> StreamFn {
        match self {
            Backend::Faux { provider, .. } => make_stream_fn(Arc::clone(provider)),
            Backend::Anthropic { provider, .. } => make_stream_fn(Arc::clone(provider)),
            Backend::OpenAiCompletions { provider, .. } => make_stream_fn(Arc::clone(provider)),
            Backend::OpenAiResponses { provider, .. } => make_stream_fn(Arc::clone(provider)),
        }
    }
}

/// 按环境选择后端。返回 `Err` 时**不 panic**：调用方把原因如实报到会话建立处，
/// 让「agent 起不来」可见可诊断（与 abb 的既有取向一致）。
pub fn select() -> Result<Backend, String> {
    select_with(&ProcessEnv)
}

/// [`select`] 的可注入版本（环境来源显式传入）。
pub fn select_with(env: &dyn EnvSource) -> Result<Backend, String> {
    // 离线**工具**路径：先发一次工具调用，再把 `ABB_AGENT_FAUX_TEXT` 当作文本回复。
    // 用于无 key、无网验证「模型 → 工具 → 结果回灌」整条链（`probes/mcp_tool_round_trip.py` 靠它）。
    if let Some(raw) = env.get(FAUX_TOOL_ENV) {
        let spec: serde_json::Value = serde_json::from_str(raw.trim())
            .map_err(|error| format!("{FAUX_TOOL_ENV} 不是合法 JSON：{error}"))?;
        let name = spec
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("{FAUX_TOOL_ENV} 缺少 name"))?
            .to_string();
        let arguments = spec
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let follow_up = env
            .get("ABB_AGENT_FAUX_TEXT")
            .unwrap_or_else(|| "工具已执行。".to_string());
        let provider = FauxProvider::new(
            FauxScript::new()
                .with_tool_call(name, arguments)
                .with_text(follow_up),
        );
        let model = provider
            .models()
            .first()
            .cloned()
            .ok_or_else(|| "faux provider 没有可用模型".to_string())?;
        return Ok(Backend::Faux { provider, model });
    }
    if let Some(text) = env.get("ABB_AGENT_FAUX_TEXT") {
        let provider = FauxProvider::new(FauxScript::new().with_text(text));
        let model = provider
            .models()
            .first()
            .cloned()
            .ok_or_else(|| "faux provider 没有可用模型".to_string())?;
        return Ok(Backend::Faux { provider, model });
    }

    match env.get("BUZZ_AGENT_PROVIDER").unwrap_or_default().trim() {
        "anthropic" => anthropic_backend(env),
        "openai" => openai_backend(env),
        "" => Err("未设置 BUZZ_AGENT_PROVIDER（abb 会送 anthropic 或 openai；\
             离线 smoke 可用 ABB_AGENT_FAUX_TEXT）"
            .to_string()),
        other => Err(format!(
            "未知的 BUZZ_AGENT_PROVIDER「{other}」（abb 只送 anthropic 或 openai；\
             openai-chat/openrouter/deepseek 等是配置 kind，env 里会被归并成 openai）"
        )),
    }
}

fn anthropic_backend(env: &dyn EnvSource) -> Result<Backend, String> {
    let api_key = required(env, "ANTHROPIC_API_KEY")?;
    // 语义：**主机根**（如 `https://gateway.example.com`）。rpi 自己拼 `/v1/messages`，
    // 所以写成 `…/v1` 会得到 `/v1/v1/messages`。这与被替代的 fork 同构
    // （`crates/buzz-agent/src/llm.rs` 也是 `{base}/v1/messages`），不是回归；
    // README 已把它写成使用约束。
    let base_url = optional(env, "ANTHROPIC_BASE_URL")
        .unwrap_or_else(|| DEFAULT_ANTHROPIC_BASE_URL.to_string());
    let model_id = required(env, "ANTHROPIC_MODEL")?;
    let http = rpi_ai::http::build_client(&base_url, None, None)
        .map_err(|error| format!("构建 HTTP 客户端失败：{error}"))?;
    let model = custom_model(
        &model_id,
        Api::AnthropicMessages,
        "anthropic",
        &base_url,
        output_cap(env),
    );
    let provider = AnthropicProvider::with_models_without_env_api_key(
        Some(api_key),
        http,
        vec![model.clone()],
    );
    Ok(Backend::Anthropic {
        provider: Arc::new(provider),
        model,
    })
}

fn openai_backend(env: &dyn EnvSource) -> Result<Backend, String> {
    let api_key = required(env, "OPENAI_COMPAT_API_KEY")?;
    let base_url = optional(env, "OPENAI_COMPAT_BASE_URL")
        .unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.to_string());
    let model_id = required(env, "OPENAI_COMPAT_MODEL")?;
    let http = rpi_ai::http::build_client(&base_url, None, None)
        .map_err(|error| format!("构建 HTTP 客户端失败：{error}"))?;
    // abb 只把 `openai-responses` 标成 responses，其余（含 openrouter/deepseek 两个
    // 预置端点）都是 chat。
    let responses = optional(env, "OPENAI_COMPAT_API").as_deref() == Some("responses");
    let api = if responses {
        Api::OpenaiResponses
    } else {
        Api::OpenaiCompletions
    };
    let model = custom_model(&model_id, api, "openai", &base_url, output_cap(env));
    if responses {
        let provider = OpenAiResponsesProvider::with_models_without_env_api_key(
            "openai",
            Some(api_key),
            http,
            vec![model.clone()],
        );
        Ok(Backend::OpenAiResponses {
            provider: Arc::new(provider),
            model,
        })
    } else {
        let provider = OpenAiCompletionsProvider::with_models_without_env_api_key(
            "openai",
            Some(api_key),
            http,
            vec![model.clone()],
        );
        Ok(Backend::OpenAiCompletions {
            provider: Arc::new(provider),
            model,
        })
    }
}

/// **显式构造**模型：id / 端点全由调用方给定，不查内置目录。
fn custom_model(id: &str, api: Api, provider_id: &str, base_url: &str, max_tokens: u64) -> Model {
    let mut model = Model::new(id, id, api, provider_id, base_url);
    model.max_tokens = max_tokens;
    model
}

/// 单回合输出上限：认 [`MAX_OUTPUT_TOKENS_ENV`]，缺省 [`DEFAULT_MAX_OUTPUT_TOKENS`]。
///
/// 非法/0 一律回落默认值而不报错：abb 今天不注入这个变量，将来注入也该是「调优」
/// 而非「必须正确」，没必要因此让 agent 起不来。
fn output_cap(env: &dyn EnvSource) -> u64 {
    env.get(MAX_OUTPUT_TOKENS_ENV)
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
}

/// 取必填 env；缺失或全空白即报错。
///
/// 「缺失就报错」不是苛刻，是与被替代的 fork 对齐：`crates/buzz-agent/src/config.rs`
/// 在 `ANTHROPIC_MODEL` 缺失时直接 `"config: ANTHROPIC_MODEL required"`（agent 起不来，
/// abb 如实看到 AgentDown）。静默替用户挑一个模型，等于替用户花他的钱。
fn required(env: &dyn EnvSource, key: &str) -> Result<String, String> {
    match env.get(key) {
        Some(value) if !value.trim().is_empty() => Ok(value.trim().to_string()),
        _ => Err(format!(
            "缺少 {key}（abb 在未配置该项时不会注入；被替代的 buzz-agent 同样在此硬失败）"
        )),
    }
}

/// 取可选 env；缺失或全空白视作未设置。
fn optional(env: &dyn EnvSource, key: &str) -> Option<String> {
    env.get(key)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// abb 注入的回合上界；缺失或不可解析时用 [`DEFAULT_MAX_ROUNDS`]。
pub fn max_rounds(env: &dyn EnvSource) -> u64 {
    env.get(MAX_ROUNDS_ENV)
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_ROUNDS)
}

/// `StreamFn` 是同步契约，`Provider::stream_simple` 是异步：用
/// `block_in_place` + `block_on` 搭桥（provider 的产出任务在 `stream_simple`
/// 返回前已经起好，所以拿到的流立刻可用）。与 rpi 官方示例 `examples/minimal`
/// 的写法一致——这是 tokio 文档认可的「同步外壳包异步生产者」模式。
///
/// **约束：必须跑在 multi-thread runtime 上**（`block_in_place` 的要求）。生产入口是
/// `#[tokio::main]`，默认就是；但 `#[tokio::test]` 默认是 current-thread，任何
/// 触发真实回合的测试都必须写 `#[tokio::test(flavor = "multi_thread")]`——
/// 否则会以「can call blocking only when running on the multi-threaded runtime」失败。
fn make_stream_fn<P: Provider + 'static>(provider: Arc<P>) -> StreamFn {
    rpi_agent::stream_fn(move |model, ctx, opts| {
        let provider = Arc::clone(&provider);
        let model = model.clone();
        let ctx = ctx.clone();
        let opts = opts.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async move { provider.stream_simple(&model, &ctx, &opts).await })
        })
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// 假环境：不碰进程全局 env，故用例之间无竞争、可并行。
    struct FakeEnv(HashMap<String, String>);

    impl FakeEnv {
        fn new(pairs: &[(&str, &str)]) -> Self {
            Self(
                pairs
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
            )
        }

        fn anthropic(extra: &[(&str, &str)]) -> Self {
            let mut base = vec![
                ("BUZZ_AGENT_PROVIDER", "anthropic"),
                ("ANTHROPIC_API_KEY", "sk-ant-probe"),
                ("ANTHROPIC_MODEL", "claude-sonnet-4-5"),
            ];
            base.extend_from_slice(extra);
            Self::new(&base)
        }

        fn openai(extra: &[(&str, &str)]) -> Self {
            let mut base = vec![
                ("BUZZ_AGENT_PROVIDER", "openai"),
                ("OPENAI_COMPAT_API_KEY", "sk-compat-probe"),
                ("OPENAI_COMPAT_MODEL", "deepseek-chat"),
                ("OPENAI_COMPAT_BASE_URL", "https://gateway.example.com"),
            ];
            base.extend_from_slice(extra);
            Self::new(&base)
        }
    }

    impl EnvSource for FakeEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    #[test]
    fn faux_env_selects_faux_backend() {
        let env = FakeEnv::new(&[("ABB_AGENT_FAUX_TEXT", "hello")]);
        let backend = select_with(&env).expect("faux 应可选");
        assert_eq!(backend.id(), "faux");
    }

    /// 离线**工具**通道：`ABB_AGENT_FAUX_TOOL` 应选 faux，且优先于纯文本通道。
    #[test]
    fn faux_tool_env_selects_faux_and_wins_over_text() {
        let env = FakeEnv::new(&[
            (
                FAUX_TOOL_ENV,
                r#"{"name":"abb-events__query","arguments":{"q":1}}"#,
            ),
            (FAUX_TEXT_ENV, "收尾文本"),
        ]);
        assert_eq!(select_with(&env).expect("faux 工具通道应可选").id(), "faux");
    }

    /// 非法 JSON 要报错，不能静默降级成纯文本（那样 E2E 会「通过」却根本没调工具）。
    #[test]
    fn faux_tool_env_rejects_malformed_json() {
        let err = select_with(&FakeEnv::new(&[(FAUX_TOOL_ENV, "{not json")]))
            .expect_err("非法 JSON 应报错");
        assert!(err.contains(FAUX_TOOL_ENV), "{err}");
    }

    #[test]
    fn faux_wins_over_provider_env() {
        let env = FakeEnv::new(&[
            ("ABB_AGENT_FAUX_TEXT", "hello"),
            ("BUZZ_AGENT_PROVIDER", "anthropic"),
        ]);
        assert_eq!(select_with(&env).expect("应选中 faux").id(), "faux");
    }

    /// abb 真值里 anthropic 送的取值就是 `"anthropic"`。
    #[test]
    fn anthropic_value_is_accepted() {
        let backend = select_with(&FakeEnv::anthropic(&[])).expect("anthropic 应可选");
        assert_eq!(backend.id(), "anthropic");
        assert_eq!(backend.model().id, "claude-sonnet-4-5");
    }

    /// **任意厂商模型 id 都要能用**：自定义网关（one-api 等）常用厂商原生 id，
    /// rpi 不限制命名空间。第一版去查内置目录，会把这条判成错误——那是实现偷懒。
    ///
    /// 注意 base_url 用的是**主机根**形状：rpi 自己拼路径，给 anthropic 写 `…/v1`
    /// 会得到 `/v1/v1/messages`（与被替代的 fork 同构，不是回归）。这条测试曾经拿
    /// `…/v1` 当「可用网关」的样本——那是在示范一个会坏掉的形状。
    #[test]
    fn arbitrary_model_id_on_custom_gateway_is_accepted() {
        let backend = select_with(&FakeEnv::anthropic(&[
            ("ANTHROPIC_MODEL", "my-vendor-internal-model-v3"),
            ("ANTHROPIC_BASE_URL", "https://one-api.internal.example"),
            ("ANTHROPIC_API_KEY", "sk-gateway"),
        ]))
        .expect("自定义网关 + 任意 id 必须可用");
        assert_eq!(backend.model().id, "my-vendor-internal-model-v3");
        assert_eq!(backend.model().base_url, "https://one-api.internal.example");
    }

    /// 输出上限：默认对齐被替代组件的 65_536；认 `BUZZ_AGENT_MAX_OUTPUT_TOKENS`；
    /// 非法/0 回落默认值。
    #[test]
    fn output_cap_defaults_to_fork_parity_and_honors_env() {
        assert_eq!(
            DEFAULT_MAX_OUTPUT_TOKENS, 65_536,
            "必须与被替代组件的默认一致（写 8192 会把输出上限静默缩 8 倍）"
        );
        assert_eq!(output_cap(&FakeEnv::new(&[])), 65_536);
        assert_eq!(
            output_cap(&FakeEnv::new(&[(MAX_OUTPUT_TOKENS_ENV, "1024")])),
            1024
        );
        assert_eq!(
            output_cap(&FakeEnv::new(&[(MAX_OUTPUT_TOKENS_ENV, "0")])),
            65_536
        );
        assert_eq!(
            output_cap(&FakeEnv::new(&[(MAX_OUTPUT_TOKENS_ENV, "abc")])),
            65_536
        );
        // 端到端：env 真的进了请求模型。
        let backend = select_with(&FakeEnv::anthropic(&[(MAX_OUTPUT_TOKENS_ENV, "4096")])).unwrap();
        assert_eq!(backend.model().max_tokens, 4096);
    }

    /// 未给模型**必须报错**：被替代的 fork 在这条路径硬失败（AgentDown，可见），
    /// 静默挑一个（库表首是 Opus 档）等于替用户花钱。
    #[test]
    fn missing_anthropic_model_is_an_error_not_a_silent_default() {
        let env = FakeEnv::new(&[
            ("BUZZ_AGENT_PROVIDER", "anthropic"),
            ("ANTHROPIC_API_KEY", "sk-ant-probe"),
        ]);
        let err = select_with(&env).expect_err("缺模型应报错");
        assert!(err.contains("ANTHROPIC_MODEL"), "{err}");
    }

    #[test]
    fn missing_anthropic_key_is_an_error() {
        let env = FakeEnv::new(&[
            ("BUZZ_AGENT_PROVIDER", "anthropic"),
            ("ANTHROPIC_MODEL", "claude-sonnet-4-5"),
        ]);
        let err = select_with(&env).expect_err("缺 key 应报错");
        assert!(err.contains("ANTHROPIC_API_KEY"), "{err}");
    }

    #[test]
    fn anthropic_base_url_env_overrides_endpoint() {
        let backend = select_with(&FakeEnv::anthropic(&[(
            "ANTHROPIC_BASE_URL",
            "https://gateway.example.com",
        )]))
        .unwrap();
        assert_eq!(backend.model().base_url, "https://gateway.example.com");
    }

    /// 未给 base_url 时用官方端点。
    #[test]
    fn anthropic_defaults_to_official_endpoint() {
        let backend = select_with(&FakeEnv::anthropic(&[])).unwrap();
        assert_eq!(backend.model().base_url, DEFAULT_ANTHROPIC_BASE_URL);
    }

    /// **openai 家族必须可用**：abb 把 `openai-chat`/`openai-responses`/`openrouter`/
    /// `deepseek` 全归并成 `BUZZ_AGENT_PROVIDER=openai`（本机 config 就是
    /// `openai-chat` + `deepseek-flash`）。
    #[test]
    fn openai_chat_family_is_wired() {
        let backend = select_with(&FakeEnv::openai(&[])).expect("openai 家族应可用");
        assert_eq!(backend.id(), "openai");
        assert_eq!(backend.model().id, "deepseek-chat");
        assert_eq!(backend.model().base_url, "https://gateway.example.com");
        assert_eq!(backend.model().api, Api::OpenaiCompletions);
    }

    #[test]
    fn openai_responses_style_is_wired() {
        let backend = select_with(&FakeEnv::openai(&[("OPENAI_COMPAT_API", "responses")])).unwrap();
        assert_eq!(backend.model().api, Api::OpenaiResponses);
        assert!(matches!(backend, Backend::OpenAiResponses { .. }));
    }

    /// `OPENAI_COMPAT_API` 缺省是 chat（abb 只有 `openai-responses` 才标 responses）。
    #[test]
    fn openai_missing_api_style_defaults_to_chat() {
        let backend = select_with(&FakeEnv::openai(&[("OPENAI_COMPAT_API", "chat")])).unwrap();
        assert_eq!(backend.model().api, Api::OpenaiCompletions);
    }

    #[test]
    fn missing_openai_model_is_an_error() {
        let env = FakeEnv::new(&[
            ("BUZZ_AGENT_PROVIDER", "openai"),
            ("OPENAI_COMPAT_API_KEY", "sk-compat-probe"),
        ]);
        let err = select_with(&env).expect_err("缺模型应报错");
        assert!(err.contains("OPENAI_COMPAT_MODEL"), "{err}");
    }

    #[test]
    fn missing_openai_key_is_an_error() {
        let env = FakeEnv::new(&[
            ("BUZZ_AGENT_PROVIDER", "openai"),
            ("OPENAI_COMPAT_MODEL", "deepseek-chat"),
        ]);
        let err = select_with(&env).expect_err("缺 key 应报错");
        assert!(err.contains("OPENAI_COMPAT_API_KEY"), "{err}");
    }

    /// **回归锁**：`max_tokens` 不能是 0。
    ///
    /// `clamp_max_tokens_to_context` 在 `context_window == 0` 时返回
    /// `max(1, max_tokens)`，所以 0 会静默变成「只回 1 个 token」——回复被截成
    /// 一个字，而日志里看不出任何异常。
    #[test]
    fn constructed_model_has_a_positive_output_cap() {
        for env in [FakeEnv::anthropic(&[]), FakeEnv::openai(&[])] {
            let backend = select_with(&env).unwrap();
            assert!(
                backend.model().max_tokens > 0,
                "max_tokens=0 会被夹成 1（只回 1 个 token）"
            );
            assert_eq!(backend.model().max_tokens, DEFAULT_MAX_OUTPUT_TOKENS);
        }
    }

    /// 配置 kind（`deepseek` 等）从来不是 `BUZZ_AGENT_PROVIDER` 的取值；
    /// 送进来要按「未知取值」报错，而不是被当成受支持的供应商。
    #[test]
    fn config_kind_is_not_a_valid_provider_value() {
        let err = select_with(&FakeEnv::new(&[("BUZZ_AGENT_PROVIDER", "deepseek")]))
            .expect_err("deepseek 不是合法取值");
        assert!(err.contains("未知"), "{err}");
        assert!(err.contains("deepseek"), "{err}");
    }

    #[test]
    fn missing_provider_names_the_faux_escape_hatch() {
        let err = select_with(&FakeEnv::new(&[])).expect_err("未配供应商应报错");
        assert!(
            err.contains("ABB_AGENT_FAUX_TEXT"),
            "错误应给出离线退路：{err}"
        );
    }

    /// 轮数上界：认 abb 的值，缺失/非法/0 回落到 200。
    #[test]
    fn max_rounds_follows_abb_contract() {
        assert_eq!(max_rounds(&FakeEnv::new(&[(MAX_ROUNDS_ENV, "37")])), 37);
        assert_eq!(max_rounds(&FakeEnv::new(&[])), DEFAULT_MAX_ROUNDS);
        assert_eq!(
            max_rounds(&FakeEnv::new(&[(MAX_ROUNDS_ENV, "abc")])),
            DEFAULT_MAX_ROUNDS
        );
        assert_eq!(
            max_rounds(&FakeEnv::new(&[(MAX_ROUNDS_ENV, "0")])),
            DEFAULT_MAX_ROUNDS
        );
    }
}
