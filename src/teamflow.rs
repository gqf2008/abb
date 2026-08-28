//! 一键创建团队·聊天入口（#124 P1 后端）——触发词识别 + 会话态 + 预览/清单渲染。
//!
//! 定位：用户在聊天里发创建意图 → 方案预览 → 确认/修改/取消 → 建群开聊。
//! 本模块是纯逻辑层（可单测，不碰 Bridge/网络）：触发词解析、会话状态机与持久化、
//! 预览/结果文案渲染；桥侧路由与 LLM 生成/建群调用在 `bridge::teamflow`。
//!
//! 边界（后端定，issue #124 自行澄清，不等回复）：
//! - 触发词表：动词「建/创建/组建/新建」+ 名词「团队/项目组/小组」，名词须在动词之后；
//!   「项目」单独不触发（"创建项目计划" 是文档意图不是团队意图，误伤率高）。
//! - 会话按（bot, chat key）持久化 `workspaces/<bot>/teamflow.json`（复用 pending 落盘
//!   模式：原子写 + 重启保留）；24h 无操作自动过期清理。

use crate::teambuilder::TeamPlan;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

/// 触发动词。
const TRIGGER_VERBS: &[&str] = &["建", "创建", "组建", "新建"];
/// 触发名词（「项目」单独不触发——"创建项目计划" 是文档意图，误伤率高）。
const TRIGGER_NOUNS: &[&str] = &["团队", "项目组", "小组"];
/// 触发命中后，从消息里剔除的语气/引导词（用于判断「是否有实质目标描述」）。
const GOAL_FILLERS: &[&str] = &[
    "个", "一个", "帮我", "帮", "我们", "来", "去", "做", "：", ":", "，", ",", " ", "的",
];
/// 会话 TTL：24h 无操作自动过期（清理磁盘与内存，防 stale 状态无限堆积）。
pub const FLOW_TTL_SECS: u64 = 24 * 3600;

/// 识别聊天消息里的团队创建意图。
/// - 命中：返回目标描述。`Some("")` = 触发但缺目标（调用方追问目标）；
///   `Some(非空)` = 目标即整条消息（LLM 从目标描述里提取真实意图，鲁棒性最好——
///   "组建研发团队" 的目标"研发"在名词之前，拆词容易误伤，整句交给模型最稳）。
/// - 未命中：返回 None（原样透传 agent，不打断普通对话）。
pub fn parse_team_intent(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    // 动词取最先出现的位置
    let verb_pos = TRIGGER_VERBS.iter().filter_map(|v| t.find(v)).min()?;
    // 名词必须出现在动词之后（"我们团队想建个文档" 的「团队」在动词前，不算触发）
    let after_verb = &t[verb_pos..];
    if !TRIGGER_NOUNS.iter().any(|n| after_verb.contains(n)) {
        return None;
    }
    // 剔除触发词与语气词后，剩余不足 2 字 = 只有触发词没有目标（如"建团队"）→ 空目标
    let mut rest = t.to_string();
    for v in TRIGGER_VERBS {
        rest = rest.replace(v, "");
    }
    for n in TRIGGER_NOUNS {
        rest = rest.replace(n, "");
    }
    for f in GOAL_FILLERS {
        rest = rest.replace(f, "");
    }
    if rest.trim().chars().count() < 2 {
        Some(String::new())
    } else {
        Some(t.to_string())
    }
}

/// 团队创建会话状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TeamFlowState {
    /// 触发但缺目标：等待用户补充团队目标。
    WaitingGoal,
    /// 已生成方案预览：等待 确认 / 改：xxx / 取消。
    WaitingConfirm { goal: String, plan: TeamPlan },
}

/// 一个（bot, chat key）的团队创建会话。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamFlow {
    pub state: TeamFlowState,
    pub updated_at: u64,
}

/// 会话存取：`workspaces/<bot>/teamflow.json`，原子写（复用 pending 落盘模式）。
pub struct TeamFlowStore {
    path: PathBuf,
    data: Mutex<HashMap<String, TeamFlow>>,
}

impl TeamFlowStore {
    pub fn new(bot_key: &str) -> TeamFlowStore {
        let dir = crate::workspace_dir(bot_key);
        let _ = fs::create_dir_all(&dir);
        Self::at(dir.join("teamflow.json"))
    }

    fn at(path: PathBuf) -> TeamFlowStore {
        let data = match fs::read_to_string(&path) {
            Ok(t) => serde_json::from_str::<HashMap<String, TeamFlow>>(&t).unwrap_or_default(),
            Err(_) => HashMap::new(),
        };
        TeamFlowStore {
            path,
            data: Mutex::new(data),
        }
    }

    /// 读（顺带清理过期会话；清到条目时落盘）。
    pub fn get(&self, key: &str) -> Option<TeamFlow> {
        let mut data = self.data.lock().unwrap();
        let now = crate::chrono_lite::unix_secs();
        let expired: Vec<String> = data
            .iter()
            .filter(|(_, f)| now.saturating_sub(f.updated_at) > FLOW_TTL_SECS)
            .map(|(k, _)| k.clone())
            .collect();
        let mut dirty = false;
        for k in expired {
            data.remove(&k);
            dirty = true;
        }
        if dirty {
            self.save(&data);
        }
        data.get(key).cloned()
    }

    pub fn set(&self, key: &str, flow: TeamFlow) {
        let mut data = self.data.lock().unwrap();
        data.insert(key.to_string(), flow);
        self.save(&data);
    }

    pub fn remove(&self, key: &str) {
        let mut data = self.data.lock().unwrap();
        if data.remove(key).is_some() {
            self.save(&data);
        }
    }

    fn save(&self, data: &HashMap<String, TeamFlow>) {
        match serde_json::to_string_pretty(data) {
            Ok(text) => {
                if let Err(e) = crate::atomic_write_text(&self.path, &text) {
                    crate::log!("[teamflow] ⚠️ 写盘失败 path={}: {e:#}", self.path.display());
                }
            }
            Err(e) => crate::log!("[teamflow] ⚠️ 序列化失败: {e:#}"),
        }
    }
}

/// 方案生成抽象（仿 `agent::AgentRunner` 的测试可测性设计）：生产用
/// [`RealTeamPlanGenerator`] 转发 teambuilder（spawn 本机 claude/codex/pi，120s 超时）；
/// 测试注入挡板返回固定方案/错误，驱动聊天流程状态机，不碰真实 LLM。
#[async_trait::async_trait]
pub trait TeamPlanGenerator: Send + Sync {
    async fn generate(
        &self,
        backend: crate::agent::Backend,
        goal: &str,
        members: &[String],
        template: Option<&str>,
    ) -> Result<TeamPlan, String>;
}

/// 生产实现：转发 teambuilder 生成链路（LLM 现场生成 + schema 校验）。
pub struct RealTeamPlanGenerator;

#[async_trait::async_trait]
impl TeamPlanGenerator for RealTeamPlanGenerator {
    async fn generate(
        &self,
        backend: crate::agent::Backend,
        goal: &str,
        members: &[String],
        template: Option<&str>,
    ) -> Result<TeamPlan, String> {
        crate::teambuilder::generate_team_plan(backend, goal, members, template).await
    }
}

/// #142：角色名 → emoji 稳定映射（顺序无关、关键词匹配；未知回退 ⚙️）。
/// 修正原 `i % len` 下标循环：角色顺序变化不再错位，同一角色跨团队显示一致。
fn emoji_for_role(role_name: &str) -> &'static str {
    // 按特异性排序：含「测试/QA」的角色名（如「测试工程师」）优先命中 🧪，
    // 避免被更宽泛的「工程/开发」抢先映射成 💻。
    const MAP: [(&[&str], &str); 7] = [
        (&["产品"], "👤"),
        (&["ui", "设计", "ux", "交互"], "🎨"),
        (&["测试", "qa", "质量"], "🧪"),
        (&["后端", "前端", "编程", "开发", "工程", "技术"], "💻"),
        (&["运营"], "📣"),
        (&["市场", "增长", "营销"], "📈"),
        (&["运维", "基础设施", "部署"], "🛠️"),
    ];
    let lower = role_name.to_lowercase();
    for (keys, emoji) in MAP {
        if keys.iter().any(|k| lower.contains(k)) {
            return emoji;
        }
    }
    "⚙️"
}

/// #162 方案 A：角色全名 = 项目-角色-姓名（member 空 = 待任命）。
/// LLM 生成的 role_name 保持纯角色名（TeamRole 字段不动），**所有使用处**（预览/
/// 建群/登记/@寻址）统一经此拼装——用户「改：」调整 member 后重新拼装仍一致；
/// emoji_for_role 仍按纯角色名匹配（完整名 contains 也能命中，纯名更稳）。
pub fn full_role_name(team_name: &str, role_name: &str, member: Option<&str>) -> String {
    // filter 空串：LLM 可能输出空 member_name（"" vs null），统一按待任命处理
    format!(
        "{team_name}-{role_name}-{}",
        member.filter(|s| !s.trim().is_empty()).unwrap_or("待任命")
    )
}

/// 渲染方案预览（#124 2.3 纯文本格式，三端通用；UI 定稿的推荐基线）。
pub fn render_preview(plan: &TeamPlan) -> String {
    let mut out = String::new();
    out.push_str(&format!("📋 团队「{}」方案预览\n", plan.team_name));
    out.push_str("━━━━━━━━━━━━━━\n");
    for role in plan.roles.iter() {
        // #162：role_name 显示完整三段（项目-角色-姓名），不再另加（member）括号
        //（三段已含姓名，避免「斯蒂芬（斯蒂芬）」重复）。
        let full = full_role_name(
            &plan.team_name,
            &role.role_name,
            role.member_name.as_deref(),
        );
        out.push_str(&format!(
            "{} {} — {}\n",
            emoji_for_role(&role.role_name),
            full,
            role.system_prompt
        ));
    }
    out.push_str("━━━━━━━━━━━━━━\n");
    if let Some(c) = plan.collab.as_deref().filter(|c| !c.trim().is_empty()) {
        out.push_str(&format!("协作流程：{c}\n"));
        out.push_str("━━━━━━━━━━━━━━\n");
    }
    out.push_str("✅ 回复「确认」创建团队\n");
    out.push_str("✏️ 回复「改：把测试改成运营」调整方案\n");
    out.push_str("❌ 回复「取消」放弃\n");
    out
}

/// 单个角色的建群结果。
#[derive(Debug, Clone)]
pub struct CreateOutcome {
    pub role_name: String,
    pub member: String,
    pub ok: bool,
    pub detail: String,
}

/// 按方案逐角色建群 + 登记虚拟 Bot（#75 登记表）。
/// 幂等：已登记同角色名的群跳过（部分失败后「确认」重试不会重复建已成功的群）。
/// `store`：登记表（桥侧传注入的 vb_store，测试隔离；生产即全局 virtual-bots.json）。
/// `owner`：平台侧群主/管理员 id（飞书必需：把 owner 拉进群 + 设为管理员）。
pub async fn create_team_groups(
    msgr: &dyn crate::messenger::Messenger,
    store: &crate::virtualbot::VirtualBotStore,
    bot_key: &str,
    owner: &str,
    plan: &TeamPlan,
) -> Vec<CreateOutcome> {
    let existing: Vec<String> = store
        .load_for(bot_key)
        .iter()
        .map(|v| v.role_name.clone())
        .collect();
    let mut out = Vec::new();
    for role in &plan.roles {
        // #162：群名/@寻址/幂等检查统一用完整三段名（项目-角色-姓名）；member 空 =
        // 待任命。LLM 生成的纯 role_name 仅作 emoji 匹配等内部用途。
        let full_name = full_role_name(
            &plan.team_name,
            &role.role_name,
            role.member_name.as_deref(),
        );
        let member = role
            .member_name
            .clone()
            .unwrap_or_else(|| "待任命".to_string());
        // #162 幂等兼容：登记表既可能存三段全名（新团队）也可能存纯角色名（#162 前
        // 旧团队，不迁移）——两者都命中即跳过，旧团队「确认」重试不会重复建群。
        if existing.contains(&full_name) || existing.contains(&role.role_name) {
            out.push(CreateOutcome {
                role_name: full_name.clone(),
                member,
                ok: true,
                detail: format!("已存在，跳过（可 @{} 对话）", full_name),
            });
            continue;
        }
        match msgr
            .create_chat(&full_name, &role.system_prompt, owner)
            .await
        {
            Ok(chat_id) => {
                let reg = crate::virtualbot::VirtualBot {
                    bot_key: bot_key.to_string(),
                    chat_id,
                    role_name: full_name.clone(),
                    created_at: crate::chrono_lite::unix_secs(),
                };
                match store.add(reg) {
                    Ok(()) => out.push(CreateOutcome {
                        role_name: full_name.clone(),
                        member,
                        ok: true,
                        detail: format!("已建，可 @{} 对话", full_name),
                    }),
                    Err(e) => out.push(CreateOutcome {
                        role_name: full_name,
                        member,
                        ok: false,
                        detail: format!("群已创建但登记失败：{e}"),
                    }),
                }
            }
            Err(e) => out.push(CreateOutcome {
                role_name: full_name,
                member,
                ok: false,
                detail: e,
            }),
        }
    }
    out
}

/// 渲染创建清单（成功 / 部分失败 / 全部失败 三种形态分开展示，#124 2.5/2.6）。
pub fn render_create_result(plan: &TeamPlan, outcomes: &[CreateOutcome]) -> String {
    let ok = outcomes.iter().filter(|o| o.ok).count();
    let mut out = String::new();
    if ok == outcomes.len() {
        out.push_str(&format!(
            "✅ 团队「{}」已创建（{} 个角色群）\n",
            plan.team_name,
            outcomes.len()
        ));
    } else if ok == 0 {
        out.push_str(&format!("❌ 团队「{}」创建失败：\n", plan.team_name));
    } else {
        out.push_str(&format!(
            "⚠️ 团队「{}」部分成功（{}/{}）：\n",
            plan.team_name,
            ok,
            outcomes.len()
        ));
    }
    for o in outcomes {
        let mark = if o.ok { "✅" } else { "❌" };
        // #162：role_name 已是完整三段（含姓名），不再另加（member）括号。
        out.push_str(&format!("  {} {} → {}\n", mark, o.role_name, o.detail));
    }
    if ok != outcomes.len() {
        out.push_str("回复「确认」可重试未成功的角色（已成功的不会重复建）。");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::teambuilder::TeamRole;

    #[test]
    fn emoji_mapping_is_order_independent_and_stable() {
        // #142：同一角色名在任何顺序下都映射同一 emoji；关键词命中；未知回退 ⚙️
        assert_eq!(emoji_for_role("产品经理"), "👤");
        assert_eq!(emoji_for_role("UI/UX 设计"), "🎨");
        assert_eq!(emoji_for_role("交互设计师"), "🎨");
        assert_eq!(emoji_for_role("后端工程师"), "💻");
        assert_eq!(emoji_for_role("前端开发"), "💻");
        assert_eq!(emoji_for_role("测试工程师"), "🧪");
        assert_eq!(emoji_for_role("QA"), "🧪");
        assert_eq!(emoji_for_role("运营专员"), "📣");
        assert_eq!(emoji_for_role("市场增长"), "📈");
        assert_eq!(emoji_for_role("运维"), "🛠️");
        assert_eq!(emoji_for_role("法务顾问"), "⚙️", "未知角色回退");
        assert_eq!(
            emoji_for_role("产品经理"),
            emoji_for_role("产品经理"),
            "稳定"
        );
    }

    #[test]
    fn full_role_name_joins_three_segments() {
        // #162 方案 A：项目-角色-姓名；member 空 → 待任命（三段不省略）
        assert_eq!(
            full_role_name("xxx项目", "前端工程师", Some("斯蒂芬")),
            "xxx项目-前端工程师-斯蒂芬"
        );
        assert_eq!(
            full_role_name("xxx项目", "前端工程师", None),
            "xxx项目-前端工程师-待任命"
        );
        assert_eq!(
            full_role_name("xxx项目", "前端工程师", Some("")),
            "xxx项目-前端工程师-待任命",
            "空串 member 按待任命处理"
        );
        // 纯角色名保持原样（emoji 匹配等内部用途依赖它）
        assert_eq!(
            full_role_name("T", "产品经理", Some("小王")),
            "T-产品经理-小王"
        );
    }

    #[test]
    fn render_preview_uses_keyword_emoji() {
        // #142：预览行 emoji 与角色名匹配（首个角色是 UI 时不再拿到 👤）
        let plan = crate::teambuilder::TeamPlan {
            team_name: "x".into(),
            roles: vec![
                TeamRole {
                    role_name: "UI/UX 设计".into(),
                    member_name: None,
                    system_prompt: "负责界面".into(),
                },
                TeamRole {
                    role_name: "产品经理".into(),
                    member_name: None,
                    system_prompt: "负责需求".into(),
                },
            ],
            collab: None,
        };
        let out = render_preview(&plan);
        assert!(
            out.contains("🎨 x-UI/UX 设计-待任命"),
            "UI 角色应映射 🎨（即使排第一个）: {out}"
        );
        assert!(out.contains("👤 x-产品经理-待任命"), "产品应映射 👤: {out}");
    }

    #[test]
    fn intent_hits_with_goal() {
        for t in [
            "帮我建个团队做记账App",
            "帮我创建一个研发团队",
            "组建一个自媒体团队来运营",
            "帮我们新建个项目组做数据分析",
            "建个项目小组搞运营",
        ] {
            let g = parse_team_intent(t);
            assert!(
                matches!(g, Some(s) if !s.is_empty()),
                "{t:?} 应命中且有目标"
            );
        }
    }

    #[test]
    fn intent_hit_without_goal_returns_empty() {
        assert_eq!(parse_team_intent("创建团队"), Some(String::new()));
        assert_eq!(parse_team_intent(" 建团队 "), Some(String::new()));
        assert_eq!(parse_team_intent("组建个项目组"), Some(String::new()));
    }

    #[test]
    fn intent_misses_pass_through() {
        // 无动词
        assert!(parse_team_intent("这个项目什么时候上线").is_none());
        // 动词后无触发名词（"创建项目计划" 是文档意图）
        assert!(parse_team_intent("帮我创建项目计划").is_none());
        // 名词在动词之前（叙述句，不是创建意图）
        assert!(parse_team_intent("我们团队想建个文档").is_none());
        // 空消息
        assert!(parse_team_intent("").is_none());
        assert!(parse_team_intent("   ").is_none());
        // 普通闲聊
        assert!(parse_team_intent("今天天气不错").is_none());
    }

    #[test]
    fn store_roundtrip_and_remove() {
        let p =
            std::env::temp_dir().join(format!("abb-teamflow-test-{}.json", uuid::Uuid::new_v4()));
        let store = TeamFlowStore::at(p.clone());
        assert!(store.get("oc_a").is_none());
        store.set(
            "oc_a",
            TeamFlow {
                state: TeamFlowState::WaitingGoal,
                updated_at: crate::chrono_lite::unix_secs(),
            },
        );
        let reloaded = TeamFlowStore::at(p.clone());
        assert!(matches!(
            reloaded.get("oc_a"),
            Some(TeamFlow {
                state: TeamFlowState::WaitingGoal,
                ..
            })
        ));
        reloaded.remove("oc_a");
        assert!(TeamFlowStore::at(p.clone()).get("oc_a").is_none());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn store_expires_stale_flows() {
        let p =
            std::env::temp_dir().join(format!("abb-teamflow-ttl-{}.json", uuid::Uuid::new_v4()));
        let store = TeamFlowStore::at(p.clone());
        store.set(
            "oc_stale",
            TeamFlow {
                state: TeamFlowState::WaitingGoal,
                updated_at: crate::chrono_lite::unix_secs() - FLOW_TTL_SECS - 1,
            },
        );
        assert!(store.get("oc_stale").is_none(), "过期会话应被清理");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn preview_contains_required_markers() {
        let plan = TeamPlan {
            team_name: "记账 App 研发组".into(),
            roles: vec![
                TeamRole {
                    role_name: "产品经理".into(),
                    member_name: Some("小王".into()),
                    system_prompt: "负责需求分析与排期".into(),
                },
                TeamRole {
                    role_name: "UI/UX".into(),
                    member_name: None,
                    system_prompt: "负责界面与交互设计".into(),
                },
            ],
            collab: Some("产品 → UI/UX 循环".into()),
        };
        let s = render_preview(&plan);
        assert!(s.contains("📋 团队「记账 App 研发组」方案预览"));
        // #162：预览角色行 = 完整三段（项目-角色-姓名），无（member）括号避免重复
        assert!(s.contains("记账 App 研发组-产品经理-小王 — 负责需求分析与排期"));
        assert!(s.contains("记账 App 研发组-UI/UX-待任命 — 负责界面与交互设计"));
        assert!(
            !s.contains("（小王）"),
            "三段名已含姓名，不应再有成员括号: {s}"
        );
        assert!(s.contains("✅ 回复「确认」创建团队"));
        assert!(s.contains("✏️ 回复「改："));
        assert!(s.contains("❌ 回复「取消」放弃"));
    }

    #[test]
    fn create_result_renders_partial_failure() {
        let plan = TeamPlan {
            team_name: "T".into(),
            roles: vec![],
            collab: None,
        };
        let outcomes = vec![
            CreateOutcome {
                role_name: "开发".into(),
                member: "待任命".into(),
                ok: true,
                detail: "已建".into(),
            },
            CreateOutcome {
                role_name: "测试".into(),
                member: "待任命".into(),
                ok: false,
                detail: "建群失败：模拟".into(),
            },
        ];
        let s = render_create_result(&plan, &outcomes);
        assert!(s.contains("部分成功（1/2）"));
        // #162：role_name 已是三段，结果行不再带（member）括号
        assert!(s.contains("✅ 开发 → 已建"));
        assert!(s.contains("❌ 测试 → 建群失败：模拟"));
        assert!(s.contains("回复「确认」可重试"));
    }
}
