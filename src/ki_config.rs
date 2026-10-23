//! 随包执行层（ki）的供应商标配。
//!
//! 2026-10-23 起随包 `abb-agent` 的产物是 ki（pi-rust 构建后改名分发）。旧
//! abb-agent 从 `ABB_AGENT_PROVIDER` / `ANTHROPIC_*` / `OPENAI_COMPAT_*` 环境变量读
//! 供应商；ki 的 `acp serve` **不收任何参数**，只看自己的 agent 目录里的
//! `models.json`（目录可用 `KI_CODING_AGENT_DIR` 覆盖）。因此这里把 ABB 当前生效的
//! 那一家供应商落成一份**只含它一个 provider**的 models.json，并把子进程的 agent
//! 目录指过去。
//!
//! 三条由实测钉死的约束（2026-10-23）：
//!
//! 1. **只写一家 provider**：ki 无 `--model` 时取「第一个已认证模型」；多写一家就
//!    可能被别的先命中，用户在设置里选的模型会被静默换掉。
//! 2. **不注入 `ANTHROPIC_API_KEY` / `OPENAI_API_KEY`**：实测它们会让 ki 内置的
//!    anthropic/openai 身份变成「已认证」，默认模型选择抢在自定义 provider 前面
//!    （端到端实证：`ANTHROPIC_API_KEY` 在场时请求打到内置 anthropic 端点）。
//!    旧 abb-agent 的凭据 env 因此不再下发——它已不随包，升级期回滚要连本改动
//!    一起回滚。
//! 3. **目录独立于用户的 `~/.ki/agent`**：不读、不覆盖用户自己的 ki 配置；缺失的
//!    skills/extensions 也正是 ABB 想要的确定性（系统提示与工作区都由 ABB 注入）。
//!
//! 落盘发生在 **spawn 前**（[`materialize`]），不在配置装配期：装配函数被大量单测
//! 调用，而 `tools/check_test_isolation.sh` 禁止测试往真实数据目录写文件。

use std::path::{Path, PathBuf};

use crate::config::ProviderConfig;

/// spawn 前由 [`materialize`] 消费的载荷变量：models.json 内容。**不透传给子进程**。
pub(crate) const MODELS_JSON_ENV: &str = "ABB_KI_MODELS_JSON";

/// `KI_CODING_AGENT_DIR`：ki 的 agent 目录（models.json / auth.json / skills 等）。
pub(crate) const AGENT_DIR_ENV: &str = "KI_CODING_AGENT_DIR";

/// 一家供应商落到 ki agent 目录后的产物（路径 + 待写内容）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KiProvider {
    pub dir: PathBuf,
    pub models_json: String,
}

/// 供应商类型 → ki 的 `models.json` 协议名。
///
/// 白名单与 `config::PROVIDER_KINDS` 同口径；未知类型在装配期响亮失败（旧映射也
/// 是这么做的：宁可报错，不要装配半截 env 让 agent 起得来却打不了模型）。
fn api_for_kind(kind: &str) -> Result<&'static str, String> {
    match kind {
        "anthropic" => Ok("anthropic-messages"),
        "openai-chat" | "openrouter" | "deepseek" => Ok("openai-completions"),
        "openai-responses" => Ok("openai-responses"),
        other => Err(format!(
            "⚠️ 供应商类型 {other} 无法用于随包 ki（支持 anthropic / openai-chat / openai-responses / openrouter / deepseek）。"
        )),
    }
}

/// 请求端点：用户填了以填的为准（含自定义网关/本地端点），否则取该类型的预置端点
/// （仅 openrouter/deepseek 有预置；其余留空 = 让 ki 用它自己的默认端点）。
fn base_url_for(provider: &ProviderConfig) -> String {
    let configured = provider.base_url.trim();
    if configured.is_empty() {
        crate::config::provider_kind_default_base_url(&provider.kind).to_string()
    } else {
        configured.to_string()
    }
}

/// 稳定的 provider id：名字只取 ASCII 可读前缀，其余用 FNV-1a 摘要区分，
/// 保证「同名不同配置」不会共用一个目录，也不会因运行间随机化而漂移。
fn provider_id(provider: &ProviderConfig, base_url: &str) -> String {
    let mut slug = String::new();
    for ch in provider.name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if matches!(ch, '-' | '_' | '.') {
            slug.push(ch);
        } else if ch.is_whitespace() {
            slug.push('-');
        }
    }
    if slug.is_empty() {
        slug.push_str("provider");
    }
    slug.truncate(40);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a 64 offset basis
    for byte in format!("{}\u{1f}{base_url}\u{1f}{}", provider.kind, provider.model).bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("abb-{slug}-{:08x}", hash as u32)
}

/// 把一家供应商渲染成 ki 的 agent 目录 + models.json 文本。**不落盘**。
pub(crate) fn for_provider(provider: &ProviderConfig, root: &Path) -> Result<KiProvider, String> {
    let api = api_for_kind(&provider.kind)?;
    let model = provider.model.trim();
    if model.is_empty() {
        return Err(format!(
            "供应商「{}」没有模型 id：随包 ki 必须显式给出一家模型（设置 → 供应商里补 model）。",
            provider.name
        ));
    }
    let base_url = base_url_for(provider);
    let id = provider_id(provider, &base_url);

    let mut entry = serde_json::json!({
        "name": provider.name,
        "api": api,
        "apiKey": provider.api_key,
        "models": [{ "id": model, "name": provider.name }],
    });
    if !base_url.is_empty() {
        entry["baseUrl"] = base_url.clone().into();
    }
    // 键是计算出来的：`json!` 的裸标识符键会被当成字面量，这里用 Map 显式插入。
    let mut providers = serde_json::Map::new();
    providers.insert(id.clone(), entry);
    let doc = serde_json::json!({ "providers": providers });
    let models_json =
        serde_json::to_string_pretty(&doc).map_err(|e| format!("渲染 ki models.json 失败: {e}"))?;

    Ok(KiProvider {
        dir: root.join("ki-agents").join(id),
        models_json,
    })
}

/// 装配期：给子进程的 env（目录 + 待写内容）。不含任何凭据 env。
pub(crate) fn provider_env(
    provider: Option<&ProviderConfig>,
) -> Result<Vec<(String, String)>, String> {
    let Some(provider) = provider else {
        return Ok(Vec::new());
    };
    let prepared = for_provider(provider, &crate::bridge_dir())?;
    Ok(vec![
        (
            AGENT_DIR_ENV.to_string(),
            prepared.dir.display().to_string(),
        ),
        (MODELS_JSON_ENV.to_string(), prepared.models_json),
    ])
}

/// spawn 前：把载荷写进 `KI_CODING_AGENT_DIR/models.json`（0600、原子替换），
/// 并从 env 里摘掉载荷变量——子进程只看得到目录，看不到中间态。
///
/// 没有载荷（未配置供应商）⇒ 原样返回，行为与改动前一致。
pub(crate) fn materialize(env: &[(String, String)]) -> Result<Vec<(String, String)>, String> {
    let Some(models_json) = env
        .iter()
        .find(|(key, _)| key == MODELS_JSON_ENV)
        .map(|(_, value)| value.clone())
    else {
        return Ok(env.to_vec());
    };
    let Some(dir) = env
        .iter()
        .find(|(key, _)| key == AGENT_DIR_ENV)
        .map(|(_, value)| PathBuf::from(value))
    else {
        return Ok(env.to_vec());
    };

    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("创建 ki agent 目录失败（{}）: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // models.json 里有 API Key：目录只给本人（文件本身由原子写设 0600）。
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("收紧 ki agent 目录权限失败（{}）: {e}", dir.display()))?;
    }
    let path = dir.join("models.json");
    crate::atomic_write_sensitive(&path, &models_json)
        .map_err(|e| format!("写 ki models.json 失败（{}）: {e}", path.display()))?;

    Ok(env
        .iter()
        .filter(|(key, _)| key != MODELS_JSON_ENV)
        .cloned()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(name: &str, kind: &str, base_url: &str, model: &str) -> ProviderConfig {
        ProviderConfig {
            name: name.into(),
            kind: kind.into(),
            base_url: base_url.into(),
            api_key: "sk-secret".into(),
            model: model.into(),
            thinking: String::new(),
            context_window: 0,
        }
    }

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "abb-ki-config-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn kinds_map_to_ki_protocols() {
        let root = temp_root("kinds");
        for (kind, api) in [
            ("anthropic", "anthropic-messages"),
            ("openai-chat", "openai-completions"),
            ("openrouter", "openai-completions"),
            ("deepseek", "openai-completions"),
            ("openai-responses", "openai-responses"),
        ] {
            let p = provider("p", kind, "", "m-1");
            let prepared = for_provider(&p, &root).unwrap();
            let doc: serde_json::Value = serde_json::from_str(&prepared.models_json).unwrap();
            let entry = &doc["providers"]
                .as_object()
                .unwrap()
                .values()
                .next()
                .unwrap();
            assert_eq!(entry["api"], api, "kind={kind}");
            assert_eq!(entry["models"][0]["id"], "m-1");
            assert_eq!(entry["apiKey"], "sk-secret");
        }
    }

    #[test]
    fn preset_base_urls_are_filled_and_custom_ones_win() {
        let root = temp_root("base");
        let p = provider("openrouter", "openrouter", "", "m");
        let doc: serde_json::Value =
            serde_json::from_str(&for_provider(&p, &root).unwrap().models_json).unwrap();
        let entry = doc["providers"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert_eq!(entry["baseUrl"], "https://openrouter.ai/api/v1");

        let p = provider("gateway", "anthropic", "https://gw.example.com", "m");
        let doc: serde_json::Value =
            serde_json::from_str(&for_provider(&p, &root).unwrap().models_json).unwrap();
        let entry = doc["providers"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert_eq!(entry["baseUrl"], "https://gw.example.com");

        // 没有预置端点也不要求用户填：留空交给 ki 自己的默认端点。
        let p = provider("plain", "openai-chat", "", "m");
        let doc: serde_json::Value =
            serde_json::from_str(&for_provider(&p, &root).unwrap().models_json).unwrap();
        let entry = doc["providers"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert!(entry.get("baseUrl").is_none());
    }

    #[test]
    fn unknown_kind_and_missing_model_fail_loudly() {
        let root = temp_root("errors");
        let err = for_provider(&provider("x", "voodoo", "", "m"), &root).unwrap_err();
        assert!(err.contains("voodoo"), "{err}");
        let err = for_provider(&provider("x", "openai-chat", "", "  "), &root).unwrap_err();
        assert!(err.contains("没有模型 id"), "{err}");
    }

    #[test]
    fn provider_ids_are_stable_and_distinct() {
        let root = temp_root("ids");
        let a = for_provider(&provider("同名", "openai-chat", "", "m-1"), &root).unwrap();
        let b = for_provider(&provider("同名", "openai-chat", "", "m-1"), &root).unwrap();
        let c = for_provider(&provider("同名", "openai-chat", "", "m-2"), &root).unwrap();
        assert_eq!(
            a.dir, b.dir,
            "同一配置必须落在同一目录（否则旧目录无限累积）"
        );
        assert_ne!(a.dir, c.dir, "换模型必须换目录");
        let file = a.dir.file_name().unwrap().to_string_lossy().to_string();
        assert!(file.starts_with("abb-provider-"), "{file}");
    }

    #[test]
    fn materialize_writes_the_payload_and_strips_it() {
        let root = temp_root("materialize");
        let dir = root.join("agent-dir");
        let env = vec![
            (AGENT_DIR_ENV.to_string(), dir.display().to_string()),
            (
                MODELS_JSON_ENV.to_string(),
                "{\"providers\":{}}".to_string(),
            ),
            ("PATH".to_string(), "/usr/bin".to_string()),
        ];
        let out = materialize(&env).unwrap();
        assert!(
            !out.iter().any(|(k, _)| k == MODELS_JSON_ENV),
            "载荷变量不许透传给子进程"
        );
        assert!(out.iter().any(|(k, _)| k == AGENT_DIR_ENV));
        assert!(out.iter().any(|(k, v)| k == "PATH" && v == "/usr/bin"));
        let written = std::fs::read_to_string(dir.join("models.json")).unwrap();
        assert_eq!(written, "{\"providers\":{}}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("models.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "models.json 里有 API Key，必须 0600");
        }
    }

    #[test]
    fn materialize_without_payload_is_a_passthrough() {
        let env = vec![("PATH".to_string(), "/usr/bin".to_string())];
        assert_eq!(materialize(&env).unwrap(), env);
    }
}
