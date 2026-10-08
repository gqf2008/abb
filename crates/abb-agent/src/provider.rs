//! provider 装配：把 abb 注入的供应商 env 翻译成 rpi provider + model。
//!
//! ## env 契约以 abb 侧真值为准
//!
//! 来源：`src/agent.rs::buzz_provider_env`（唯一注入点，经 `service.rs` 组装进
//! 子进程 env）。**第一版把词表写错了**（照抄了配置 kind 的名字），评审据此指出
//! 「照此匹配永不命中」——这里按 abb 实际发送的值重写：
//!
//! | 供应商 | `BUZZ_AGENT_PROVIDER` | 其余 |
//! | --- | --- | --- |
//! | anthropic | `"anthropic"` | `ANTHROPIC_API_KEY`、`ANTHROPIC_BASE_URL`(可选)、`ANTHROPIC_MODEL`(可选) |
//! | openai 家族 | `"openai"` | `OPENAI_COMPAT_API_KEY`、`OPENAI_COMPAT_BASE_URL`、`OPENAI_COMPAT_MODEL`、`OPENAI_COMPAT_API`(`chat`/`responses`) |
//!
//! 注意 `openai-chat` / `openai-responses` / `openrouter` / `deepseek` 是**配置里的
//! kind**，不是 env 里 `BUZZ_AGENT_PROVIDER` 的取值——它们都会被 abb 归并成
//! `"openai"`（API 形态另走 `OPENAI_COMPAT_API`）。
//!
//! `ANTHROPIC_API_KEY` 我们自己不读：abb 已经把它注入子进程 env，而 rpi 的
//! anthropic provider 默认允许回落到该 env（`allow_env_api_key: true`）。
//!
//! 另有**离线**通道 `ABB_AGENT_FAUX_TEXT=<文本>`：命中即用 rpi 的 faux provider
//! （不联网、无凭据、逐字可复现），供 smoke 与集成测试用。它是本包自己的调试开关，
//! 不属于 abb 的契约。

use std::sync::Arc;

use rpi_agent::StreamFn;
use rpi_ai::providers::anthropic::AnthropicProvider;
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_ai::{Model, Provider};

/// abb 回合作数的环境变量名（abb 恒送，默认 200）。
pub const MAX_ROUNDS_ENV: &str = "BUZZ_AGENT_MAX_ROUNDS";

/// abb 未指定时我们采用的回合上界，与其 `BUZZ_AGENT_MAX_ROUNDS` 默认值一致。
pub const DEFAULT_MAX_ROUNDS: u64 = 200;

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

/// 选中的执行后端（provider + 已解析好的 model）。
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
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 手写而非 derive：provider 内部（reqwest client 等）没有 Debug，
        // 而我们真正想看到的就是「选了哪个后端、哪个模型」。
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
        }
    }

    pub fn model(&self) -> &Model {
        match self {
            Backend::Faux { model, .. } | Backend::Anthropic { model, .. } => model,
        }
    }

    /// 本后端的 `StreamFn`（agent loop 每次请求都会调它）。
    pub fn stream_fn(&self) -> StreamFn {
        match self {
            Backend::Faux { provider, .. } => make_stream_fn(Arc::clone(provider)),
            Backend::Anthropic { provider, .. } => make_stream_fn(Arc::clone(provider)),
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
        "anthropic" => {
            // 不读 API key：abb 已经把 ANTHROPIC_API_KEY 注入子进程 env，rpi 的
            // anthropic provider 默认允许回落到它。
            let provider = Arc::new(AnthropicProvider::from_env());
            let model = anthropic_model(&provider, env)?;
            Ok(Backend::Anthropic { provider, model })
        }
        // abb 真值里 openai 家族一律归并成 "openai"；本刀未接。
        "openai" => Err("供应商 openai 尚未接入（第二刀：接 rpi-ai 的 openai 适配器并翻译 \
             OPENAI_COMPAT_API_KEY / OPENAI_COMPAT_BASE_URL / OPENAI_COMPAT_MODEL / OPENAI_COMPAT_API）"
            .to_string()),
        "" => Err(
            "未设置 BUZZ_AGENT_PROVIDER（abb 会送 anthropic 或 openai；\
             离线 smoke 可用 ABB_AGENT_FAUX_TEXT）"
                .to_string(),
        ),
        other => Err(format!(
            "未知的 BUZZ_AGENT_PROVIDER「{other}」（abb 只送 anthropic 或 openai；\
             openai-chat/openrouter/deepseek 等是配置 kind，env 里会被归并成 openai）"
        )),
    }
}

/// 解析 anthropic 的模型与端点。
///
/// `ANTHROPIC_MODEL` / `ANTHROPIC_BASE_URL` 是 abb 的既有契约，**必须认**：
/// 不认就会把用户选的（可能是便宜的）模型静默换成模型表第一个，自定义网关
/// 也会被忽略。第一版正是如此，评审实测反证。
fn anthropic_model(provider: &AnthropicProvider, env: &dyn EnvSource) -> Result<Model, String> {
    let wanted = env.get("ANTHROPIC_MODEL").unwrap_or_default();
    let wanted = wanted.trim();
    let mut model = if wanted.is_empty() {
        provider
            .models()
            .first()
            .cloned()
            .ok_or_else(|| "anthropic provider 没有可用模型".to_string())?
    } else {
        provider
            .models()
            .iter()
            .find(|m| m.id == wanted || m.name == wanted)
            .cloned()
            .ok_or_else(|| {
                format!("ANTHROPIC_MODEL={wanted} 不在 rpi-ai 的 anthropic 模型表里（不静默改选）")
            })?
    };
    if let Some(base_url) = env.get("ANTHROPIC_BASE_URL") {
        let base_url = base_url.trim();
        if !base_url.is_empty() {
            // rpi 的 anthropic provider 按 `model.base_url` 发请求，所以覆盖这里
            // 就等于支持自定义网关。
            model.base_url = base_url.to_string();
        }
    }
    Ok(model)
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

        /// 只要 anthropic 那一组键（避免每个用例都抄一遍）。
        fn anthropic(extra: &[(&str, &str)]) -> Self {
            let mut base = vec![("BUZZ_AGENT_PROVIDER", "anthropic")];
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
        assert_eq!(backend.model().id, backend.model().id); // 有模型即可
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
    }

    /// `ANTHROPIC_MODEL` 必须真的改选模型——不认就会静默换成模型表第一个。
    #[test]
    fn anthropic_model_env_actually_selects() {
        let all = AnthropicProvider::from_env();
        let wanted = all.models()[1].id.clone(); // 表里第二个（非默认项）
        let backend =
            select_with(&FakeEnv::anthropic(&[("ANTHROPIC_MODEL", wanted.as_str())])).unwrap();
        assert_eq!(backend.model().id, wanted, "ANTHROPIC_MODEL 未被采纳");
    }

    /// 未知模型 id 要**报错**，不能静默回退到第一个（第一版就是这样把用户的
    /// 便宜模型换成 Opus 档的）。
    #[test]
    fn unknown_anthropic_model_is_an_error_not_a_silent_fallback() {
        let err = select_with(&FakeEnv::anthropic(&[(
            "ANTHROPIC_MODEL",
            "totally-bogus-model",
        )]))
        .expect_err("未知模型应报错");
        assert!(err.contains("totally-bogus-model"), "{err}");
    }

    /// `ANTHROPIC_BASE_URL` 必须真的改端点（自定义网关）。
    #[test]
    fn anthropic_base_url_env_overrides_endpoint() {
        let backend = select_with(&FakeEnv::anthropic(&[(
            "ANTHROPIC_BASE_URL",
            "https://gateway.example.com",
        )]))
        .unwrap();
        assert_eq!(backend.model().base_url, "https://gateway.example.com");
    }

    /// `BUZZ_AGENT_PROVIDER=openai` 是 abb 真值；本刀未接，必须如实报「未接入」
    /// 且点明是 openai（而不是把 `openai-chat` 之类的配置 kind 当成取值）。
    #[test]
    fn openai_is_reported_as_not_yet_wired() {
        let err = select_with(&FakeEnv::new(&[("BUZZ_AGENT_PROVIDER", "openai")]))
            .expect_err("openai 本刀未接，应报错");
        assert!(err.contains("openai"), "{err}");
        assert!(
            err.contains("OPENAI_COMPAT_"),
            "应指出待翻译的 env 契约：{err}"
        );
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
