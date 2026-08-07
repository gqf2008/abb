//! 配置 —— 读写 ~/.agent-bridge/config.json（0600）。多 bot 结构。
//!
//! 新 schema：{owner_open_id, default_backend, bots:[{name, kind, enabled, backend, app_id, app_secret, bot_name, bot_open_id, primary_chat_id, wx_*, ding_*}]}
//! backend 是 per-bot 默认后端（空=跟随全局 default_backend）。
//! 兼容：load() 自动把旧单 bot 字段（顶层 app_id/app_secret/bot_name/bot_open_id）迁移成 bots[0]。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// 单个 bot 的配置。name 是隔离键（决定 workspace/jobs/sessions 子目录）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotConfig {
    /// 隔离名（目录名）。空则用 app_id 尾 6 位兜底，保证唯一且文件系统安全。
    #[serde(default)]
    pub name: String,
    /// bot 类型：feishu（默认）| wechat | dingtalk
    #[serde(default = "default_kind")]
    pub kind: String,
    /// 是否启用。false 时 service 不启动此 bot（仍在设置窗显示，可重新启用）。
    /// 默认 true；旧 config 无此字段时反序列化按 true（default_true）。
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub app_secret: String,
    /// 运行时自动填充（bot/v3/info）
    #[serde(default)]
    pub bot_name: String,
    #[serde(default)]
    pub bot_open_id: String,
    /// 该 bot 的主会话（与 owner 的私聊 p2p）chat_id —— 定时任务会话失效时的回落目标。
    #[serde(default)]
    pub primary_chat_id: String,
    /// 该 bot 的默认后端（claude|codex）。空 = 跟随全局 default_backend（向后兼容旧 config）。
    /// per-bot 独立：改飞书 bot 的后端不会再动到微信 bot。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub backend: String,
    /// 飞书：只响应这个 owner 的 open_id（ou_xxx）。per-bot——挪自全局 owner_open_id（飞书概念，
    /// 微信用 wx_user_id 不读它）。空 = 回落全局 owner_open_id（旧 config 兼容）。微信 bot 忽略。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub owner_open_id: String,
    /// 微信：登录拿到的 bot_token（飞书忽略）。等同凭证，0600。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub wx_token: String,
    /// 微信：登录拿到的 baseurl（空则用默认网关）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub wx_base_url: String,
    /// 微信：登录拿到的 ilink_user_id（owner 的微信标识；微信侧 should_respond 判据）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub wx_user_id: String,
    /// 钉钉：允许响应的用户 staffId（owner 过滤；空 = 响应所有发来消息的人）。
    /// 与飞书 owner_open_id、微信 wx_user_id 同职责，只是钉钉的用户标识格式。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ding_user_id: String,
    /// 钉钉：机器人编码（RobotCode）。企业内部应用机器人通常 = AppKey，个别后台单独展示时填它。
    /// 空 = 发送时用 app_id 兜底（对绝大多数企业内部应用机器人成立）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ding_robot_code: String,
    /// 该 bot 的模型供应商名（指向 Config.providers[].name）。空 = 跟随全局 default_provider。
    /// per-bot 独立：不同 bot 可走不同 key/模型（如飞书用官方 key、微信用 deepseek）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
}

/// 手动 Default：enabled 默认 true（derive(Default) 对 bool 给 false，会把新/迁移 bot 误设成停用）。
/// 所有 `BotConfig { ..Default::default() }` 站点因此都拿到 enabled=true。
impl Default for BotConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: default_kind(),
            enabled: true,
            app_id: String::new(),
            app_secret: String::new(),
            bot_name: String::new(),
            bot_open_id: String::new(),
            primary_chat_id: String::new(),
            backend: String::new(),
            owner_open_id: String::new(),
            wx_token: String::new(),
            wx_base_url: String::new(),
            wx_user_id: String::new(),
            ding_user_id: String::new(),
            ding_robot_code: String::new(),
            provider: String::new(),
        }
    }
}

fn default_kind() -> String {
    "feishu".to_string()
}

fn default_true() -> bool {
    true
}

impl BotConfig {
    /// 隔离键：name 优先，空则 app_id 尾 6 位；再空则 "default"。
    pub fn key(&self) -> String {
        if !self.name.is_empty() {
            return sanitize(&self.name);
        }
        // 按字符取尾 6 位（不能按字节切：非 ASCII 的 app_id 会落在 UTF-8 中间 panic）
        let chars: Vec<char> = self.app_id.chars().collect();
        if chars.len() >= 6 {
            let tail: String = chars[chars.len() - 6..].iter().collect();
            return sanitize(&tail);
        }
        "default".to_string()
    }

    /// 是否微信通道。
    pub fn is_wechat(&self) -> bool {
        self.kind == "wechat"
    }

    /// 是否钉钉通道。
    pub fn is_dingtalk(&self) -> bool {
        self.kind == "dingtalk"
    }

    /// 凭证是否齐备可跑（单一事实源：service 启动门槛 + Config::missing 都用它）。
    /// 飞书要 app_id+app_secret；微信要 wx_token+wx_user_id（扫码登录拿到）；
    /// 钉钉要 app_id（AppKey）+app_secret（AppSecret）。
    pub fn credentials_ready(&self) -> bool {
        if self.is_wechat() {
            !self.wx_token.is_empty() && !self.wx_user_id.is_empty()
        } else {
            !self.app_id.is_empty() && !self.app_secret.is_empty()
        }
    }

    /// 缺哪些凭证（仅人读，用于 missing() 报错）。
    fn missing_fields(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.is_wechat() {
            if self.wx_token.is_empty() {
                v.push("wx_token（微信需先扫码登录）".to_string());
            }
            if self.wx_user_id.is_empty() {
                v.push("wx_user_id（微信需先扫码登录）".to_string());
            }
        } else if self.is_dingtalk() {
            if self.app_id.is_empty() {
                v.push("app_id（钉钉 AppKey）".to_string());
            }
            if self.app_secret.is_empty() {
                v.push("app_secret（钉钉 AppSecret）".to_string());
            }
        } else {
            if self.app_id.is_empty() {
                v.push("app_id".to_string());
            }
            if self.app_secret.is_empty() {
                v.push("app_secret".to_string());
            }
        }
        v
    }

    /// 微信侧的 owner 判据：微信登录拿到的 ilink_user_id（should_respond 用它比对 from_user_id）。
    pub fn wx_owner(&self) -> &str {
        &self.wx_user_id
    }

    /// 钉钉侧的 owner 判据：允许响应的用户 staffId（should_respond 用它比对 senderStaffId）。
    /// 空 = 不设限（响应所有能发消息给机器人的人）。
    pub fn ding_owner(&self) -> &str {
        &self.ding_user_id
    }

    /// 钉钉发送用的机器人编码：显式配置优先，否则回落 AppKey（企业内部应用机器人默认相同）。
    pub fn ding_robot_code(&self) -> &str {
        if self.ding_robot_code.is_empty() {
            &self.app_id
        } else {
            &self.ding_robot_code
        }
    }

    /// 该 bot 的生效后端：自身 backend 非空用之，否则回落全局默认。返回值保证是 claude/codex。
    pub fn effective_backend<'a>(&'a self, global_default: &'a str) -> &'a str {
        if self.backend.is_empty() {
            global_default
        } else {
            &self.backend
        }
    }

    /// 该飞书 bot 的生效 owner：自身 owner_open_id 非空用之，否则回落全局 owner_open_id。
    /// 微信 bot 不用这个（用 wx_user_id）。
    pub fn effective_owner<'a>(&'a self, global_owner: &'a str) -> &'a str {
        if self.owner_open_id.is_empty() {
            global_owner
        } else {
            &self.owner_open_id
        }
    }

    /// 该 bot 的生效供应商名：自身 provider 非空用之，否则回落全局 default_provider。
    /// 返回空串 = 未配置供应商（claude 走 CC Switch / codex 走自认证的旧行为）。
    pub fn effective_provider<'a>(&'a self, global_default: &'a str) -> &'a str {
        if self.provider.is_empty() {
            global_default
        } else {
            &self.provider
        }
    }
}

/// 模型供应商配置。只支持 Anthropic 原生 + OpenAI 兼容（chat / responses）。
/// api_key 等同凭证，随 config.json 0600 保存，绝不进日志。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// 唯一键（BotConfig.provider / Config.default_provider 指向它）。
    #[serde(default)]
    pub name: String,
    /// 类型：anthropic | openai-chat | openai-responses
    #[serde(default = "default_provider_kind")]
    pub kind: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    /// 模型名（空 = 后端默认模型）。
    #[serde(default)]
    pub model: String,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: default_provider_kind(),
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
        }
    }
}

fn default_provider_kind() -> String {
    "anthropic".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub owner_open_id: String,
    #[serde(default)]
    pub default_backend: String,
    #[serde(default)]
    pub bots: Vec<BotConfig>,
    /// 模型供应商列表。空 = 未配置（claude 走 CC Switch / codex 走自认证的旧行为）。
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    /// 全局默认供应商名（指向 providers[].name）。bot.provider 非空时优先于它。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default_provider: String,

    // ── 旧单 bot 字段（仅用于自动迁移，迁移后清空）──
    #[serde(default, skip_serializing_if = "String::is_empty")]
    app_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    app_secret: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    bot_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    bot_open_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    primary_chat_id: String,
}

/// 文件名安全化：只留字母数字、-、_、中文等，去掉路径分隔与空白。
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect::<String>()
}

impl Config {
    pub fn path() -> PathBuf {
        crate::bridge_dir().join("config.json")
    }

    pub fn load() -> Result<Config> {
        let p = Self::path();
        if !p.exists() {
            return Ok(Config::default());
        }
        let text = fs::read_to_string(&p)
            .with_context(|| format!("读 config.json 失败: {}", p.display()))?;
        let mut cfg: Config =
            serde_json::from_str(&text).with_context(|| "config.json 不是合法 JSON")?;
        if cfg.default_backend.is_empty() {
            cfg.default_backend = "claude".into();
        }
        cfg.migrate_legacy();
        Ok(cfg)
    }

    /// 把旧单 bot 顶层字段迁移成 bots[0]（仅当 bots 为空且旧字段有值）。
    fn migrate_legacy(&mut self) {
        if !self.bots.is_empty() || self.app_id.is_empty() {
            return;
        }
        let bot = BotConfig {
            name: if self.bot_name.is_empty() {
                String::new()
            } else {
                self.bot_name.clone()
            },
            kind: "feishu".to_string(),
            app_id: std::mem::take(&mut self.app_id),
            app_secret: std::mem::take(&mut self.app_secret),
            bot_name: std::mem::take(&mut self.bot_name),
            bot_open_id: std::mem::take(&mut self.bot_open_id),
            primary_chat_id: std::mem::take(&mut self.primary_chat_id),
            ..Default::default()
        };
        self.bots.push(bot);
        crate::log!("[config] 已迁移旧单 bot 配置 → bots[0]");
    }

    /// 缺哪些必填项（缺则服务不能跑）。
    pub fn missing(&self) -> Vec<String> {
        let mut v = Vec::new();
        let mut has_feishu = false;
        let mut any_enabled = false;
        if self.bots.is_empty() {
            v.push("bots（至少配一个）".to_string());
        } else {
            for (i, b) in self.bots.iter().enumerate() {
                if !b.enabled {
                    continue; // 停用的 bot 不参与就绪判断（可能正因凭证不齐而停用）
                }
                any_enabled = true;
                if b.kind == "feishu" {
                    has_feishu = true;
                }
                for f in b.missing_fields() {
                    v.push(format!("bots[{i}].{f}"));
                }
            }
            if !any_enabled {
                v.push("bots（至少启用一个）".to_string());
            }
        }
        // owner_open_id 是飞书概念；只要求「启用的飞书 bot」的生效 owner 非空（per-bot 优先，回落全局）。
        // 微信 bot 不看这个（用各自 wx_user_id）。
        if has_feishu {
            let feishu_owner_missing = self
                .bots
                .iter()
                .filter(|b| b.enabled && b.kind == "feishu")
                .any(|b| b.effective_owner(&self.owner_open_id).is_empty());
            if feishu_owner_missing {
                v.push("owner_open_id（飞书 bot 需配 owner）".to_string());
            }
        }
        v
    }

    pub fn is_configured(&self) -> bool {
        self.missing().is_empty()
    }

    /// 原子写（tmp + rename），并设 0600。
    pub fn save(&self) -> Result<()> {
        let p = Self::path();
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = p.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self)?;
        fs::write(&tmp, text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&tmp, &p)?;
        Ok(())
    }

    /// 记录某 bot 的主会话（私聊 p2p）chat_id。收到私聊消息时调用；变化才落盘。
    pub fn save_primary_chat(bot_key: &str, chat_id: &str) {
        if chat_id.is_empty() {
            return;
        }
        if let Ok(mut c) = Config::load() {
            if let Some(b) = c.bots.iter_mut().find(|b| b.key() == bot_key) {
                if b.primary_chat_id != chat_id {
                    b.primary_chat_id = chat_id.to_string();
                    if let Err(e) = c.save() {
                        crate::log!("[config] 保存 primary_chat_id 失败: {e:#}");
                    }
                }
            }
        }
    }

    /// 读某 bot 的主会话 chat_id（可能为空：还没收到过私聊）。
    pub fn primary_chat(bot_key: &str) -> String {
        Config::load()
            .ok()
            .and_then(|c| {
                c.bots
                    .into_iter()
                    .find(|b| b.key() == bot_key)
                    .map(|b| b.primary_chat_id)
            })
            .unwrap_or_default()
    }

    /// 解析某 bot 的生效供应商：bot.provider（非空优先）→ 全局 default_provider → providers 里查名。
    /// 返回 None = 未配置供应商（走 CC Switch / codex 自认证的旧行为）；名不配位也 None + 警告。
    pub fn resolve_provider(&self, bot: &BotConfig) -> Option<&ProviderConfig> {
        let name = bot.effective_provider(&self.default_provider);
        if name.is_empty() {
            return None;
        }
        let found = self.providers.iter().find(|p| p.name == name);
        if found.is_none() {
            crate::log!(
                "[config] bot「{}」指向的供应商「{}」不在 providers 里，按未配置处理",
                bot.key(),
                name
            );
        }
        found
    }

    /// 按 bot_key 读其生效供应商（load + find）。agent.rs 每条消息调用；config.json 很小，
    /// 每次 load 与 save_primary_chat 等现有站点同理，可接受。
    pub fn provider_for_bot_key(bot_key: &str) -> Option<ProviderConfig> {
        Config::load().ok().and_then(|c| {
            c.bots
                .iter()
                .find(|b| b.key() == bot_key)
                .and_then(|b| c.resolve_provider(b).cloned())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_detection() {
        // 空配置：缺 bot；owner_open_id 只在有飞书 bot 时才强制
        let c = Config::default();
        assert!(c.missing().iter().any(|s| s.starts_with("bots")));
        // 飞书 bot：app_id/secret/owner_open_id 齐了就 configured
        let c2 = Config {
            owner_open_id: "o".into(),
            bots: vec![BotConfig {
                app_id: "a".into(),
                app_secret: "s".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(c2.is_configured());
        // 飞书 bot 缺 owner_open_id → 不 configured
        let c3 = Config {
            bots: vec![BotConfig {
                app_id: "a".into(),
                app_secret: "s".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(c3.missing().iter().any(|m| m.contains("owner_open_id")));
        // 纯微信 bot：不需要飞书 owner_open_id，但要 wx_token + wx_user_id
        let c4 = Config {
            bots: vec![BotConfig {
                kind: "wechat".into(),
                wx_token: "tok".into(),
                wx_user_id: "wxu".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(c4.is_configured(), "纯微信 bot 不应要求飞书字段");
        // 微信缺 token → 不 configured
        let c5 = Config {
            bots: vec![BotConfig {
                kind: "wechat".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(c5.missing().iter().any(|s| s.contains("wx_token")));
    }

    #[test]
    fn dingtalk_config() {
        // 钉钉 bot：app_id/app_secret 齐了就 configured；不强制飞书 owner_open_id
        let c = Config {
            bots: vec![BotConfig {
                kind: "dingtalk".into(),
                app_id: "dingappkey".into(),
                app_secret: "sec".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(c.is_configured(), "纯钉钉 bot 不应要求飞书字段");
        assert!(c.bots[0].is_dingtalk());
        assert!(c.bots[0].credentials_ready());

        // 缺 secret → 不 configured，错误信息点名 app_secret
        let c2 = Config {
            bots: vec![BotConfig {
                kind: "dingtalk".into(),
                app_id: "dingappkey".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(c2.missing().iter().any(|m| m.contains("app_secret")));

        // robotCode 回落 AppKey；显式配置优先
        let b = BotConfig {
            kind: "dingtalk".into(),
            app_id: "dingappkey".into(),
            ..Default::default()
        };
        assert_eq!(b.ding_robot_code(), "dingappkey");
        let b2 = BotConfig {
            kind: "dingtalk".into(),
            app_id: "dingappkey".into(),
            ding_robot_code: "dingrobot".into(),
            ..Default::default()
        };
        assert_eq!(b2.ding_robot_code(), "dingrobot");

        // 混合配置：飞书 bot 有 owner + 钉钉 bot 无 owner → 仍 configured（owner 要求只落在飞书 bot 上）
        let mixed = Config {
            owner_open_id: "ou_owner".into(),
            bots: vec![
                BotConfig {
                    kind: "feishu".into(),
                    app_id: "a".into(),
                    app_secret: "s".into(),
                    ..Default::default()
                },
                BotConfig {
                    kind: "dingtalk".into(),
                    app_id: "dingappkey".into(),
                    app_secret: "sec".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert!(mixed.is_configured());

        // ding_user_id 缺省空 = 不设限
        let b3: BotConfig = serde_json::from_str(r#"{"kind":"dingtalk","app_id":"x"}"#).unwrap();
        assert_eq!(b3.ding_owner(), "");
        // 序列化兼容：新字段不写旧 config 不报错
        let b4: BotConfig =
            serde_json::from_str(r#"{"kind":"dingtalk","app_id":"x","app_secret":"s"}"#).unwrap();
        assert!(b4.ding_user_id.is_empty());
        assert!(b4.ding_robot_code.is_empty());
    }

    #[test]
    fn legacy_migration() {
        let mut c = Config {
            owner_open_id: "ou_x".into(),
            app_id: "cli_old".into(),
            app_secret: "sec".into(),
            bot_name: "庆小丰".into(),
            bot_open_id: "ou_bot".into(),
            primary_chat_id: "oc_main".into(),
            ..Default::default()
        };
        c.migrate_legacy();
        assert_eq!(c.bots.len(), 1);
        assert_eq!(c.bots[0].app_id, "cli_old");
        assert_eq!(c.bots[0].primary_chat_id, "oc_main");
        assert!(c.app_id.is_empty(), "迁移后旧字段清空");
        // key 用 bot_name
        assert_eq!(c.bots[0].key(), "庆小丰");
    }

    #[test]
    fn bot_key_fallback() {
        let b = BotConfig {
            app_id: "cli_a75884b6c733900b".into(),
            ..Default::default()
        };
        assert_eq!(b.key(), "33900b"); // app_id 尾 6 位
        let named = BotConfig {
            name: "my bot/一号".into(),
            ..Default::default()
        };
        assert_eq!(named.key(), "mybot一号"); // 去空白/斜杠
    }

    #[test]
    fn provider_resolution() {
        let prov = |name: &str, kind: &str| ProviderConfig {
            name: name.into(),
            kind: kind.into(),
            base_url: "https://x".into(),
            api_key: "k".into(),
            model: "m".into(),
        };
        // 全局默认：bot.provider 空 → 跟随 default_provider
        let c = Config {
            default_provider: "g".into(),
            providers: vec![prov("g", "anthropic"), prov("b2", "openai-chat")],
            bots: vec![BotConfig {
                name: "bot1".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let p = c.resolve_provider(&c.bots[0]).unwrap();
        assert_eq!(p.name, "g");
        assert_eq!(p.kind, "anthropic");

        // 逐 bot 覆盖：bot.provider 非空 → 赢过全局默认
        let c2 = Config {
            default_provider: "g".into(),
            providers: vec![prov("g", "anthropic"), prov("b2", "openai-chat")],
            bots: vec![BotConfig {
                name: "bot1".into(),
                provider: "b2".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(c2.resolve_provider(&c2.bots[0]).unwrap().name, "b2");

        // 指向不存在的名 → None（按未配置处理，不 panic）
        let c3 = Config {
            default_provider: "ghost".into(),
            providers: vec![prov("g", "anthropic")],
            bots: vec![BotConfig {
                name: "bot1".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(c3.resolve_provider(&c3.bots[0]).is_none());

        // 完全没配供应商 → None（旧行为：CC Switch / codex 自认证）
        let c4 = Config {
            bots: vec![BotConfig {
                name: "bot1".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(c4.resolve_provider(&c4.bots[0]).is_none());
    }

    #[test]
    fn provider_serde_defaults() {
        // 旧 config 无 providers/default_provider 字段 → 反序列化为空，不报错
        let text = r#"{"owner_open_id":"o","default_backend":"claude","bots":[]}"#;
        let c: Config = serde_json::from_str(text).unwrap();
        assert!(c.providers.is_empty());
        assert!(c.default_provider.is_empty());
        // kind 缺省 = anthropic
        let p: ProviderConfig = serde_json::from_str(r#"{"name":"x"}"#).unwrap();
        assert_eq!(p.kind, "anthropic");
        // BotConfig 无 provider 字段 → 空
        let b: BotConfig = serde_json::from_str(r#"{"app_id":"a"}"#).unwrap();
        assert!(b.provider.is_empty());
        // 空 provider/default_provider 不落盘（skip_serializing_if）
        let c5 = Config::default();
        let s = serde_json::to_string(&c5).unwrap();
        assert!(!s.contains("default_provider"), "空 default_provider 不应序列化");
    }
}
