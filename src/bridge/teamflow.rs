//! Bridge 子模块：一键创建团队·聊天入口（#124 P1 后端）。
//! handle 在 pending 落盘之前调用 [`Bridge::team_chat_reply`]：命中团队流程 → 返回
//! 回复并短路（不落 pending、不进 agent）；未命中 → None 原样走 agent 主链路。
//! 子模块是父模块后代，可访问 mod.rs 私有字段（与 outbox/virtualbot 同款）。

use super::*;
use crate::teambuilder::TeamPlan;
use crate::teamflow::{render_create_result, render_preview, TeamFlow, TeamFlowState};
use std::sync::atomic::Ordering;

impl Bridge {
    /// 一键创建团队·聊天入口。返回 Some(回复) = 本消息已被团队流程消费（调用方发送后
    /// return）；None = 未命中团队流程，继续走 agent 主链路。
    /// 串行：生成/建群期间持 per-chat 串行锁（与 agent 路径同款，同 chat 消息排队等待）。
    pub(super) async fn team_chat_reply(&self, ev: &Ev, text: &str) -> Option<String> {
        // 仅 owner 可操作（建群是管理动作；与 /trash、/mention 开关同口径）
        if ev.role != crate::config::SenderRole::Owner {
            return None;
        }
        let key = ev.key();
        let intent = crate::teamflow::parse_team_intent(text);
        let has_flow = self.team_flows.get(&key).is_some();
        if !has_flow && intent.is_none() {
            return None; // 无流程且无触发词 → 放行
        }
        // 生成/建群期间同 chat 其它消息排队（复用 agent 路径的 per-chat 串行锁，
        // 与 #21/#27 消息路由互斥，避免两个流程交错）
        let lock = self.chat_lock(&key);
        let _guard = lock.lock().await;
        // 锁内重读：排队期间状态可能已变化（前一消息已消费/已取消）
        let flow = self.team_flows.get(&key);
        match flow {
            Some(flow) => match &flow.state {
                TeamFlowState::WaitingGoal => self.team_waiting_goal(ev, &key, text).await,
                TeamFlowState::WaitingConfirm { goal, plan } => {
                    self.team_waiting_confirm(ev, &key, goal, plan, text).await
                }
            },
            None => match intent {
                Some(goal) if goal.is_empty() => {
                    self.team_flows.set(
                        &key,
                        TeamFlow {
                            state: TeamFlowState::WaitingGoal,
                            updated_at: crate::chrono_lite::unix_secs(),
                        },
                    );
                    Some(
                        "想建一个什么样的团队？直接发一句话描述目标（如「帮我们建一个做记账 App 的研发团队」），我来出方案。"
                            .to_string(),
                    )
                }
                Some(goal) => self.team_generate(ev, &key, &goal, None).await,
                None => None,
            },
        }
    }

    /// WaitingGoal：等待用户补充团队目标（"取消" 中止；下一条消息即目标）。
    async fn team_waiting_goal(&self, ev: &Ev, key: &str, text: &str) -> Option<String> {
        let t = text.trim();
        if t == "取消" {
            self.team_flows.remove(key);
            return Some("已取消团队创建，随时可以再发起。".to_string());
        }
        if t.is_empty() {
            return Some("想建一个什么样的团队？直接发一句话描述目标即可。".to_string());
        }
        self.team_generate(ev, key, t, None).await
    }

    /// WaitingConfirm：确认创建 / 改：xxx 重新生成 / 取消中止；其它消息放行 agent
    /// （不打断普通对话，流程保留，用户可随时回来确认）。
    async fn team_waiting_confirm(
        &self,
        ev: &Ev,
        key: &str,
        goal: &str,
        plan: &TeamPlan,
        text: &str,
    ) -> Option<String> {
        let t = text.trim();
        if t == "确认" {
            return Some(self.team_confirm(ev, key, plan).await);
        }
        if t == "取消" {
            self.team_flows.remove(key);
            return Some("已取消团队创建，随时可以再发起。".to_string());
        }
        if let Some(rev) = t.strip_prefix("改：").or_else(|| t.strip_prefix("改:")) {
            let rev = rev.trim();
            if rev.is_empty() {
                return Some("请说明怎么改，例如「改：把测试改成运营」。".to_string());
            }
            return self.team_generate(ev, key, goal, Some(rev)).await;
        }
        None
    }

    /// 生成团队方案（加载提示 → LLM 生成 → 落 WaitingConfirm → 返回预览）。
    /// 生成期间注册打断标志：用户发「取消」→ 生成完检查后中止（与 agent 路径同款）。
    async fn team_generate(
        &self,
        ev: &Ev,
        key: &str,
        goal: &str,
        revision: Option<&str>,
    ) -> Option<String> {
        if let Err(e) = self.send_reply(ev, "⏳ 正在为你组建团队…").await {
            crate::log!("[bridge] 团队加载提示发送失败: {e:#}");
        }
        let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.cancel_flags
            .lock()
            .unwrap()
            .insert(key.to_string(), cancel_flag.clone());
        let backend = Backend::parse(self.bot.effective_backend(&self.default_backend));
        // 调整（改：xxx）时把原目标 + 调整要求合并给 LLM（保留上下文）
        let prompt_goal = match revision {
            Some(rev) => format!("{goal}\n调整要求：{rev}"),
            None => goal.to_string(),
        };
        let plan = self
            .team_gen
            .generate(backend, &prompt_goal, &[], None)
            .await;
        self.cancel_flags.lock().unwrap().remove(key);
        if cancel_flag.load(Ordering::Relaxed) {
            self.team_flows.remove(key);
            return Some("已取消团队创建。".to_string());
        }
        match plan {
            Ok(p) => {
                self.team_flows.set(
                    key,
                    TeamFlow {
                        state: TeamFlowState::WaitingConfirm {
                            goal: goal.to_string(),
                            plan: p.clone(),
                        },
                        updated_at: crate::chrono_lite::unix_secs(),
                    },
                );
                Some(render_preview(&p))
            }
            Err(e) => {
                if revision.is_some() {
                    // 调整失败：保留原方案，可再次调整或直接确认
                    Some(format!(
                        "⚠️ 方案调整失败：{e}\n可回复「改：xxx」再次调整，或回复「确认」用当前方案创建。"
                    ))
                } else {
                    Some(format!(
                        "⚠️ 团队方案生成失败：{e}\n可稍后重发创建意图重试。"
                    ))
                }
            }
        }
    }

    /// 确认创建：逐角色建群 + 登记，返回创建清单。
    /// 全部成功 → 清会话；部分/全部失败 → 保留会话（回复「确认」可重试未成功角色，
    /// 已成功的不重复建——create_team_groups 按登记表幂等跳过）。
    async fn team_confirm(&self, ev: &Ev, key: &str, plan: &TeamPlan) -> String {
        // owner 平台 id：飞书建群必须把 owner 拉进群（否则用户看不到群）
        let owner = if self.bot.is_wechat() {
            self.bot.wx_user_id.clone()
        } else if self.bot.is_dingtalk() {
            crate::config::first_owner_id(&self.bot.ding_owner_ids).unwrap_or_default()
        } else {
            crate::config::first_owner_id(&self.bot.owner_open_id).unwrap_or_default()
        };
        if owner.is_empty() {
            self.team_flows.remove(key);
            return "⚠️ 未配置管理员（owner）标识，无法自动建群。请在设置里完成 owner 配置后重试。"
                .to_string();
        }
        if let Err(e) = self.send_reply(ev, "⏳ 正在创建团队…").await {
            crate::log!("[bridge] 团队创建提示发送失败: {e:#}");
        }
        let outcomes = crate::teamflow::create_team_groups(
            self.msgr.as_ref(),
            &self.vb_store,
            &self.bot.key(),
            &owner,
            plan,
        )
        .await;
        let reply = render_create_result(plan, &outcomes);
        // #147：建群完成后登记团队（聊天入口 ↔ GUI 同一份数据源；部分成功也登记，
        // 重试「确认」时 register_created 按角色名合并补建成功的角色）。登记失败不阻断。
        if outcomes.iter().any(|o| o.ok) {
            let regs = crate::teamreg::role_regs_from_plan(
                plan,
                &crate::virtualbot::VirtualBotStore::new(),
                &self.bot.key(),
            );
            if let Err(e) = crate::teamreg::TeamStore::new().register_created(
                &self.bot.key(),
                &plan.team_name,
                regs,
            ) {
                crate::log!(
                    "[bridge] 团队登记失败 bot={} team={}: {e}",
                    self.bot.key(),
                    plan.team_name
                );
            }
        }
        if outcomes.iter().all(|o| o.ok) {
            self.team_flows.remove(key);
        }
        reply
    }
}
