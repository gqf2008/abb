//! provider 装配：把 abb 的供应商 env 契约翻译成 rpi-ai 的 provider + model。
//!
//! abb 侧契约（`src/agent.rs::buzz_provider_env`，经 `service.rs` 注入子进程 env）：
//! `BUZZ_AGENT_PROVIDER` 取 `anthropic` / `openai-chat` / `openai-responses` /
//! `openrouter` / `deepseek`，配对应厂商的 API Key env。
//!
//! **第一刀只接 anthropic**；openai 系列（含 openrouter/deepseek 的兼容端点）
//! 留第二刀——它们是同一套 `openai_completions`/`openai_responses` 适配器，
//! 差别只在 base_url 与 api_style，接的时候按 abb 的 env 把 `Model.base_url`
//! 指过去即可。
//!
//! 另有一条**离线**通道 `ABB_AGENT_FAUX_TEXT=<文本>`：命中即用 rpi 的 faux
//! provider（不联网、无凭据、逐字可复现），供 smoke 与集成测试用。

use std::sync::Arc;

use rpi_agent::StreamFn;
use rpi_ai::providers::anthropic::AnthropicProvider;
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_ai::{Model, Provider};

/// 选中的执行后端。
///
/// 刻意用**具体类型**而不是 `Arc<dyn Provider>`：`Provider::stream_simple` 是
/// `async fn`（AFIT），直接做 trait 对象不稳；这里用泛型助手构造 `StreamFn`，
/// 把对象安全问题挡在外面。
pub enum Backend {
    Faux(Arc<FauxProvider>),
    Anthropic(Arc<AnthropicProvider>),
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 手写而非 derive：provider 内部（reqwest client 等）没有 Debug，
        // 而我们真正想看到的就是「选了哪个后端」。
        f.debug_tuple("Backend").field(&self.id()).finish()
    }
}

impl Backend {
    pub fn id(&self) -> &'static str {
        match self {
            Backend::Faux(_) => "faux",
            Backend::Anthropic(_) => "anthropic",
        }
    }

    fn models(&self) -> &[Model] {
        match self {
            Backend::Faux(p) => p.models(),
            Backend::Anthropic(p) => p.models(),
        }
    }

    /// 选本次会话使用的模型：`ABB_AGENT_MODEL` 按 id 或 name 命中，缺省取该
    /// provider 模型表的第一个。**选不到就报错**，不静默换一个——模型不是本 agent
    /// 该替用户决定的东西。
    pub fn model(&self) -> Result<Model, String> {
        match std::env::var("ABB_AGENT_MODEL") {
            Ok(wanted) if !wanted.trim().is_empty() => self
                .models()
                .iter()
                .find(|m| m.id == wanted || m.name == wanted)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "ABB_AGENT_MODEL={wanted} 在 provider「{}」的模型表里不存在",
                        self.id()
                    )
                }),
            _ => self
                .models()
                .first()
                .cloned()
                .ok_or_else(|| format!("provider「{}」没有可用模型", self.id())),
        }
    }

    /// 本后端的 `StreamFn`（agent loop 每次请求都会调它）。
    pub fn stream_fn(&self) -> StreamFn {
        match self {
            Backend::Faux(p) => make_stream_fn(Arc::clone(p)),
            Backend::Anthropic(p) => make_stream_fn(Arc::clone(p)),
        }
    }
}

/// 环境读取缝。
///
/// **为什么不直接在测试里 `set_var`**：env 是进程全局的，两个用例并行跑会互相踩
/// （第一版实测就踩了：一个设 `ABB_AGENT_FAUX_TEXT`、另一个删它，结果随机失败）。
/// 把读取收成 trait，测试用假环境注入，就不需要串行化也不需要碰全局状态。
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

/// 按环境选择后端。返回 `Err` 时**不 panic**：调用方把原因如实报到会话建立处，
/// 让「agent 起不来」可见可诊断（与 abb 的既有取向一致）。
pub fn select() -> Result<Backend, String> {
    select_with(&ProcessEnv)
}

/// [`select`] 的可注入版本（环境来源显式传入）。
pub fn select_with(env: &dyn EnvSource) -> Result<Backend, String> {
    if let Some(text) = env.get("ABB_AGENT_FAUX_TEXT") {
        let script = FauxScript::new().with_text(text);
        return Ok(Backend::Faux(FauxProvider::new(script)));
    }
    let provider = env.get("BUZZ_AGENT_PROVIDER").unwrap_or_default();
    match provider.trim() {
        "anthropic" => Ok(Backend::Anthropic(Arc::new(AnthropicProvider::from_env()))),
        "" => Err(
            "未设置 BUZZ_AGENT_PROVIDER（期望 anthropic；离线 smoke 可用 ABB_AGENT_FAUX_TEXT）"
                .to_string(),
        ),
        other => Err(format!(
            "供应商「{other}」尚未接入（第一刀只接 anthropic；openai 系列待第二刀）"
        )),
    }
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
    }

    impl EnvSource for FakeEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    /// 离线通道：命中 `ABB_AGENT_FAUX_TEXT` 即选 faux，且能拿到模型。
    #[test]
    fn faux_env_selects_faux_backend() {
        let env = FakeEnv::new(&[("ABB_AGENT_FAUX_TEXT", "hello")]);
        let backend = select_with(&env).expect("faux 应可选");
        assert_eq!(backend.id(), "faux");
        assert!(backend.model().is_ok(), "faux 必须自带可用模型");
    }

    /// faux 优先于供应商配置：离线 smoke 不该被本机 `BUZZ_AGENT_PROVIDER` 影响。
    #[test]
    fn faux_wins_over_provider_env() {
        let env = FakeEnv::new(&[
            ("ABB_AGENT_FAUX_TEXT", "hello"),
            ("BUZZ_AGENT_PROVIDER", "anthropic"),
        ]);
        assert_eq!(select_with(&env).expect("应选中 faux").id(), "faux");
    }

    /// 未接的供应商要给出**可读**错误，而不是 panic 或静默回退。
    #[test]
    fn unsupported_provider_is_a_readable_error() {
        let env = FakeEnv::new(&[("BUZZ_AGENT_PROVIDER", "deepseek")]);
        let err = select_with(&env).expect_err("deepseek 第一刀未接，应报错");
        assert!(err.contains("deepseek"), "错误里要带上是哪个供应商：{err}");
    }

    /// 什么都没配：报错里要给出可操作的下一步（而不是只说「失败」）。
    #[test]
    fn missing_provider_names_the_faux_escape_hatch() {
        let err = select_with(&FakeEnv::new(&[])).expect_err("未配供应商应报错");
        assert!(
            err.contains("ABB_AGENT_FAUX_TEXT"),
            "错误应给出离线退路：{err}"
        );
    }
}
