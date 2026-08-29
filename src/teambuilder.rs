//! 一键创建团队（#100）——提示词模板 + LLM 生成团队方案。
//!
//! 定位（2026-08-27 设计重定义）：不再维护固定「角色清单 JSON」，
//! 而是维护**组建方法论提示词**——用户输入一句话目标 + 可选成员名单，
//! LLM 现场生成团队方案（结构化 JSON），预览确认后复用 #75 批量建群。
//!
//! 本模块（P0 核心）：
//! 1. 团队方案数据模型 + schema 校验（字段完整 / 角色数 2-10 / 重名拦截 /
//!    成员不重复分配 / prompt 长度对齐群介绍限制）
//! 2. 组建方法论提示词 + 内置 3 份起手式模板（软件产品 / 自媒体 / OPC）
//! 3. LLM 生成链路：调本机 agent（claude -p / codex exec / pi -p json），
//!    强制 JSON 解析 + schema 校验——**校验失败提示重试/手动编辑，不直接建群**
//!    （非确定性兜底，与需求「先预览确认再执行创建」一致）
//!
//! P1（执行创建 + 成员名 + 授权码）复用 #75 VirtualBotStore / #30 授权码，
//! 依赖本模块落地后由 UI 预览确认流程推进。

use serde::{Deserialize, Serialize};

/// 角色数下限/上限（需求 3.2：数量约束 2-10 个角色）。
pub const MIN_ROLES: usize = 2;
pub const MAX_ROLES: usize = 10;
/// 群介绍长度上限（对齐 virtualbot::ROLE_PROMPT_MAX，飞书群描述限制）。
pub const ROLE_PROMPT_MAX: usize = 100;

/// 团队方案里的一个角色：role_name（角色名=群名）+ member_name（谁担任，可空=待任命）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamRole {
    pub role_name: String,
    #[serde(default)]
    pub member_name: Option<String>,
    pub system_prompt: String,
}

/// LLM 生成的团队方案（预览确认的对象）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamPlan {
    pub team_name: String,
    pub roles: Vec<TeamRole>,
    /// 协作关系简述（团队内 @角色名 寻址的协作接口设计）。
    #[serde(default)]
    pub collab: Option<String>,
}

/// 组建方法论提示词模板：领域聚焦说明 + 名称，公共方法论见 [`methodology_prompt`]。
#[derive(Debug, Clone)]
pub struct TeamPromptTemplate {
    pub name: String,
    pub description: String,
    /// 领域聚焦提示词（起手式：软件产品 / 自媒体 / OPC 的组建侧重）。
    pub focus: String,
}

/// 内置 3 份起手式模板（需求 3.1：仅作参考起点，最终产出由 LLM 结合用户输入决定）。
pub fn builtin_team_templates() -> Vec<TeamPromptTemplate> {
    vec![
        TeamPromptTemplate {
            name: "软件产品团队".into(),
            description: "从产品目标拆解研发/设计/运营角色".into(),
            focus: "领域：软件产品研发。组建侧重：产品定义 → 技术架构 → 设计 → 测试 → 运营增长，\
                    角色覆盖端到端交付闭环，避免只堆开发角色。"
                .into(),
        },
        TeamPromptTemplate {
            name: "自媒体团队".into(),
            description: "内容生产/分发/增长的角色配置".into(),
            focus: "领域：内容自媒体。组建侧重：选题策划 → 内容生产 → 视觉/剪辑 → 平台分发 → \
                    数据复盘，角色覆盖内容闭环与增长。"
                .into(),
        },
        TeamPromptTemplate {
            name: "OPC 团队".into(),
            description: "运营/产品/增长三角色经典配置".into(),
            focus: "领域：OPC（运营-产品-增长）。组建侧重：运营抓手、产品迭代、增长实验三者分工\
                    协作，角色精简聚焦。"
                .into(),
        },
    ]
}

/// 按名称取内置模板；找不到回退第一个（默认「软件产品团队」）。
pub fn resolve_template(name: Option<&str>) -> TeamPromptTemplate {
    let t = builtin_team_templates();
    match name {
        Some(n) => t
            .iter()
            .find(|x| x.name == n)
            .cloned()
            .unwrap_or_else(|| t[0].clone()),
        None => t[0].clone(),
    }
}

/// 公共组建方法论提示词（模板 = 方法论，团队结构由 LLM 现场推导）。
fn methodology_prompt() -> &'static str {
    "你是团队组建专家。根据用户目标设计一个虚拟团队方案，输出**严格 JSON**（不要 markdown 代码块、\
     不要解释、不要多余文字）。规则：\
     1. 目标拆解：把用户目标拆成 2-10 个必要职责，每个职责对应一个角色；\
     2. 角色推导：角色名是「职位语义」（如 产品经理/后端工程师），不是人名；\
     3. 成员分配：若用户给了成员名单，按能力把名字分给角色，一个成员只能担任一个角色；\
     没给名单则全部 member_name 置 null（待任命）；\
     4. system_prompt：为该角色写一条飞书群聊机器人系统提示词（群介绍），不超过 100 个中文字符，\
     必须以「你是{角色名}，名字叫{成员名}」开头（成员待任命时写「你是{角色名}，名字待任命」），\
     直接描述职责与协作方式，不要引号；\
     5. 数量约束：角色数必须 2-10 个，role_name 不得重复；\
     6. collab：一句话描述角色间的协作关系（谁产出交给谁）。\
     JSON 结构：{\"team_name\":\"团队名\",\"roles\":[{\"role_name\":\"...\",\"member_name\":\"...或null\",\
     \"system_prompt\":\"...\"}],\"collab\":\"...\"}"
}

/// 拼装完整生成 prompt：方法论 + 领域聚焦 + 用户目标 + 成员名单。
pub fn build_generation_prompt(
    template: &TeamPromptTemplate,
    goal: &str,
    members: &[String],
) -> String {
    let mut p = String::new();
    p.push_str(methodology_prompt());
    p.push_str("\n\n");
    p.push_str(&template.focus);
    p.push_str("\n\n用户目标：");
    p.push_str(goal);
    if members.is_empty() {
        p.push_str("\n成员名单：未提供（全部待任命）");
    } else {
        p.push_str("\n成员名单：");
        p.push_str(&members.join("、"));
    }
    p
}

/// 从模型输出中提取 JSON 对象文本（容错 markdown ```json 代码块与前后杂文）。
fn extract_json(text: &str) -> Option<String> {
    let t = text.trim();
    let start = t.find('{')?;
    let end = t.rfind('}')?;
    if end <= start {
        return None;
    }
    Some(t[start..=end].to_string())
}

/// schema 校验 + 解析：字段完整 / 角色数 2-10 / role_name 非空不重复 /
/// member_name 不重复分配 / system_prompt 非空且 ≤100 字。
/// 返回 Err 时文案面向用户（提示重试或手动编辑），**调用方不得直接建群**。
pub fn validate_team_plan_json(text: &str) -> Result<TeamPlan, String> {
    let json = extract_json(text).ok_or_else(|| {
        "未能在模型输出中找到 JSON 团队方案（输出可能是空/被截断）。请重试或手动编辑团队方案。"
            .to_string()
    })?;
    let plan: TeamPlan = serde_json::from_str(&json)
        .map_err(|e| format!("团队方案 JSON 解析失败（{e}）。请重试或手动编辑。"))?;

    let team_name = plan.team_name.trim();
    if team_name.is_empty() {
        return Err("团队名（team_name）不能为空。请重试或手动编辑。".into());
    }

    let n = plan.roles.len();
    if !(MIN_ROLES..=MAX_ROLES).contains(&n) {
        return Err(format!(
            "角色数量 {n} 超出约束（需 {MIN_ROLES}-{MAX_ROLES} 个）。请调整后重试。"
        ));
    }

    let mut seen_roles = std::collections::HashSet::new();
    let mut seen_members = std::collections::HashSet::new();
    for r in &plan.roles {
        let rn = r.role_name.trim();
        if rn.is_empty() {
            return Err("存在空 role_name。请重试或手动编辑。".into());
        }
        if !seen_roles.insert(rn.to_string()) {
            return Err(format!("角色名「{rn}」重复。请重试或手动编辑。"));
        }
        let sp = r.system_prompt.trim();
        if sp.is_empty() {
            return Err(format!(
                "角色「{rn}」的 system_prompt 为空。请重试或手动编辑。"
            ));
        }
        if sp.chars().count() > ROLE_PROMPT_MAX {
            return Err(format!(
                "角色「{rn}」的 system_prompt 超长（{} 字 > {ROLE_PROMPT_MAX}）。请缩短后重试。",
                sp.chars().count()
            ));
        }
        if let Some(m) = r
            .member_name
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            if !seen_members.insert(m.to_string()) {
                return Err(format!("成员「{m}」被分配给了多个角色。请重试或手动编辑。"));
            }
        }
    }
    Ok(plan)
}

/// 一次 stdin 问答调本机 agent 生成团队方案（claude -p / codex exec / pi -p json）。
/// 复用 generate_role_prompt 的调用模式：deps::find_in_path 解析绝对路径、
/// Windows 抑制控制台窗口（#104）、stdin 写 prompt + EOF、超时兜底。
/// 返回解析 + 校验后的 [`TeamPlan`]，失败给用户可操作错误（重试/手动编辑）。
/// #123（2026-08-27）：codex 分支对齐 agent.rs::codex_command 成熟版（--json
/// --skip-git-repo-check + owner 沙箱 + 供应商 -c 注入）；claude/pi 分支补供应商
/// env / --provider/--model 注入；stderr 不再吞（失败原因透传，替代「空输出」）。
pub async fn generate_team_plan(
    backend: crate::agent::Backend,
    goal: &str,
    members: &[String],
    template_name: Option<&str>,
) -> Result<TeamPlan, String> {
    if goal.trim().is_empty() {
        return Err("团队目标不能为空。".into());
    }
    let template = resolve_template(template_name);
    let prompt = build_generation_prompt(&template, goal, members);

    // #123：供应商注入与主链路（agent.rs run_once）同源。CLI 无 bot 上下文：
    // 优先 AGENT_BRIDGE_BOT_KEY env（桥内调用），否则回落全局 default_provider；
    // 未配置供应商 = 各后端自认证/CC Switch 的旧行为（build_injection 内部处理）。
    let provider = crate::config::Config::load().ok().and_then(|cfg| {
        match std::env::var("AGENT_BRIDGE_BOT_KEY") {
            Ok(bk) if !bk.is_empty() => cfg
                .bots
                .iter()
                .find(|b| b.key() == bk)
                .and_then(|b| cfg.resolve_provider(b))
                .cloned(),
            _ => cfg
                .providers
                .iter()
                .find(|p| p.name == cfg.default_provider)
                .cloned(),
        }
    });
    let inject = crate::agent::build_injection(backend, provider.as_ref())?;

    let program = match backend {
        crate::agent::Backend::Pi => "pi",
        crate::agent::Backend::Codex => "codex",
        crate::agent::Backend::Claude => "claude",
    };
    let resolved =
        crate::deps::find_in_path(program).unwrap_or_else(|| std::path::PathBuf::from(program));
    // codex >= 0.146 才支持 --add-dir（沙箱 workspace-write 需要）；旧版回退 bypass。
    let sandbox_mode_ok = if backend == crate::agent::Backend::Codex {
        resolved
            .to_str()
            .and_then(crate::deps::codex_version)
            .map(|v| crate::deps::version_at_least(&v, "0.146"))
            .unwrap_or(false)
    } else {
        false
    };
    let mut cmd = build_agent_command(backend, &resolved, &inject, sandbox_mode_ok);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped()); // #123：stderr 不再吞，失败原因透传

    #[cfg(windows)]
    let mut child = {
        // #153：隐藏控制台 spawn（CreateProcessW + SW_HIDE），agent 内部 Bash 孙进程
        // 继承隐藏控制台 → 不再闪可见黑框；参数/环境从同一 tokio Command 提取。
        use std::ffi::OsString;
        let program = cmd.as_std().get_program().to_os_string();
        let args: Vec<OsString> = cmd.as_std().get_args().map(|a| a.to_os_string()).collect();
        let cwd = cmd.as_std().get_current_dir().map(|p| p.to_path_buf());
        let envs: Vec<(OsString, Option<OsString>)> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| (k.to_os_string(), v.map(|x| x.to_os_string())))
            .collect();
        crate::winproc::spawn_hidden(&program, &args, cwd.as_deref(), &envs).map_err(|e| {
            format!(
                "启动 {} 失败：{e}（未安装？请先在一键安装里装好后端）",
                program.to_string_lossy()
            )
        })?
    };
    #[cfg(not(windows))]
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("启动 {program} 失败：{e}（未安装？请先在一键安装里装好后端）"))?;
    use tokio::io::AsyncWriteExt;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .await
            .map_err(|e| format!("写入 prompt 失败：{e}"))?;
        drop(stdin); // EOF：触发后端非交互模式处理
    }
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| "团队方案生成超时（120s）。请重试或改短目标。".to_string())?
    .map_err(|e| format!("等待 {program} 退出失败：{e}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // codex：--json 事件流（item.completed/agent_message）；pi：JSONL message_end；claude：stdout 直接文本
    let raw = match backend {
        crate::agent::Backend::Codex => codex_json_text(&stdout),
        crate::agent::Backend::Pi => pi_json_text(&stdout),
        crate::agent::Backend::Claude => stdout.to_string(),
    };

    validate_team_plan_json(&raw).map_err(|e| {
        let mut msg = format!("{e}\n（模型原始输出片段：{}）", preview(&raw, 200));
        let err = stderr.trim();
        if !err.is_empty() {
            msg.push_str(&format!("\n（模型 stderr：{}）", preview(err, 300)));
        }
        msg
    })
}

/// 单轮 agent 命令构造（#123：与 agent.rs run_once 同源）。
/// - codex：复用 agent::codex_command 成熟版（--json --skip-git-repo-check + owner
///   沙箱 workspace-write / <0.146 回退 bypass + 供应商 -c 注入），单轮生成不涉受限会话。
/// - claude：-p --output-format text + 供应商 ANTHROPIC_* env 注入（对齐 run_once，
///   不再依赖全局 env 恰好有 key）。
/// - pi：-p --mode json --session-id <uuid> + 供应商 --provider/--model + api key env。
fn build_agent_command(
    backend: crate::agent::Backend,
    resolved: &std::path::Path,
    inject: &crate::agent::Injection,
    sandbox_mode_ok: bool,
) -> tokio::process::Command {
    let mut cmd = match backend {
        crate::agent::Backend::Claude => {
            let mut c = tokio::process::Command::from(crate::agent::shim_command(resolved));
            c.arg("-p").arg("--output-format").arg("text");
            c
        }
        crate::agent::Backend::Codex => {
            // #123：对齐 agent.rs::codex_command——--json --skip-git-repo-check +
            // owner 沙箱（workspace-write，默认域=cwd；bridge_dir 可写根保住
            // $ABB_BIN job/deliver 落盘域）。codex < 0.146 无 --add-dir → 回退 bypass。
            let writable_roots: Vec<std::path::PathBuf> = if sandbox_mode_ok {
                vec![crate::bridge_dir()]
            } else {
                Vec::new()
            };
            crate::agent::codex_command(
                resolved,
                false,
                "",
                &inject.extra_args,
                false,
                &writable_roots,
                false, // resume_bypass：team generate 恒全新会话，resume=false 无意义
            )
        }
        crate::agent::Backend::Pi => {
            // pi 非交互 JSON 模式：stdin 读 prompt，stdout JSONL（message_end 权威文本）。
            let mut c = tokio::process::Command::from(crate::agent::shim_command(resolved));
            c.arg("-p")
                .arg("--mode")
                .arg("json")
                .arg("--session-id")
                .arg(uuid::Uuid::new_v4().to_string());
            // 桥内供应商 → --provider/--model（api key 走 env，见 build_injection）
            for a in &inject.extra_args {
                c.arg(a);
            }
            c
        }
    };
    // 供应商 env（codex 的 AGENT_BRIDGE_MODEL_KEY / claude 的 ANTHROPIC_* / pi 的 api key）
    if let Some(env) = &inject.env {
        cmd.envs(env);
    }
    cmd
}

/// codex `--json` 事件流 → 最后一条 agent_message 文本（与 agent.rs process_line 同源解析）。
fn codex_json_text(stdout: &str) -> String {
    let mut last = String::new();
    for line in stdout.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v.get("type").and_then(|t| t.as_str()) == Some("item.completed") {
                let item = &v["item"];
                if item.get("type").and_then(|t| t.as_str()) == Some("agent_message") {
                    let t = item
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    if !t.is_empty() {
                        last = t;
                    }
                }
            }
        }
    }
    last
}

/// pi JSONL → 最后一条 message_end（assistant）的文本。
fn pi_json_text(stdout: &str) -> String {
    let mut last = String::new();
    for line in stdout.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v.get("type").and_then(|t| t.as_str()) == Some("message_end") {
                let msg = &v["message"];
                if msg.get("role").and_then(|r| r.as_str()) == Some("assistant") {
                    let t = crate::agent::pi_message_text(msg);
                    if !t.is_empty() {
                        last = t;
                    }
                }
            }
        }
    }
    last
}

/// 输出片段预览（错误提示用，截断到 n 字符，避免刷屏）。
fn preview(s: &str, n: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= n {
        t.to_string()
    } else {
        let cut: String = t.chars().take(n).collect();
        format!("{cut}…")
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_plan() -> String {
        r#"{
            "team_name": "记账App团队",
            "roles": [
                {"role_name": "产品经理", "member_name": "小王", "system_prompt": "你是产品经理，名字叫小王，负责需求与验收。"},
                {"role_name": "后端工程师", "member_name": "steven", "system_prompt": "你是后端工程师，名字叫steven，负责API与数据。"}
            ],
            "collab": "产品经理产出需求，后端工程师实现。"
        }"#
        .to_string()
    }

    #[test]
    fn accepts_valid_plan() {
        let p = validate_team_plan_json(&valid_plan()).unwrap();
        assert_eq!(p.team_name, "记账App团队");
        assert_eq!(p.roles.len(), 2);
        assert_eq!(p.roles[0].member_name.as_deref(), Some("小王"));
        assert!(p.collab.is_some());
    }

    #[test]
    fn strips_markdown_code_fence() {
        let wrapped = format!("```json\n{}\n```", valid_plan());
        assert!(validate_team_plan_json(&wrapped).is_ok());
    }

    #[test]
    fn rejects_non_json() {
        assert!(validate_team_plan_json("好的，我来设计团队：").is_err());
        assert!(validate_team_plan_json("").is_err());
    }

    #[test]
    fn rejects_empty_team_name() {
        let t = valid_plan().replace("记账App团队", "");
        assert!(validate_team_plan_json(&t).is_err());
    }

    #[test]
    fn rejects_too_few_roles() {
        let t = r#"{"team_name":"x","roles":[{"role_name":"产品经理","member_name":null,"system_prompt":"你是产品经理。"}]}"#;
        assert!(validate_team_plan_json(t).is_err());
    }

    #[test]
    fn rejects_too_many_roles() {
        let mut roles = String::new();
        for i in 0..11 {
            roles.push_str(&format!(
                "{{\"role_name\":\"角色{i}\",\"member_name\":null,\"system_prompt\":\"你是角色{i}。\"}},"
            ));
        }
        let t = format!("{{\"team_name\":\"x\",\"roles\":[{}]}}", roles);
        assert!(validate_team_plan_json(&t).is_err());
    }

    #[test]
    fn accepts_boundary_10_roles() {
        let roles: Vec<String> = (0..10)
            .map(|i| {
                format!(
                    "{{\"role_name\":\"角色{i}\",\"member_name\":null,\"system_prompt\":\"你是角色{i}。\"}}"
                )
            })
            .collect();
        let t = format!("{{\"team_name\":\"x\",\"roles\":[{}]}}", roles.join(","));
        assert!(validate_team_plan_json(&t).is_ok());
    }

    #[test]
    fn rejects_duplicate_role_name() {
        let t = r#"{"team_name":"x","roles":[
            {"role_name":"产品经理","member_name":null,"system_prompt":"你是产品经理。"},
            {"role_name":"产品经理","member_name":null,"system_prompt":"你是产品经理。"}]}"#;
        assert!(validate_team_plan_json(t).is_err());
    }

    #[test]
    fn rejects_duplicate_member() {
        let t = r#"{"team_name":"x","roles":[
            {"role_name":"产品经理","member_name":"小王","system_prompt":"你是产品经理，名字叫小王。"},
            {"role_name":"后端工程师","member_name":"小王","system_prompt":"你是后端工程师，名字叫小王。"}]}"#;
        assert!(validate_team_plan_json(t).is_err());
    }

    #[test]
    fn rejects_empty_prompt() {
        let t = r#"{"team_name":"x","roles":[
            {"role_name":"产品经理","member_name":null,"system_prompt":""},
            {"role_name":"后端工程师","member_name":null,"system_prompt":"你是后端工程师。"}]}"#;
        assert!(validate_team_plan_json(t).is_err());
    }

    #[test]
    fn rejects_oversized_prompt() {
        let long = "长".repeat(ROLE_PROMPT_MAX + 1);
        let t = format!(
            r#"{{"team_name":"x","roles":[
            {{"role_name":"产品经理","member_name":null,"system_prompt":"{long}"}},
            {{"role_name":"后端工程师","member_name":null,"system_prompt":"你是后端工程师。"}}]}}"#
        );
        assert!(validate_team_plan_json(&t).is_err());
    }

    #[test]
    fn templates_cover_three_domains() {
        let ts = builtin_team_templates();
        assert_eq!(ts.len(), 3);
        for t in &ts {
            assert!(!t.name.is_empty());
            assert!(!t.focus.is_empty());
        }
    }

    #[test]
    fn prompt_builds_with_and_without_members() {
        let t = resolve_template(Some("自媒体团队"));
        let p1 = build_generation_prompt(&t, "做美食短视频", &[]);
        assert!(p1.contains("美食短视频"));
        assert!(p1.contains("未提供"));
        let p2 = build_generation_prompt(&t, "做美食短视频", &["小王".into(), "steven".into()]);
        assert!(p2.contains("小王、steven"));
        assert!(p2.contains("用户目标"));
    }

    #[test]
    fn extract_json_handles_surrounding_text() {
        assert_eq!(
            extract_json("好的，方案如下：{\"a\":1}\n希望能帮到你").as_deref(),
            Some("{\"a\":1}")
        );
        assert_eq!(extract_json("无 JSON").as_deref(), None);
    }

    // ── #123：命令构造对齐 agent.rs run_once（QA 静态核对 A8）──

    fn args_of(cmd: &tokio::process::Command) -> Vec<String> {
        cmd.as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn envs_of(cmd: &tokio::process::Command) -> Vec<(String, String)> {
        cmd.as_std()
            .get_envs()
            .filter_map(|(k, v)| {
                v.map(|v| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect()
    }

    #[test]
    fn build_agent_command_codex_aligns_run_once() {
        // codex + OpenAI 兼容供应商：--json --skip-git-repo-check + workspace-write
        // 沙箱 + --add-dir(bridge_dir) + 供应商 -c 注入；api key 只进 env 不进 argv。
        let provider = crate::config::ProviderConfig {
            name: "测试网关".into(),
            kind: "openai-chat".into(),
            base_url: "https://api.example.com/v1".into(),
            api_key: "sk-test-123".into(),
            model: "gpt-x".into(),
        };
        let inject =
            crate::agent::build_injection(crate::agent::Backend::Codex, Some(&provider)).unwrap();
        let cmd = build_agent_command(
            crate::agent::Backend::Codex,
            std::path::Path::new("codex"),
            &inject,
            true,
        );
        let args = args_of(&cmd);
        assert!(args.iter().any(|a| a == "exec"));
        assert!(args.iter().any(|a| a == "--json"));
        assert!(args.iter().any(|a| a == "--skip-git-repo-check"));
        assert!(args.iter().any(|a| a == "--sandbox"));
        assert!(args.iter().any(|a| a == "workspace-write"));
        assert!(args.iter().any(|a| a == "--add-dir"));
        assert!(args.iter().any(|a| a == "-c"));
        assert!(args.iter().any(|a| a.starts_with("model_provider=")));
        assert!(args
            .iter()
            .any(|a| a.starts_with("model_providers.agent_bridge.base_url=")));
        // api key 不进 argv
        assert!(!args.iter().any(|a| a.contains("sk-test-123")));
        let envs = envs_of(&cmd);
        assert!(envs
            .iter()
            .any(|(k, v)| k == "AGENT_BRIDGE_MODEL_KEY" && v == "sk-test-123"));
    }

    #[test]
    fn build_agent_command_codex_old_version_falls_back_bypass() {
        // codex < 0.146（无 --add-dir）：与 run_once 同款回退 bypass，不带 --sandbox。
        let inject = crate::agent::build_injection(crate::agent::Backend::Codex, None).unwrap();
        let cmd = build_agent_command(
            crate::agent::Backend::Codex,
            std::path::Path::new("codex"),
            &inject,
            false,
        );
        let args = args_of(&cmd);
        assert!(args
            .iter()
            .any(|a| a == "--dangerously-bypass-approvals-and-sandbox"));
        assert!(!args.iter().any(|a| a == "--sandbox"));
        assert!(args.iter().any(|a| a == "--skip-git-repo-check"));
    }

    #[test]
    fn build_agent_command_claude_injects_provider_env() {
        // claude + anthropic 供应商：ANTHROPIC_* env 注入（不再依赖全局 env 恰好有 key）。
        let provider = crate::config::ProviderConfig {
            name: "测试Anthropic".into(),
            kind: "anthropic".into(),
            base_url: "https://api.anthropic.example".into(),
            api_key: "sk-ant-test".into(),
            model: "claude-x".into(),
        };
        let inject =
            crate::agent::build_injection(crate::agent::Backend::Claude, Some(&provider)).unwrap();
        let cmd = build_agent_command(
            crate::agent::Backend::Claude,
            std::path::Path::new("claude"),
            &inject,
            false,
        );
        let args = args_of(&cmd);
        assert!(args.iter().any(|a| a == "-p"));
        assert!(!args.iter().any(|a| a.contains("sk-ant-test")));
        let envs = envs_of(&cmd);
        assert!(envs
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_AUTH_TOKEN" && v == "sk-ant-test"));
        assert!(envs
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v == "https://api.anthropic.example"));
        assert!(envs
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_MODEL" && v == "claude-x"));
    }

    #[test]
    fn build_agent_command_pi_injects_provider_args() {
        // pi + OpenAI 兼容供应商：--provider/--model 参数 + OPENAI_API_KEY env。
        let provider = crate::config::ProviderConfig {
            name: "测试OpenAI".into(),
            kind: "openai-responses".into(),
            base_url: "https://api.openai.example/v1".into(),
            api_key: "sk-pi-test".into(),
            model: "gpt-pi".into(),
        };
        let inject =
            crate::agent::build_injection(crate::agent::Backend::Pi, Some(&provider)).unwrap();
        let cmd = build_agent_command(
            crate::agent::Backend::Pi,
            std::path::Path::new("pi"),
            &inject,
            false,
        );
        let args = args_of(&cmd);
        assert!(args.iter().any(|a| a == "--provider"));
        assert!(args.iter().any(|a| a == "openai"));
        assert!(args.iter().any(|a| a == "--model"));
        assert!(args.iter().any(|a| a == "gpt-pi"));
        assert!(!args.iter().any(|a| a.contains("sk-pi-test")));
        let envs = envs_of(&cmd);
        assert!(envs
            .iter()
            .any(|(k, v)| k == "OPENAI_API_KEY" && v == "sk-pi-test"));
    }

    #[test]
    fn codex_json_text_extracts_last_agent_message() {
        // #123：codex --json 事件流 → 最后一条 agent_message 文本（对齐 process_line）。
        let out = "{\"type\":\"thread.started\",\"thread_id\":\"t1\"}\n\
                   {\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"先输出一句：\"}}\n\
                   {\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"{\\\"team_name\\\":\\\"记账团队\\\",\\\"roles\\\":[]}\"}}\n";
        let t = codex_json_text(out);
        assert!(t.contains("team_name"));
        assert!(!t.contains("先输出一句"));
    }

    #[test]
    fn codex_json_text_ignores_non_message_events() {
        let out = "{\"type\":\"thread.started\",\"thread_id\":\"t1\"}\n\
                   {\"type\":\"item.completed\",\"item\":{\"type\":\"function_call\",\"name\":\"x\"}}\n";
        assert_eq!(codex_json_text(out), "");
    }

    #[test]
    fn pi_json_text_extracts_last_message_end() {
        let out = "{\"type\":\"session\",\"session_id\":\"s1\"}\n\
                   {\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"方案：{\\\"team_name\\\":\\\"A\\\"}\"}]}}\n";
        assert!(pi_json_text(out).contains("team_name"));
    }
}
