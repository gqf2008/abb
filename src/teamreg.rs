//! 团队登记表（#147）：GUI / 聊天入口创建的团队统一登记，团队列表、任命成员、
//! 解散团队的数据源。
//!
//! 与 virtual-bots.json 的关系：虚拟 Bot 登记表是「群」维度（一个 bot 下所有角色群
//! 平铺，寻址/注入的事实源）；本表是「团队」维度（按团队分组 + 成员任命 + 职责摘要）。
//! 双向一致约束（#124 §3.3）：建群仍只写 virtual-bots.json（不双写群数据），本表只追加
//! 团队分组与成员/职责元数据；解散团队 = 逐个移除虚拟 Bot 登记 + 归档聊天历史 + 删团队条目。
//! 本模块是纯文件存取 + 纯逻辑（可单测，不碰网络）。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 团队内一个角色的登记。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamRoleReg {
    /// 角色名（= 平台群名，与 virtual-bots.json 的 role_name 对应）。
    pub role_name: String,
    /// 成员名；"" = 待任命（任命成员 = 写这里；清空 = 恢复待任命）。
    pub member: String,
    /// 角色群 chat_id；"" = 该角色建群失败（团队状态 = 部分失败）。
    pub chat_id: String,
    /// 职责摘要（生成方案的 system_prompt 截断，团队卡片角色行展示用）。
    pub duty: String,
}

/// 一个团队。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamReg {
    pub bot_key: String,
    pub team_name: String,
    pub created_at: u64,
    pub roles: Vec<TeamRoleReg>,
}

impl TeamReg {
    /// 团队状态：全角色都有群 = 运行中；有角色缺群（建群失败/未建） = 部分失败。
    /// 空角色列表按「部分失败」处理（异常态，界面兜底）。
    pub fn running(&self) -> bool {
        !self.roles.is_empty() && self.roles.iter().all(|r| !r.chat_id.is_empty())
    }
}

/// 团队登记表存取：`<bridge_dir>/teams.json`（与 virtual-bots.json 并列）。
/// 每次操作读盘-改-原子写（与 VirtualBotStore 同款并发模型：单进程写、多进程读热刷新）。
pub struct TeamStore {
    path: PathBuf,
}

impl TeamStore {
    pub fn new() -> TeamStore {
        TeamStore::new_at(crate::bridge_dir().join("teams.json"))
    }

    /// 测试/自建路径注入（单测用临时目录，不碰真实登记表）。
    pub fn new_at(path: PathBuf) -> TeamStore {
        TeamStore { path }
    }

    /// 读全部团队。文件缺失/损坏 → 空列表（不 panic，损坏时日志留痕）。
    pub fn load(&self) -> Vec<TeamReg> {
        match std::fs::read_to_string(&self.path) {
            Ok(t) => match serde_json::from_str(&t) {
                Ok(v) => v,
                Err(e) => {
                    crate::log!("[teamreg] 团队登记表解析失败（按空处理）: {e:#}");
                    Vec::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                crate::log!("[teamreg] 读团队登记表失败: {e:#}");
                Vec::new()
            }
        }
    }

    /// 某 bot 的团队（团队列表按 bot 过滤展示）。
    pub fn load_for(&self, bot_key: &str) -> Vec<TeamReg> {
        self.load()
            .into_iter()
            .filter(|t| t.bot_key == bot_key)
            .collect()
    }

    /// 查某个团队。
    pub fn find(&self, bot_key: &str, team_name: &str) -> Option<TeamReg> {
        self.load()
            .into_iter()
            .find(|t| t.bot_key == bot_key && t.team_name == team_name)
    }

    /// 新增团队。Err：同 bot 同团队名已存在（团队名不唯一会歧义，拒绝覆盖）。
    /// 生产建群统一走 [`TeamStore::register_created`]（幂等合并）；本方法供严格新增场景。
    #[allow(dead_code)]
    pub fn add(&self, team: TeamReg) -> Result<(), String> {
        let mut cur = self.load();
        if cur
            .iter()
            .any(|t| t.bot_key == team.bot_key && t.team_name == team.team_name)
        {
            return Err(format!("团队「{}」已存在", team.team_name));
        }
        cur.push(team);
        self.write(&cur)
    }

    /// 建群完成后登记/更新团队（幂等合并）：
    /// - 同 (bot, 团队名) 已存在（聊天确认重试 / GUI 再次创建）→ 按角色名合并：
    ///   已有角色保留（含成员任命），只补空缺 chat_id 与职责；新角色追加；
    /// - 不存在 → 新建（created_at 用当前时间）。
    ///
    /// 返回 Err 仅当写盘失败（登记失败不阻断建群结果，调用方日志留痕即可）。
    pub fn register_created(
        &self,
        bot_key: &str,
        team_name: &str,
        roles: Vec<TeamRoleReg>,
    ) -> Result<(), String> {
        let mut cur = self.load();
        if let Some(existing) = cur
            .iter_mut()
            .find(|t| t.bot_key == bot_key && t.team_name == team_name)
        {
            for r in roles {
                if let Some(e) = existing
                    .roles
                    .iter_mut()
                    .find(|e| e.role_name == r.role_name)
                {
                    // 只补空缺：重试建群成功的角色补 chat_id；职责/成员保留已有
                    if e.chat_id.is_empty() && !r.chat_id.is_empty() {
                        e.chat_id = r.chat_id;
                    }
                    if e.duty.is_empty() {
                        e.duty = r.duty;
                    }
                } else {
                    existing.roles.push(r);
                }
            }
            self.write(&cur)
        } else {
            cur.push(TeamReg {
                bot_key: bot_key.to_string(),
                team_name: team_name.to_string(),
                created_at: crate::chrono_lite::unix_secs(),
                roles,
            });
            self.write(&cur)
        }
    }

    /// 删除团队条目（解散团队：调用方还需逐个移除虚拟 Bot 登记 + 归档历史）。
    /// 返回 true=确有删除。
    pub fn remove(&self, bot_key: &str, team_name: &str) -> bool {
        let cur = self.load();
        let before = cur.len();
        let next: Vec<TeamReg> = cur
            .into_iter()
            .filter(|t| !(t.bot_key == bot_key && t.team_name == team_name))
            .collect();
        if next.len() == before {
            return false;
        }
        self.write(&next).is_ok()
    }

    /// 任命成员：member="" 恢复「待任命」。返回 Err：团队/角色不存在。
    pub fn set_member(
        &self,
        bot_key: &str,
        team_name: &str,
        role_name: &str,
        member: &str,
    ) -> Result<(), String> {
        let mut cur = self.load();
        let team = cur
            .iter_mut()
            .find(|t| t.bot_key == bot_key && t.team_name == team_name)
            .ok_or_else(|| "团队不存在（可能已解散）".to_string())?;
        let role = team
            .roles
            .iter_mut()
            .find(|r| r.role_name == role_name)
            .ok_or_else(|| format!("角色「{}」不存在", role_name))?;
        role.member = member.trim().to_string();
        self.write(&cur)
    }

    /// #147 双向一致：虚拟 Bot 登记被移除（取消登记 / 解散群 / 平台解散刷新发现）后，
    /// 同步清掉团队条目里对应角色的 chat_id（团队状态转「部分失败」）。返回清理的角色数。
    pub fn clear_chat(&self, bot_key: &str, chat_id: &str) -> usize {
        let mut cur = self.load();
        let mut cleared = 0;
        for t in cur.iter_mut().filter(|t| t.bot_key == bot_key) {
            for r in t.roles.iter_mut() {
                if r.chat_id == chat_id {
                    r.chat_id.clear();
                    cleared += 1;
                }
            }
        }
        if cleared > 0 {
            let _ = self.write(&cur);
        }
        cleared
    }

    fn write(&self, teams: &[TeamReg]) -> Result<(), String> {
        let text = serde_json::to_string_pretty(teams).map_err(|e| format!("序列化失败：{e}"))?;
        crate::atomic_write_text(&self.path, &text).map_err(|e| format!("写盘失败：{e:#}"))
    }
}

/// 职责摘要截断长度（团队卡片角色行展示，够看即可）。
const DUTY_MAX: usize = 60;

/// 职责摘要：system_prompt 去空白 + 截断（防卡片超长）。
pub fn truncate_duty(s: &str) -> String {
    let t = s.trim();
    if t.chars().count() <= DUTY_MAX {
        t.to_string()
    } else {
        let mut out: String = t.chars().take(DUTY_MAX).collect();
        out.push('…');
        out
    }
}

/// 建群完成后 → 团队角色登记（#147）：chat_id 从虚拟 Bot 登记表按角色名解析
/// （建群失败/未建 = ""，团队状态显示「部分失败」）；成员沿用方案（无 = 待任命）。
pub fn role_regs_from_plan(
    plan: &crate::teambuilder::TeamPlan,
    store: &crate::virtualbot::VirtualBotStore,
    bot_key: &str,
) -> Vec<TeamRoleReg> {
    plan.roles
        .iter()
        .map(|r| {
            // #162：角色登记/寻址统一用完整三段名（项目-角色-姓名）——登记表（建群时
            // create_team_groups 写入）与团队条目必须同口径，纯名 resolve 会 miss。
            let full_name = crate::teamflow::full_role_name(
                &plan.team_name,
                &r.role_name,
                r.member_name.as_deref(),
            );
            TeamRoleReg {
                role_name: full_name.clone(),
                member: r.member_name.clone().unwrap_or_default(),
                chat_id: store.resolve(bot_key, &full_name).unwrap_or_default(),
                duty: truncate_duty(&r.system_prompt),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::teambuilder::{TeamPlan, TeamRole};

    fn tmp_store() -> (TeamStore, std::path::PathBuf) {
        let p = std::env::temp_dir().join(format!("abb-teamreg-{}.json", uuid::Uuid::new_v4()));
        (TeamStore::new_at(p.clone()), p)
    }

    fn team(bot: &str, name: &str) -> TeamReg {
        TeamReg {
            bot_key: bot.into(),
            team_name: name.into(),
            created_at: 100,
            roles: vec![TeamRoleReg {
                role_name: "产品经理".into(),
                member: String::new(),
                chat_id: "oc_1".into(),
                duty: "负责需求".into(),
            }],
        }
    }

    #[test]
    fn store_roundtrip_and_filter() {
        let (s, p) = tmp_store();
        assert!(s.load().is_empty());
        s.add(team("bot_a", "记账组")).unwrap();
        s.add(team("bot_b", "运营组")).unwrap();
        assert_eq!(s.load().len(), 2);
        assert_eq!(s.load_for("bot_a").len(), 1);
        assert_eq!(s.load_for("bot_b")[0].team_name, "运营组");
        assert!(s.find("bot_a", "记账组").is_some());
        assert!(s.find("bot_a", "不存在").is_none());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn add_rejects_duplicate_team_name() {
        let (s, p) = tmp_store();
        s.add(team("bot_a", "记账组")).unwrap();
        assert!(
            s.add(team("bot_a", "记账组")).is_err(),
            "同 bot 同团队名应拒绝"
        );
        // 不同 bot 同名团队允许（跨 bot 独立）
        assert!(s.add(team("bot_b", "记账组")).is_ok());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn register_created_merges_roles_idempotently() {
        let (s, p) = tmp_store();
        // 首次：2 角色
        s.register_created(
            "bot_a",
            "记账组",
            vec![
                TeamRoleReg {
                    role_name: "产品经理".into(),
                    member: "小王".into(),
                    chat_id: "oc_1".into(),
                    duty: "负责需求".into(),
                },
                TeamRoleReg {
                    role_name: "UI".into(),
                    member: String::new(),
                    chat_id: String::new(), // 建群失败
                    duty: "负责界面".into(),
                },
            ],
        )
        .unwrap();
        // 重试（确认重发）：UI 建群成功 → 补 chat_id；产品经理保留成员任命
        s.register_created(
            "bot_a",
            "记账组",
            vec![
                TeamRoleReg {
                    role_name: "产品经理".into(),
                    member: String::new(),
                    chat_id: "oc_1".into(),
                    duty: "负责需求".into(),
                },
                TeamRoleReg {
                    role_name: "UI".into(),
                    member: String::new(),
                    chat_id: "oc_2".into(),
                    duty: "负责界面".into(),
                },
            ],
        )
        .unwrap();
        let t = s.find("bot_a", "记账组").unwrap();
        assert_eq!(t.roles.len(), 2, "合并不重复");
        let pm = t.roles.iter().find(|r| r.role_name == "产品经理").unwrap();
        assert_eq!(pm.member, "小王", "已有成员任命保留");
        assert_eq!(pm.chat_id, "oc_1");
        let ui = t.roles.iter().find(|r| r.role_name == "UI").unwrap();
        assert_eq!(ui.chat_id, "oc_2", "重试成功补 chat_id");
        // 新增角色追加
        s.register_created(
            "bot_a",
            "记账组",
            vec![TeamRoleReg {
                role_name: "测试".into(),
                member: String::new(),
                chat_id: "oc_3".into(),
                duty: "负责质量".into(),
            }],
        )
        .unwrap();
        assert_eq!(s.find("bot_a", "记账组").unwrap().roles.len(), 3);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn set_member_and_clear() {
        let (s, p) = tmp_store();
        s.add(team("bot_a", "记账组")).unwrap();
        s.set_member("bot_a", "记账组", "产品经理", " 李四 ")
            .unwrap();
        assert_eq!(
            s.find("bot_a", "记账组").unwrap().roles[0].member,
            "李四",
            "成员名 trim"
        );
        // 清空 = 待任命
        s.set_member("bot_a", "记账组", "产品经理", "").unwrap();
        assert_eq!(s.find("bot_a", "记账组").unwrap().roles[0].member, "");
        // 团队不存在 / 角色不存在 → Err
        assert!(s.set_member("bot_a", "不存在", "产品经理", "x").is_err());
        assert!(s.set_member("bot_a", "记账组", "不存在", "x").is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn remove_team() {
        let (s, p) = tmp_store();
        s.add(team("bot_a", "记账组")).unwrap();
        assert!(s.remove("bot_a", "记账组"));
        assert!(!s.remove("bot_a", "记账组"), "二次删除返回 false");
        assert!(s.load().is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn clear_chat_syncs_team_status() {
        let (s, p) = tmp_store();
        s.add(team("bot_a", "记账组")).unwrap();
        // 无关 chat_id 不影响
        assert_eq!(s.clear_chat("bot_a", "oc_other"), 0);
        assert_eq!(s.clear_chat("bot_b", "oc_1"), 0, "其它 bot 不受影响");
        // 命中：chat_id 清空，状态转「部分失败」
        assert_eq!(s.clear_chat("bot_a", "oc_1"), 1);
        let t = s.find("bot_a", "记账组").unwrap();
        assert!(t.roles[0].chat_id.is_empty());
        assert!(!t.running());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn running_status() {
        let mut t = team("bot_a", "记账组");
        assert!(t.running(), "全角色有群 = 运行中");
        t.roles[0].chat_id = String::new();
        assert!(!t.running(), "有角色缺群 = 部分失败");
        t.roles.clear();
        assert!(!t.running(), "空角色 = 部分失败（异常兜底）");
    }

    #[test]
    fn duty_truncation() {
        assert_eq!(truncate_duty("  负责需求分析  "), "负责需求分析");
        let long = "职".repeat(100);
        assert_eq!(
            truncate_duty(&long).chars().count(),
            DUTY_MAX + 1,
            "60 字 + 省略号"
        );
    }

    #[test]
    fn role_regs_from_plan_resolves_chat_ids() {
        let plan = TeamPlan {
            team_name: "T".into(),
            roles: vec![
                TeamRole {
                    role_name: "产品经理".into(),
                    member_name: Some("小王".into()),
                    system_prompt: "负责需求".into(),
                },
                TeamRole {
                    role_name: "UI".into(),
                    member_name: None,
                    system_prompt: "负责界面".into(),
                },
            ],
            collab: None,
        };
        let p = std::env::temp_dir().join(format!("abb-vb-{}.json", uuid::Uuid::new_v4()));
        let store = crate::virtualbot::VirtualBotStore::new_at(p.clone());
        // #162：登记表存完整三段名（建群时 create_team_groups 写入）——resolve 同口径
        store
            .add(crate::virtualbot::VirtualBot {
                bot_key: "bot_a".into(),
                chat_id: "oc_1".into(),
                role_name: "T-产品经理-小王".into(),
                created_at: 1,
            })
            .unwrap();
        let regs = role_regs_from_plan(&plan, &store, "bot_a");
        assert_eq!(regs.len(), 2);
        assert_eq!(regs[0].role_name, "T-产品经理-小王", "角色登记用三段全名");
        assert_eq!(regs[1].role_name, "T-UI-待任命", "无成员 = 待任命三段");
        assert_eq!(regs[0].chat_id, "oc_1", "已登记角色解析到 chat_id");
        assert_eq!(regs[0].member, "小王");
        assert!(regs[1].chat_id.is_empty(), "未登记角色 = 建群失败");
        assert_eq!(regs[1].member, "", "无成员 = 待任命");
        let _ = std::fs::remove_file(&p);
    }
}
