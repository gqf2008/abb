//! 配置 —— 读写 ~/.agent-bridge/config.json（0600）。多 bot 结构。
//!
//! 新 schema：{owner_open_id, default_backend, bots:[{name, kind, enabled, backend, app_id, app_secret, bot_name, bot_open_id, primary_chat_id, wx_*, ding_*}]}
//! backend 是 per-bot 默认后端（空=跟随全局 default_backend）。
//! 兼容：load() 自动把旧单 bot 字段（顶层 app_id/app_secret/bot_name/bot_open_id）迁移成 bots[0]。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 进程内 config.json 写锁：save_primary_chat 是「读-改-写」整份文件，
/// 多 bot/多消息并发时旧快照会互相覆盖（A 的 owner 被 B 覆盖 → 消息被静默丢弃）。
/// 高频写方（service 内多个消息任务）先串行化；GUI 是独立进程，跨进程仍有极小竞态。
static CONFIG_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
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
    /// 飞书：**owner（管理员）** 白名单（逗号/分号/空白分隔多个 open_id）。负责管理 bot、
    /// 生成授权码。与「授权者」（granted_ids，授权码添加）分开。微信 bot 忽略。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub owner_open_id: String,
    /// 飞书：**授权者**白名单（逗号/分号/空白分隔多个 open_id）——通过授权码获得的普通
    /// 使用权限，与 owner（管理员）分开。授权码消费时自动加入。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub granted_ids: String,
    /// 飞书：对话权限开关。false=仅授权用户（owner+授权者）可对话（默认）；true=任何人都可以
    /// 对话（公开，owner/授权者/授权码均不限制）。公开模式下授权码无意义（生成按钮禁用）。
    #[serde(default, skip_serializing_if = "not_open")]
    pub open_access: bool,
    /// 微信：登录拿到的 bot_token（飞书忽略）。等同凭证，0600。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub wx_token: String,
    /// 微信：登录拿到的 baseurl（空则用默认网关）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub wx_base_url: String,
    /// 微信：登录拿到的 ilink_user_id（owner 的微信标识；微信侧 should_respond 判据）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub wx_user_id: String,
    /// 微信：媒体 CDN 根地址（默认 https://novac2c.cdn.weixin.qq.com/c2c）。
    /// 入站图片/语音/文件下载解密用（#12 过渡能力）；空 = 客户端内置默认。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub wx_cdn_base_url: String,
    /// 钉钉：允许响应的用户 staffId（owner 过滤；空 = 响应所有发来消息的人）。
    /// 与飞书 owner_open_id、微信 wx_user_id 同职责，只是钉钉的用户标识格式。
    /// 旧单值字段；新体系用 ding_owner_ids（管理员白名单），load 时自动迁移过来。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ding_user_id: String,
    /// 钉钉：owner（管理员）白名单（逗号分隔 staffId）。与飞书 owner_open_id 对应。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ding_owner_ids: String,
    /// 钉钉：授权者白名单（逗号分隔 staffId）——授权码添加的普通用户。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ding_granted_ids: String,
    /// 钉钉：对话权限开关（false=仅授权用户可对话，true=任何人）。
    #[serde(default, skip_serializing_if = "not_open")]
    pub ding_open_access: bool,
    /// 钉钉：owner 展示名（staffId + 名字）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ding_owner_infos: Vec<OwnerInfo>,
    /// 钉钉：授权者展示名（staffId + 名字）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ding_granted_infos: Vec<OwnerInfo>,
    /// 钉钉：机器人编码（RobotCode）。企业内部应用机器人通常 = AppKey，个别后台单独展示时填它。
    /// 空 = 发送时用 app_id 兜底（对绝大多数企业内部应用机器人成立）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ding_robot_code: String,
    /// GitHub 能力（附挂到任意 kind 的 IM bot，非独立 bot kind）：PAT token。
    /// 空 = 不启用 github 指令/通知。注意：**不参与 credentials_ready 门槛**——github 是
    /// IM bot 的附加能力，IM 凭证齐了 bot 就该跑，gh_token 缺失只是该项能力不可用。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub gh_token: String,
    /// 仓库白名单：逗号分隔 `owner/repo`，支持 `owner/*`（整组织）。空 = 不限制。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub gh_repos: String,
    /// 新 issue 通知目标会话（该 bot 自己的群/私聊 chat_id）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub gh_notify_chat: String,
    /// 自己的 GitHub login：watch 通知过滤「自问自答」回显（须与 token 对应账号精确一致）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub gh_username: String,
    /// @提及映射：login:chat_id 对（逗号分隔），issue 评论 @提及 私信通知用。
    /// 无映射项 → 静默跳过（不群发骚扰）。chat_id 为该 bot 自己通道的群/私聊会话。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub gh_mention_map: String,
    /// 该 bot 的模型供应商名（指向 Config.providers[].name）。空 = 跟随全局 default_provider。
    /// per-bot 独立：不同 bot 可走不同 key/模型（如飞书用官方 key、微信用 deepseek）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    /// 待消费的授权码（一次性 + 过期）。owner 在 GUI 点「生成授权码」→ 把码发给目标用户 →
    /// 对方把码私聊给 bot → bridge 消费并把发送者加入 owner 白名单。重新生成会作废旧码。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_codes: Vec<OwnerCode>,
    /// 已授权用户展示信息（open_id + 名字）。owner_open_id 白名单是事实源，这里只补名字
    /// 供 GUI 列表展示（授权时经飞书联系人 API 反查，查不到用 open_id 兜底）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owner_infos: Vec<OwnerInfo>,
    /// 授权者展示信息（open_id + 名字）——授权码添加的普通授权用户。owner_infos 只管 owner。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub granted_infos: Vec<OwnerInfo>,
    /// 授权者（granted_ids 成员）agent 会话是否隔离受限（安全默认 true）：
    /// true=授权者驱动 agent 时走受限模式（仅工作区内读写 + $ABB_BIN 白名单，不能联网）；
    /// false=授权者与 owner 同权限（现状全权限）。owner 会话不受此字段影响，恒全权限。
    #[serde(default = "default_true", skip_serializing_if = "restrict_on")]
    pub restrict_granted_agent: bool,
    /// #51 免 @ 群聊开关：chat_id → "on"/"off"。off = 该群顶层消息免 @ 直接进 agent；
    /// 缺省（无条目）= 需要 @（默认，向后兼容旧 config）。仅顶层群聊 chat_id 记录；
    /// 私聊/话题不适用（本就无需 @）。值合法性只认 "off"，其余按需要 @ 处理。
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub mention_modes: std::collections::HashMap<String, String>,
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
            wx_cdn_base_url: String::new(),
            ding_user_id: String::new(),
            ding_robot_code: String::new(),
            gh_token: String::new(),
            gh_repos: String::new(),
            gh_notify_chat: String::new(),
            gh_username: String::new(),
            gh_mention_map: String::new(),
            ding_owner_ids: String::new(),
            ding_granted_ids: String::new(),
            ding_open_access: false,
            ding_owner_infos: Vec::new(),
            ding_granted_infos: Vec::new(),
            provider: String::new(),
            pending_codes: Vec::new(),
            owner_infos: Vec::new(),
            granted_ids: String::new(),
            granted_infos: Vec::new(),
            open_access: false,
            restrict_granted_agent: true,
            mention_modes: std::collections::HashMap::new(),
        }
    }
}

pub(crate) fn default_kind() -> String {
    "feishu".to_string()
}

/// skip_serializing_if：false（默认私有）不落盘，旧 config 兼容。
fn not_open(b: &bool) -> bool {
    !*b
}

/// skip_serializing_if：restrict_granted_agent 为 true（安全默认）不落盘，旧 config 兼容。
fn restrict_on(b: &bool) -> bool {
    *b
}

/// 授权码有效期（秒）：30 分钟。过期码仍保留在 pending_codes 里直到被消费/重新生成，
/// 目的是对方发来过期码时能给「已过期」的明确反馈（而不是静默无效）。
const OWNER_CODE_TTL_SECS: u64 = 30 * 60;

/// 一个待消费的授权码。code 随机 4 位大写字母数字；expires_at = unix 秒。
/// role 决定消费后落哪个白名单："owner"（管理员码）→ owner_open_id；"granted"（普通码）→ 授权者。
/// 4 位足够：字符集 28（去易混淆的 0/O/1/I）≈ 61 万组合，且一次性 + 30 分钟过期 + 仅 p2p
/// 接受——即使被猜中也只开一次口子，不构成长期访问面。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OwnerCode {
    pub code: String,
    pub expires_at: u64,
    /// "owner" | "granted"（默认 granted）
    pub role: String,
}

/// 已授权用户的展示信息（GUI 列表用）。open_id 是事实键，name 尽力而为。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OwnerInfo {
    pub open_id: String,
    pub name: String,
}

/// 在 BotConfig 上移除**授权者**（纯逻辑）：从 granted 白名单字符串删 open_id + 清对应展示名。
/// GUI 同步工作副本与 Config::remove_granted 共用，避免两处各拆一遍。按 bot kind 落位。
pub fn remove_granted_from_bot(bot: &mut BotConfig, open_id: &str) {
    let (_, _, granted_ids, granted_infos) = bot.whitelists_mut();
    let keep: Vec<&str> = granted_ids
        .split(|ch: char| ch == ',' || ch == ';' || ch.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != open_id)
        .collect();
    *granted_ids = keep.join(",");
    granted_infos.retain(|i| i.open_id != open_id);
}

/// 消费授权码的结果（bridge 据此回发用户可见文案）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerCodeResult {
    /// 授权成功：sender 已加入 owner 白名单。
    Granted,
    /// 码匹配但已过期（30 分钟）。
    Expired,
    /// 不匹配任何待消费码（当作普通消息，桥静默忽略）。
    NotFound,
}

/// 生成新授权码：4 位大写字母+数字（去掉易混淆的 0/O/1/I）。
/// 纯函数（fastrand 线程局部 RNG），可单测格式。
fn new_owner_code() -> String {
    const CHARS: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789"; // 无 0/O/1/I
    let mut s = String::with_capacity(4);
    for _ in 0..4 {
        s.push(CHARS[fastrand::usize(..CHARS.len())] as char);
    }
    s
}

/// 把 open_id 加入白名单字符串 + 展示名列表（去重；name 空用 open_id 兜底）。
/// owner 与授权者共用同一套追加逻辑（consume 按 role 决定写到哪份）。
fn whitelist_add(list: &mut String, infos: &mut Vec<OwnerInfo>, open_id: &str, name: &str) {
    if list.is_empty() {
        *list = open_id.to_string();
    } else if !is_owner_allowed(list, open_id) {
        list.push(',');
        list.push_str(open_id);
    }
    let disp = if name.is_empty() { open_id } else { name };
    if let Some(i) = infos.iter_mut().find(|i| i.open_id == open_id) {
        i.name = disp.to_string();
    } else {
        infos.push(OwnerInfo {
            open_id: open_id.to_string(),
            name: disp.to_string(),
        });
    }
}

/// 在 BotConfig 上消费授权码（纯逻辑，不落盘）：
/// 匹配（不区分大小写）pending 里的码 → 删除该码（一次性）；未过期 → 按码的 role 把 sender
/// 加入对应白名单（owner 码→owner_open_id+owner_infos；普通码→granted_ids+granted_infos）并记录
/// 展示名（name 空则用 open_id 兜底）返回 Granted；过期 → Expired；无匹配 → NotFound。
/// 顺带清理过期残留码。
fn consume_owner_code_on_bot(
    bot: &mut BotConfig,
    code: &str,
    sender: &str,
    name: &str,
    now: u64,
) -> OwnerCodeResult {
    let code = code.trim();
    if code.is_empty() {
        return OwnerCodeResult::NotFound;
    }
    let mut found: Option<(bool, String)> = None; // (未过期, role)
    bot.pending_codes.retain(|c| {
        if c.code.eq_ignore_ascii_case(code) {
            found = Some((c.expires_at > now, c.role.clone()));
            false // 消费（删除），一次性
        } else {
            c.expires_at > now // 顺带清理过期残留
        }
    });
    match found {
        Some((true, role)) => {
            let (owner_ids, owner_infos, granted_ids, granted_infos) = bot.whitelists_mut();
            if role == "owner" {
                whitelist_add(owner_ids, owner_infos, sender, name);
            } else {
                whitelist_add(granted_ids, granted_infos, sender, name);
            }
            OwnerCodeResult::Granted
        }
        Some((false, _)) => OwnerCodeResult::Expired,
        None => OwnerCodeResult::NotFound,
    }
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

    /// GitHub 能力开关：token 非空即可（白名单/通知群是可选增强）。
    /// 附挂在任意 kind 的 IM bot 上，不是独立通道——**不参与 credentials_ready 门槛**。
    pub fn is_github_capable(&self) -> bool {
        !self.gh_token.is_empty()
    }

    /// 白名单拆 Vec<owner/repo>（逗号/分号/空白分隔，与 is_owner_allowed 同款解析）。
    pub fn gh_repo_list(&self) -> Vec<String> {
        self.gh_repos
            .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// 仓库是否放行（空白名单 = 全部放行，见 github::repo_in_whitelist）。
    pub fn gh_allows_repo(&self, repo: &str) -> bool {
        crate::github::repo_in_whitelist(repo, &self.gh_repos)
    }

    /// @提及映射解析（login:chat_id 对，见 github::parse_mention_map）。
    pub fn gh_mention_map_list(&self) -> Vec<(String, String)> {
        crate::github::parse_mention_map(&self.gh_mention_map)
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

    /// 统一访问判定（不矛盾的单一入口）：公开开关开 → 放行所有人；否则只放行 owner ∪ 授权者
    /// 白名单成员。飞书用 open_id 字段、钉钉用 staffId 字段（各自独立，互不干扰）。
    /// 微信不走这套（wx_user_id 是登录身份，on_weixin 独立过滤）。
    /// 桥每次消息从 config 读最新值调用（授权后立即生效，不依赖启动快照）。
    pub fn access_allows(&self, sender_id: &str) -> bool {
        if self.is_wechat() {
            return true; // 微信由 on_weixin 的 wx_user_id 判据管，不在这里限制
        }
        if self.is_dingtalk() {
            if self.ding_open_access {
                return true;
            }
            let in_owner = !self.ding_owner_ids.is_empty()
                && is_owner_allowed(&self.ding_owner_ids, sender_id);
            let in_granted = !self.ding_granted_ids.is_empty()
                && is_owner_allowed(&self.ding_granted_ids, sender_id);
            return in_owner || in_granted;
        }
        if self.open_access {
            return true;
        }
        let in_owner =
            !self.owner_open_id.is_empty() && is_owner_allowed(&self.owner_open_id, sender_id);
        let in_granted =
            !self.granted_ids.is_empty() && is_owner_allowed(&self.granted_ids, sender_id);
        in_owner || in_granted
    }

    /// 会话发送者角色推导（与 access_allows 同构，准入闸顺路区分 owner 与授权者）：
    /// 命中 owner 白名单 → Owner（agent 全权限）；否则一律 Granted（agent 受限）——
    /// 公开模式（open_access）下的陌生人同样归 Granted。注意：空 owner 白名单 → 一律
    /// Granted（安全默认，不猜 Owner；is_owner_allowed 对空串返回 true 只管准入不管角色，
    /// 若需 owner 全权限请先把自己加进白名单）。微信恒 Owner
    /// （wx_user_id 是唯一 owner 判据，on_weixin 已先过滤，无授权者概念）。
    pub fn sender_role(&self, sender_id: &str) -> SenderRole {
        if self.is_wechat() {
            return SenderRole::Owner;
        }
        let owner_ids = if self.is_dingtalk() {
            &self.ding_owner_ids
        } else {
            &self.owner_open_id
        };
        if !owner_ids.is_empty() && is_owner_allowed(owner_ids, sender_id) {
            SenderRole::Owner
        } else {
            SenderRole::Granted
        }
    }

    /// 按 kind 取（owner 白名单, owner 展示名, 授权者白名单, 授权者展示名）的可变引用，
    /// 授权码消费/取消授权按 bot 类型落位（飞书 open_id / 钉钉 staffId）。
    fn whitelists_mut(
        &mut self,
    ) -> (
        &mut String,
        &mut Vec<OwnerInfo>,
        &mut String,
        &mut Vec<OwnerInfo>,
    ) {
        if self.is_dingtalk() {
            (
                &mut self.ding_owner_ids,
                &mut self.ding_owner_infos,
                &mut self.ding_granted_ids,
                &mut self.ding_granted_infos,
            )
        } else {
            (
                &mut self.owner_open_id,
                &mut self.owner_infos,
                &mut self.granted_ids,
                &mut self.granted_infos,
            )
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

/// 纯函数：owner 白名单是否放行 sender。空（或只有分隔符/空白）owner = 不设限（true）；
/// 非空时按逗号/分号/空白拆成多个 open_id，任一精确匹配即 true。bridge.rs 运行时判定与单测
/// 共用，避免两边各拆一遍。
pub fn is_owner_allowed(owner: &str, sender_id: &str) -> bool {
    let ids: Vec<&str> = owner
        .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .map(str::trim)
        .filter(|o| !o.is_empty())
        .collect();
    ids.is_empty() || ids.contains(&sender_id)
}

/// 会话发送者角色：owner（管理员，agent 全权限）/ granted（授权者，agent 受限）。
/// PendingItem/Job 的 role 字段落盘为小写字符串；Default=Owner 兼容旧数据（无角色
/// 时代的任务/待恢复消息按全权限处理，与现状一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SenderRole {
    #[default]
    Owner,
    Granted,
}

impl SenderRole {
    /// "granted"→Granted，其余（含空/未知）→Owner（旧数据与手动运行 CLI 兜底）。
    pub fn parse(s: &str) -> SenderRole {
        if s.eq_ignore_ascii_case("granted") {
            SenderRole::Granted
        } else {
            SenderRole::Owner
        }
    }

    /// 从 AGENT_BRIDGE_SENDER_ROLE env 推导（桥 spawn agent 时注入；CLI/guard 共用，
    /// 改名/新增取值只改这一处）。
    pub fn from_env() -> SenderRole {
        SenderRole::parse(&std::env::var("AGENT_BRIDGE_SENDER_ROLE").unwrap_or_default())
    }

    /// "owner" | "granted"（env 注入 / 日志用）。
    pub fn as_str(&self) -> &'static str {
        match self {
            SenderRole::Owner => "owner",
            SenderRole::Granted => "granted",
        }
    }
}

/// 受限模式判定（agent::run 的 spawn 分支 / bridge prompt 注入 / run_job 定时任务
/// 三处共用，防语义漂移）：role==Granted 且该 bot 的「授权者 agent 隔离」开关未放宽；
/// 配置读不到按安全默认 true。每次热读（授权/关开关即时生效）。
pub fn restrict_granted(role: SenderRole, bot_key: &str) -> bool {
    role == SenderRole::Granted
        && Config::bot_for_bot_key(bot_key)
            .map(|b| b.restrict_granted_agent)
            .unwrap_or(true)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub owner_open_id: String,
    #[serde(default)]
    pub default_backend: String,
    /// 跨会话投递总开关（#21）：默认关闭。开启后 agent 可通过 `$ABB_BIN deliver`
    /// 把消息投递到其它 bot 的会话（服务侧路由投递 + 失败兜底）。
    #[serde(default)]
    pub cross_delivery_enabled: bool,
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
        cfg.migrate_ding_owner();
        Ok(cfg)
    }

    /// 把旧单 bot 顶层字段迁移成 bots[0]（仅当 bots 为空且旧字段有值）。
    /// 全局 owner_open_id 一并复制进 bots[0]：迁移后 owner 判定只读 per-bot 字段（空=不设限），
    /// 不复制的话旧用户（原只响应 owner）会静默变成响应所有人。
    fn migrate_legacy(&mut self) {
        if !self.bots.is_empty() || self.app_id.is_empty() {
            return;
        }
        let mut bot = BotConfig {
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
        if !self.owner_open_id.is_empty() {
            bot.owner_open_id = self.owner_open_id.clone();
        }
        self.bots.push(bot);
        crate::log!("[config] 已迁移旧单 bot 配置 → bots[0]");
    }

    /// 旧钉钉单值 owner（ding_user_id）→ 新管理员白名单（ding_owner_ids）。幂等：
    /// ding_owner_ids 已有值则不覆盖（手动填过的不动）。
    fn migrate_ding_owner(&mut self) {
        for b in self.bots.iter_mut() {
            if b.is_dingtalk() && !b.ding_user_id.is_empty() && b.ding_owner_ids.is_empty() {
                b.ding_owner_ids = b.ding_user_id.clone();
            }
        }
    }

    /// 缺哪些必填项（缺则服务不能跑）。
    pub fn missing(&self) -> Vec<String> {
        let mut v = Vec::new();
        let mut any_enabled = false;
        if self.bots.is_empty() {
            v.push("bots（至少配一个）".to_string());
        } else {
            for (i, b) in self.bots.iter().enumerate() {
                if !b.enabled {
                    continue; // 停用的 bot 不参与就绪判断（可能正因凭证不齐而停用）
                }
                any_enabled = true;
                for f in b.missing_fields() {
                    v.push(format!("bots[{i}].{f}"));
                }
            }
            if !any_enabled {
                v.push("bots（至少启用一个）".to_string());
            }
        }
        // owner_open_id 不再作为启动门槛：空 = 不设限（响应所有人）；要限定到某人就在 bot 上填。
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

    // ── 未保存草稿（自动保存 + 崩溃恢复）──

    /// 草稿文件路径：与 config.json 同目录，保存成功后删除。
    pub fn draft_path() -> PathBuf {
        crate::bridge_dir().join("config.draft.json")
    }

    /// 读草稿（不存在/损坏 → None，不报错：草稿只是兜底，不该挡正常启动）。
    pub fn load_draft() -> Option<Config> {
        let p = Self::draft_path();
        if !p.exists() {
            return None;
        }
        let text = std::fs::read_to_string(&p).ok()?;
        let mut cfg: Config = serde_json::from_str(&text).ok()?;
        if cfg.default_backend.is_empty() {
            cfg.default_backend = "claude".into();
        }
        cfg.migrate_legacy();
        Some(cfg)
    }

    /// 草稿是否比正式配置新（mtime 比较；config.json 不存在时视作有新草稿）。
    pub fn draft_is_newer() -> bool {
        let dm = std::fs::metadata(Self::draft_path()).and_then(|m| m.modified());
        let cm = std::fs::metadata(Self::path()).and_then(|m| m.modified());
        match (dm, cm) {
            (Ok(dm), Ok(cm)) => dm > cm,
            (Ok(_), Err(_)) => true,
            _ => false,
        }
    }

    /// 写草稿：与 save 相同的原子写 + 0600（含密钥，权限必须收紧）。
    pub fn save_draft(&self) -> Result<()> {
        let p = Self::draft_path();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = p.with_extension("draft.json.tmp");
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&tmp, text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }

    /// 删除草稿（保存成功 / 用户选择丢弃时调用）。
    pub fn remove_draft() {
        let _ = std::fs::remove_file(Self::draft_path());
    }

    /// 记录某 bot 的主会话（私聊 p2p）chat_id。收到私聊消息时调用；变化才落盘。
    pub fn save_primary_chat(bot_key: &str, chat_id: &str) {
        if chat_id.is_empty() {
            return;
        }
        let _g = CONFIG_WRITE_LOCK.lock().unwrap();
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

    /// #51：设置某 bot 某群聊的 @ 门槛。mode: Some("on"/"off")；None = 删除条目恢复默认
    /// （需要 @）。与 save_primary_chat 同款：写锁 + load + find + save。
    pub fn set_mention_mode(bot_key: &str, chat_id: &str, mode: Option<&str>) {
        if chat_id.is_empty() {
            return;
        }
        let _g = CONFIG_WRITE_LOCK.lock().unwrap();
        if let Ok(mut c) = Config::load() {
            if let Some(b) = c.bots.iter_mut().find(|b| b.key() == bot_key) {
                match mode {
                    Some(m) => {
                        b.mention_modes.insert(chat_id.to_string(), m.to_string());
                    }
                    None => {
                        b.mention_modes.remove(chat_id);
                    }
                }
                if let Err(e) = c.save() {
                    crate::log!("[config] 保存 mention_modes 失败: {e:#}");
                }
            }
        }
    }

    /// #51：读某 bot 某群聊的 @ 门槛（热读，每次群消息门槛判定时调用）。
    /// 返回 None = 无条目 = 需要 @（默认）。
    pub fn mention_mode(bot_key: &str, chat_id: &str) -> Option<String> {
        Config::load().ok().and_then(|c| {
            c.bots
                .into_iter()
                .find(|b| b.key() == bot_key)
                .and_then(|b| b.mention_modes.get(chat_id).cloned())
        })
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

    /// 按 bot_key 读 BotConfig（load + find，provider_for_bot_key 同款热读）。
    /// agent.rs 每次受限判定调用，判定安全开关（restrict_granted_agent）跟随最新配置。
    pub fn bot_for_bot_key(bot_key: &str) -> Option<BotConfig> {
        Config::load()
            .ok()
            .and_then(|c| c.bots.into_iter().find(|b| b.key() == bot_key))
    }

    // ── 授权码（GUI 生成 / bridge 消费 / GUI 展示）──

    /// 生成新授权码并落盘：role 指定码类型（"owner"=管理员码→owner 白名单；"granted"=普通码→
    /// 授权者）。只作废**同 role** 的旧码（管理员码与普通码可同时存在）。返回 (码, 过期秒)。
    /// GUI「生成管理员授权码 / 生成授权码」按钮调用。
    pub fn generate_owner_code(bot_key: &str, role: &str) -> Option<(String, u64)> {
        let _g = CONFIG_WRITE_LOCK.lock().unwrap();
        let mut c = Config::load().ok()?;
        let bot = c.bots.iter_mut().find(|b| b.key() == bot_key)?;
        let code = new_owner_code();
        let expires_at = crate::chrono_lite::unix_secs() + OWNER_CODE_TTL_SECS;
        bot.pending_codes.retain(|c| c.role != role); // 同 role 旧码作废，另一 role 保留
        bot.pending_codes.push(OwnerCode {
            code: code.clone(),
            expires_at,
            role: role.to_string(),
        });
        if c.save().is_err() {
            return None;
        }
        Some((code, expires_at))
    }

    /// 消费授权码（bridge 收到疑似授权码的私聊消息时调用）：
    /// Granted → sender 已加入该 bot owner 白名单（含展示名）并落盘；Expired/NotFound → 只反馈不落盘。
    /// 找不到 bot / 落盘失败 → NotFound（不误授权，也静默不打扰）。
    pub fn consume_owner_code(
        bot_key: &str,
        code: &str,
        sender: &str,
        name: &str,
    ) -> OwnerCodeResult {
        let _g = CONFIG_WRITE_LOCK.lock().unwrap();
        let mut c = match Config::load() {
            Ok(c) => c,
            Err(_) => return OwnerCodeResult::NotFound,
        };
        let Some(bot) = c.bots.iter_mut().find(|b| b.key() == bot_key) else {
            return OwnerCodeResult::NotFound;
        };
        let r = consume_owner_code_on_bot(bot, code, sender, name, crate::chrono_lite::unix_secs());
        // 落盘失败 = 授权未生效，按未消费处理（不误授权）
        if r == OwnerCodeResult::Granted && c.save().is_err() {
            return OwnerCodeResult::NotFound;
        }
        r
    }

    /// 取消授权（GUI「取消授权」按钮）：从**授权者**白名单移除 open_id，并清掉对应展示名记录。
    /// owner（管理员）不走这——GUI owner 输入框直接编辑。
    pub fn remove_granted(bot_key: &str, open_id: &str) -> bool {
        let _g = CONFIG_WRITE_LOCK.lock().unwrap();
        let mut c = match Config::load() {
            Ok(c) => c,
            Err(_) => return false,
        };
        let Some(bot) = c.bots.iter_mut().find(|b| b.key() == bot_key) else {
            return false;
        };
        let before_ids = bot.granted_ids.clone();
        let before_infos = bot.granted_infos.len();
        remove_granted_from_bot(bot, open_id);
        if bot.granted_ids == before_ids && bot.granted_infos.len() == before_infos {
            return false; // 该用户本来就不在授权者列表（幂等）
        }
        c.save().is_ok()
    }

    /// 查询某 bot 当前未过期的授权码（GUI 展示「码 + 剩余分钟」）。返回 (role, 码, 过期秒)；
    /// 管理员码与普通码可同时存在。
    pub fn pending_owner_codes(bot_key: &str) -> Vec<(String, String, u64)> {
        let now = crate::chrono_lite::unix_secs();
        Config::load()
            .ok()
            .map(|c| {
                c.bots
                    .iter()
                    .find(|b| b.key() == bot_key)
                    .map(|b| {
                        b.pending_codes
                            .iter()
                            .filter(|c| c.expires_at > now)
                            .map(|c| (c.role.clone(), c.code.clone(), c.expires_at))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_detection() {
        // 空配置：缺 bot
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
        // 飞书 bot 缺 owner_open_id → 仍 configured（owner 空=不设限，不是启动门槛）
        let c3 = Config {
            bots: vec![BotConfig {
                app_id: "a".into(),
                app_secret: "s".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(c3.is_configured(), "缺 owner 不应阻止启动");
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

        // ding_owner_ids 缺省空 = 不设限；旧 ding_user_id 迁移进 ding_owner_ids
        let b3: BotConfig = serde_json::from_str(r#"{"kind":"dingtalk","app_id":"x"}"#).unwrap();
        assert_eq!(b3.ding_owner_ids, "");
        let mut c_mig = Config {
            bots: vec![serde_json::from_str(
                r#"{"kind":"dingtalk","app_id":"x","ding_user_id":"u9"}"#,
            )
            .unwrap()],
            ..Default::default()
        };
        c_mig.migrate_ding_owner();
        assert_eq!(
            c_mig.bots[0].ding_owner_ids, "u9",
            "旧 ding_user_id 应迁移到管理员白名单"
        );
        // 序列化兼容：新字段不写旧 config 不报错
        let b4: BotConfig =
            serde_json::from_str(r#"{"kind":"dingtalk","app_id":"x","app_secret":"s"}"#).unwrap();
        assert!(b4.ding_user_id.is_empty());
        assert!(b4.ding_robot_code.is_empty());
    }

    #[test]
    fn github_fields_serde_default_and_capable() {
        // 旧 config 无 gh_* 字段 → 空串、能力关
        let b: BotConfig =
            serde_json::from_str(r#"{"kind":"feishu","app_id":"x","app_secret":"s"}"#).unwrap();
        assert!(!b.is_github_capable());
        assert!(b.gh_repo_list().is_empty());

        // 配了 token → 能力开；白名单解析 + 放行判断
        let b: BotConfig = serde_json::from_str(
            r#"{"kind":"feishu","app_id":"x","app_secret":"s","gh_token":"ghp_x","gh_repos":"o/a, o/*","gh_notify_chat":"oc_g","gh_username":"bot","gh_mention_map":"alice:oc_1,bob:oc_2"}"#,
        )
        .unwrap();
        assert!(b.is_github_capable());
        assert_eq!(b.gh_repo_list(), vec!["o/a".to_string(), "o/*".to_string()]);
        assert!(b.gh_allows_repo("o/a"));
        assert!(b.gh_allows_repo("o/any"));
        assert!(!b.gh_allows_repo("x/y"));
        assert_eq!(b.gh_notify_chat, "oc_g");
        assert_eq!(b.gh_username, "bot");
        assert_eq!(
            b.gh_mention_map_list(),
            vec![
                ("alice".to_string(), "oc_1".to_string()),
                ("bob".to_string(), "oc_2".to_string())
            ]
        );
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
        // 旧全局 owner 复制进 bots[0]：迁移后 owner 判定只读 per-bot，不复制会静默变成响应所有人
        assert_eq!(c.bots[0].owner_open_id, "ou_x");
    }

    #[test]
    fn owner_unset_is_closed_until_activated() {
        // 函数层语义：白名单为空时 is_owner_allowed 对任意 sender 放行（桥层在 owner 空时
        // 短路为「未授权封闭」，不会用本函数判空 owner——见 bridge on_payload 访问控制段）。
        let b = BotConfig {
            name: "未授权 bot".into(),
            ..Default::default()
        };
        assert_eq!(b.owner_open_id, "", "空 owner 表示未授权");
        // 配了 owner 才放行白名单内的人
        let b2 = BotConfig {
            owner_open_id: "ou_only_me".into(),
            ..Default::default()
        };
        assert_eq!(b2.owner_open_id, "ou_only_me");
        assert!(is_owner_allowed("ou_only_me", "ou_only_me"));
        assert!(!is_owner_allowed("ou_only_me", "ou_other"));
        // 全局 owner_open_id 不再影响 per-bot 判定（仅迁移/打印用）
        let mut c = Config {
            owner_open_id: "ou_global".into(),
            bots: vec![BotConfig {
                name: "x".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(c.bots[0].owner_open_id, "", "per-bot 空时全局不应兜底");
        // 新格式 config（bots 非空）不触发 legacy 迁移，全局字段保持原样
        assert_eq!(c.owner_open_id, "ou_global");
        let before = c.bots.len();
        c.migrate_legacy();
        assert_eq!(c.bots.len(), before, "新格式不迁移");
    }

    #[test]
    fn owner_allows_multiple_open_ids() {
        // 逗号分隔多个 owner：任一精确匹配即放行，其它人拒绝
        let owner = "ou_a, ou_b,ou_c";
        assert!(is_owner_allowed(owner, "ou_a"));
        assert!(is_owner_allowed(owner, "ou_b"));
        assert!(is_owner_allowed(owner, "ou_c"));
        assert!(!is_owner_allowed(owner, "ou_d"));
        // 分号 / 换行 / 空白分隔同样生效
        assert!(is_owner_allowed("ou_a; ou_b\nou_c", "ou_c"));
        // 空串 / 全空白 = 不设限
        assert!(is_owner_allowed("", "ou_x"));
        assert!(is_owner_allowed("  ,  ; ", "ou_x"));
        // 前缀不误伤（精确匹配，不是 contains）
        assert!(!is_owner_allowed("ou_ab", "ou_a"));
        // 与自由函数一致（BotConfig 侧判定走 config::is_owner_allowed）
        assert!(is_owner_allowed("ou_x, ou_y", "ou_y"));
        assert!(!is_owner_allowed("ou_x, ou_y", "ou_z"));
    }

    // ── 发送者角色（owner=全权限 / granted=受限）──

    #[test]
    fn sender_role_owner_vs_granted() {
        // 飞书：owner 白名单命中 → Owner；授权者 → Granted；公开模式陌生人 → Granted
        let bot = BotConfig {
            kind: "feishu".into(),
            owner_open_id: "ou_boss, ou_admin".into(),
            granted_ids: "ou_friend".into(),
            ..Default::default()
        };
        assert_eq!(bot.sender_role("ou_boss"), SenderRole::Owner);
        assert_eq!(bot.sender_role("ou_admin"), SenderRole::Owner);
        assert_eq!(bot.sender_role("ou_friend"), SenderRole::Granted);
        assert_eq!(bot.sender_role("ou_stranger"), SenderRole::Granted);
        // 公开模式：owner 命中 → Owner，陌生人 → Granted（受限）
        let open = BotConfig {
            kind: "feishu".into(),
            owner_open_id: "ou_boss".into(),
            open_access: true,
            ..Default::default()
        };
        assert_eq!(open.sender_role("ou_boss"), SenderRole::Owner);
        assert_eq!(open.sender_role("ou_stranger"), SenderRole::Granted);
        // 空 owner 白名单：白名单里没有任何人 → 一律 Granted（安全默认，不猜 Owner）
        let no_owner = BotConfig {
            kind: "feishu".into(),
            ..Default::default()
        };
        assert_eq!(no_owner.sender_role("ou_anyone"), SenderRole::Granted);
    }

    #[test]
    fn sender_role_dingtalk_uses_staff_ids() {
        let bot = BotConfig {
            kind: "dingtalk".into(),
            ding_owner_ids: "staff1, staff2".into(),
            ding_granted_ids: "staff3".into(),
            ..Default::default()
        };
        assert_eq!(bot.sender_role("staff1"), SenderRole::Owner);
        assert_eq!(bot.sender_role("staff2"), SenderRole::Owner);
        assert_eq!(bot.sender_role("staff3"), SenderRole::Granted);
        assert_eq!(bot.sender_role("staff4"), SenderRole::Granted);
        // 公开模式陌生人 → Granted
        let open = BotConfig {
            kind: "dingtalk".into(),
            ding_owner_ids: "staff1".into(),
            ding_open_access: true,
            ..Default::default()
        };
        assert_eq!(open.sender_role("staff1"), SenderRole::Owner);
        assert_eq!(open.sender_role("staff9"), SenderRole::Granted);
    }

    #[test]
    fn sender_role_wechat_always_owner() {
        // 微信只有 owner（wx_user_id 判据，on_weixin 先过滤），无授权者概念
        let bot = BotConfig {
            kind: "wechat".into(),
            wx_user_id: "wx_owner".into(),
            ..Default::default()
        };
        assert_eq!(bot.sender_role("wx_owner"), SenderRole::Owner);
        assert_eq!(bot.sender_role("wx_anyone"), SenderRole::Owner);
    }

    #[test]
    fn sender_role_parse_and_serde() {
        assert_eq!(SenderRole::parse("granted"), SenderRole::Granted);
        assert_eq!(SenderRole::parse("GRANTED"), SenderRole::Granted);
        assert_eq!(SenderRole::parse("owner"), SenderRole::Owner);
        assert_eq!(SenderRole::parse(""), SenderRole::Owner); // 旧数据/手动 CLI 兜底
        assert_eq!(SenderRole::parse("未知"), SenderRole::Owner);
        assert_eq!(SenderRole::Owner.as_str(), "owner");
        assert_eq!(SenderRole::Granted.as_str(), "granted");
        // 落盘 lowercase；round-trip 保真
        let s = serde_json::to_string(&SenderRole::Granted).unwrap();
        assert_eq!(s, "\"granted\"");
        assert_eq!(
            serde_json::from_str::<SenderRole>(&s).unwrap(),
            SenderRole::Granted
        );
    }

    #[test]
    fn restrict_granted_agent_serde_compat() {
        // 手动 Default 与旧 config 反序列化都落到安全默认 true
        assert!(BotConfig::default().restrict_granted_agent);
        let bot: BotConfig = serde_json::from_str(r#"{"name":"b1","kind":"feishu"}"#).unwrap();
        assert!(bot.restrict_granted_agent);
        // round-trip：默认 true 时字段不落盘（skip_serializing_if 保旧 config 兼容）
        let s = serde_json::to_string(&bot).unwrap();
        assert!(!s.contains("restrict_granted_agent"));
        let back: BotConfig = serde_json::from_str(&s).unwrap();
        assert!(back.restrict_granted_agent);
        // 显式关闭（GUI 放宽）→ 落盘并往返保真
        let off = BotConfig {
            restrict_granted_agent: false,
            ..Default::default()
        };
        let s2 = serde_json::to_string(&off).unwrap();
        assert!(s2.contains("\"restrict_granted_agent\":false"));
        let back2: BotConfig = serde_json::from_str(&s2).unwrap();
        assert!(!back2.restrict_granted_agent);
    }

    // ── 授权码（GUI 生成 / 对方发码给 bot 自动授权）──

    #[test]
    fn owner_code_generated_format() {
        // 4 位大写字母+数字（4 位好记：28 字符集≈61 万组合 + 一次性/过期/仅 p2p 兜底），
        // 且不含易混淆的 0/O/1/I
        let c = new_owner_code();
        assert_eq!(c.len(), 4);
        assert!(c
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit()));
        assert!(!c.contains(['0', 'O', '1', 'I']));
        // 随机性：多生成几个不至于全相同（4 位 20 个样本几乎必重复，但不应全同）
        let set: std::collections::HashSet<String> = (0..20).map(|_| new_owner_code()).collect();
        assert!(set.len() > 1, "授权码应有随机性");
    }

    fn bot_with_code(code: &str, expires_at: u64) -> BotConfig {
        BotConfig {
            owner_open_id: "ou_boss".into(),
            pending_codes: vec![OwnerCode {
                code: code.into(),
                expires_at,
                role: "granted".into(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn owner_code_consume_adds_granted_not_owner() {
        let now = crate::chrono_lite::unix_secs();
        let mut bot = bot_with_code("K3M8XQ2P", now + 1800);
        let r = consume_owner_code_on_bot(&mut bot, "K3M8XQ2P", "ou_friend", "张三", now);
        assert_eq!(r, OwnerCodeResult::Granted);
        // 授权码只产生「授权者」，owner（管理员）不动
        assert!(is_owner_allowed(&bot.granted_ids, "ou_friend"));
        assert!(
            !is_owner_allowed(&bot.owner_open_id, "ou_friend"),
            "owner 不该被授权码污染"
        );
        assert_eq!(bot.owner_open_id, "ou_boss", "owner 保持手填值");
        assert!(bot.pending_codes.is_empty(), "码一次性，消费后删除");
        // 展示名随授权记录（GUI 授权者列表能显示「谁」）
        assert_eq!(bot.granted_infos.len(), 1);
        assert_eq!(bot.granted_infos[0].open_id, "ou_friend");
        assert_eq!(bot.granted_infos[0].name, "张三");
        // 私有模式下 access_allows 放行 owner 与授权者
        assert!(bot.access_allows("ou_boss"));
        assert!(bot.access_allows("ou_friend"));
        assert!(!bot.access_allows("ou_stranger"));
    }

    #[test]
    fn owner_code_consume_name_falls_back_to_open_id() {
        // 查不到名字（飞书 API 失败/非联系人）→ 用 open_id 兜底，授权照常成功
        let now = crate::chrono_lite::unix_secs();
        let mut bot = bot_with_code("NAME000", now + 1800);
        let r = consume_owner_code_on_bot(&mut bot, "NAME000", "ou_anon", "", now);
        assert_eq!(r, OwnerCodeResult::Granted);
        assert_eq!(bot.granted_infos[0].name, "ou_anon", "名字空时兜底 open_id");
    }

    #[test]
    fn remove_granted_strips_whitelist_and_info() {
        // 从多成员授权者白名单里精确移除一个：白名单字符串 + 展示名记录都清掉，其它成员保留
        let mut bot = BotConfig {
            granted_ids: "ou_a, ou_b, ou_c".into(),
            granted_infos: vec![
                OwnerInfo {
                    open_id: "ou_a".into(),
                    name: "甲".into(),
                },
                OwnerInfo {
                    open_id: "ou_b".into(),
                    name: "乙".into(),
                },
                OwnerInfo {
                    open_id: "ou_c".into(),
                    name: "丙".into(),
                },
            ],
            ..Default::default()
        };
        remove_granted_from_bot(&mut bot, "ou_b");
        assert_eq!(bot.granted_ids, "ou_a,ou_c");
        assert!(is_owner_allowed(&bot.granted_ids, "ou_a"));
        assert!(!is_owner_allowed(&bot.granted_ids, "ou_b"));
        assert_eq!(bot.granted_infos.len(), 2);
        assert!(bot.granted_infos.iter().all(|i| i.open_id != "ou_b"));

        // 移除最后一个 → 授权者清空（私有模式下只剩 owner 能对话；owner 也空则封闭）
        remove_granted_from_bot(&mut bot, "ou_a");
        remove_granted_from_bot(&mut bot, "ou_c");
        assert!(bot.granted_ids.is_empty());
        assert!(bot.granted_infos.is_empty());
        assert!(!bot.access_allows("ou_a"), "授权者移除后不再放行");

        // 移除不存在的成员 = 幂等无副作用
        let mut bot2 = BotConfig {
            granted_ids: "ou_x".into(),
            granted_infos: vec![OwnerInfo {
                open_id: "ou_x".into(),
                name: "x".into(),
            }],
            ..Default::default()
        };
        remove_granted_from_bot(&mut bot2, "ou_none");
        assert_eq!(bot2.granted_ids, "ou_x");
        assert_eq!(bot2.granted_infos.len(), 1);
    }

    #[test]
    fn open_access_allows_everyone_and_ignores_lists() {
        // 公开开关：任何人都能对话，owner/授权者/授权码均不限制（无矛盾：公开优先）
        let mut bot = BotConfig {
            owner_open_id: "ou_boss".into(),
            granted_ids: "ou_friend".into(),
            open_access: true,
            ..Default::default()
        };
        assert!(bot.access_allows("ou_boss"));
        assert!(bot.access_allows("ou_friend"));
        assert!(bot.access_allows("ou_stranger"), "公开模式任何人可对话");
        // 私有模式（默认）才限制
        bot.open_access = false;
        assert!(!bot.access_allows("ou_stranger"));
        assert!(bot.access_allows("ou_boss"));
        assert!(bot.access_allows("ou_friend"));
    }

    #[test]
    fn owner_code_consume_case_insensitive_and_single_use() {
        let now = crate::chrono_lite::unix_secs();
        let mut bot = bot_with_code("AbCdEfG2", now + 1800);
        // 小写输入也能匹配（用户手输大小写不一致很常见）
        assert_eq!(
            consume_owner_code_on_bot(&mut bot, "abcdefg2", "ou_a", "李四", now),
            OwnerCodeResult::Granted
        );
        // 二次消费同码 → NotFound（已删）
        assert_eq!(
            consume_owner_code_on_bot(&mut bot, "abcdefg2", "ou_b", "李四", now),
            OwnerCodeResult::NotFound
        );
        assert!(
            !is_owner_allowed(&bot.owner_open_id, "ou_b"),
            "重复码不重复授权"
        );
    }

    #[test]
    fn owner_code_consume_expired() {
        let now = crate::chrono_lite::unix_secs();
        let mut bot = bot_with_code("EXPIRED1", now - 1); // 已过期
        let r = consume_owner_code_on_bot(&mut bot, "EXPIRED1", "ou_late", "", now);
        assert_eq!(r, OwnerCodeResult::Expired);
        // 过期码被删（一次性），owner 不变
        assert!(bot.pending_codes.is_empty());
        assert!(!is_owner_allowed(&bot.owner_open_id, "ou_late"));
    }

    #[test]
    fn owner_code_consume_not_found_keeps_pending() {
        let now = crate::chrono_lite::unix_secs();
        let mut bot = bot_with_code("K3M8XQ2P", now + 1800);
        let r = consume_owner_code_on_bot(&mut bot, "WRONG000", "ou_x", "", now);
        assert_eq!(r, OwnerCodeResult::NotFound);
        assert_eq!(bot.pending_codes.len(), 1, "未匹配不应删码");
        assert!(!is_owner_allowed(&bot.owner_open_id, "ou_x"));
        // 空串 / 纯空白同样 NotFound
        assert_eq!(
            consume_owner_code_on_bot(&mut bot, "   ", "ou_x", "", now),
            OwnerCodeResult::NotFound
        );
        assert_eq!(bot.pending_codes.len(), 1);
    }

    #[test]
    fn owner_code_first_grant_adds_granted_only() {
        // owner 空 + 授权码 → 发送者成为「授权者」，owner（管理员）保持空（需 GUI 手填）
        let now = crate::chrono_lite::unix_secs();
        let mut bot = bot_with_code("FIRST01", now + 1800);
        bot.owner_open_id.clear();
        assert_eq!(
            consume_owner_code_on_bot(&mut bot, "FIRST01", "ou_me", "", now),
            OwnerCodeResult::Granted
        );
        assert!(bot.owner_open_id.is_empty(), "授权码不改 owner");
        assert!(is_owner_allowed(&bot.granted_ids, "ou_me"));
        assert!(bot.access_allows("ou_me"), "私有模式下授权者可对话");
    }

    #[test]
    fn owner_code_admin_role_adds_owner() {
        // 管理员码（role=owner）→ 发送者成为 owner（管理员），不进授权者列表
        let now = crate::chrono_lite::unix_secs();
        let mut bot = BotConfig {
            pending_codes: vec![OwnerCode {
                code: "ADMIN1".into(),
                expires_at: now + 1800,
                role: "owner".into(),
            }],
            ..Default::default()
        };
        assert_eq!(
            consume_owner_code_on_bot(&mut bot, "admin1", "ou_admin", "李总", now),
            OwnerCodeResult::Granted
        );
        assert!(is_owner_allowed(&bot.owner_open_id, "ou_admin"));
        assert!(bot.granted_ids.is_empty(), "管理员码不进授权者");
        assert_eq!(bot.owner_infos[0].name, "李总");
        assert!(bot.access_allows("ou_admin"));
    }

    #[test]
    fn owner_code_admin_and_granted_coexist() {
        // 两类码可同时存在（生成时不互相作废）；消费各落各的位
        let now = crate::chrono_lite::unix_secs();
        let mut bot = BotConfig {
            pending_codes: vec![
                OwnerCode {
                    code: "ADMIN2".into(),
                    expires_at: now + 1800,
                    role: "owner".into(),
                },
                OwnerCode {
                    code: "GRANT2".into(),
                    expires_at: now + 1800,
                    role: "granted".into(),
                },
            ],
            ..Default::default()
        };
        // 消费普通码：只落授权者；管理员码保留
        assert_eq!(
            consume_owner_code_on_bot(&mut bot, "GRANT2", "ou_u", "小张", now),
            OwnerCodeResult::Granted
        );
        assert!(is_owner_allowed(&bot.granted_ids, "ou_u"));
        assert!(bot.owner_open_id.is_empty());
        assert_eq!(bot.pending_codes.len(), 1, "管理员码未被消费");
        assert_eq!(bot.pending_codes[0].role, "owner");
        // 再消费管理员码
        assert_eq!(
            consume_owner_code_on_bot(&mut bot, "admin2", "ou_admin", "李总", now),
            OwnerCodeResult::Granted
        );
        assert!(is_owner_allowed(&bot.owner_open_id, "ou_admin"));
        assert!(bot.pending_codes.is_empty());
    }

    #[test]
    fn owner_code_consume_cleans_expired_residue() {
        let now = crate::chrono_lite::unix_secs();
        let mut bot = bot_with_code("NEWCODE1", now + 1800);
        bot.pending_codes.push(OwnerCode {
            code: "OLDSTALE".into(),
            expires_at: now - 10, // 过期残留
            role: "granted".into(),
        });
        assert_eq!(
            consume_owner_code_on_bot(&mut bot, "NEWCODE1", "ou_f", "王五", now),
            OwnerCodeResult::Granted
        );
        // 消费新码时顺带清掉过期残留，pending 不无限增长
        assert!(bot.pending_codes.is_empty());
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
        assert!(
            !s.contains("default_provider"),
            "空 default_provider 不应序列化"
        );
    }

    #[test]
    fn cross_delivery_enabled_defaults_off_and_roundtrips() {
        // 默认关闭：新功能不应改变旧行为
        let c = Config::default();
        assert!(!c.cross_delivery_enabled);
        // 旧 config 无该字段 → 反序列化为 false，不报错
        let text = r#"{"owner_open_id":"o","default_backend":"claude","bots":[]}"#;
        let c2: Config = serde_json::from_str(text).unwrap();
        assert!(!c2.cross_delivery_enabled);
        // 显式打开 → 序列化/反序列化往返不丢
        let c3 = Config {
            cross_delivery_enabled: true,
            ..Default::default()
        };
        let s = serde_json::to_string(&c3).unwrap();
        assert!(s.contains("\"cross_delivery_enabled\":true"));
        let back: Config = serde_json::from_str(&s).unwrap();
        assert!(back.cross_delivery_enabled);
    }

    #[test]
    fn draft_roundtrip_and_newer_check() {
        // 草稿读写 + mtime 判定（写完即删，避免污染真实草稿）
        let mut c = Config::default();
        c.bots.push(BotConfig {
            name: "draft-test".into(),
            kind: "feishu".into(),
            ..Default::default()
        });
        c.save_draft().unwrap();
        assert!(Config::draft_path().exists(), "草稿应已落盘");
        assert!(
            Config::draft_is_newer(),
            "刚写的草稿应比正式配置新（或正式配置不存在）"
        );
        let loaded = Config::load_draft().expect("草稿应能读回");
        assert_eq!(loaded.bots.len(), 1);
        assert_eq!(loaded.bots[0].name, "draft-test");
        Config::remove_draft();
        assert!(!Config::draft_path().exists(), "删除后草稿不应存在");
        assert!(Config::load_draft().is_none());
        assert!(!Config::draft_is_newer());
    }

    #[test]
    fn mention_modes_roundtrip_and_old_config_compat() {
        // 新格式：mention_modes 序列化往返（含 per-群隔离的两把钥匙）
        let mut c = Config::default();
        c.bots.push(BotConfig {
            name: "mm-test".into(),
            kind: "feishu".into(),
            mention_modes: [
                ("oc_a".to_string(), "off".to_string()),
                ("oc_b".to_string(), "on".to_string()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        });
        let s = serde_json::to_string(&c).unwrap();
        let back: Config = serde_json::from_str(&s).unwrap();
        assert_eq!(
            back.bots[0].mention_modes.get("oc_a").map(String::as_str),
            Some("off")
        );
        assert_eq!(
            back.bots[0].mention_modes.get("oc_b").map(String::as_str),
            Some("on")
        );
        // 空 map 不落盘（skip_serializing_if）
        let empty = Config::default();
        let s2 = serde_json::to_string(&empty).unwrap();
        assert!(!s2.contains("mention_modes"), "空 map 不序列化");
        // 旧 config 无该字段 → 反序列化按空 map（缺省 = 需要 @）
        let old = r#"{"bots":[{"name":"legacy","kind":"feishu"}]}"#;
        let back_old: Config = serde_json::from_str(old).unwrap();
        assert!(back_old.bots[0].mention_modes.is_empty(), "旧文件兼容缺省");
    }
}
