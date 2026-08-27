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
///
/// #123（2026-08-27 P0）：codex 分支对齐 `agent.rs::codex_command`——exec + `--json` +
/// `--skip-git-repo-check` + 沙箱（workspace-write + bridge_dir 可写根）+ 桥内供应商 -c 注入；
/// stderr 不再吞（`Stdio::null()` → piped，失败原因透传）；claude/pi 分支也补桥内供应商
/// 注入（claude ANTHROPIC_* env / pi --provider/--model），消除对全局 env 的依赖。
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

    let program = match backend {
        crate::agent::Backend::Pi => "pi",
        crate::agent::Backend::Codex => "codex",
        crate::agent::Backend::Claude => "claude",
    };
    let resolved =
        crate::deps::find_in_path(program).unwrap_or_else(|| std::path::PathBuf::from(program));

    // 桥内供应商注入（对齐 agent.rs 主链路）：桥内配置了供应商 → 按后端注入
    //（codex -c / pi --provider/--model / claude ANTHROPIC_* env）；未配置 → 不注入，
    // 回落各后端自带登录态 / 父进程 env（保持历史行为，不强制要求 CC Switch）。
    // bot_key 由桥注入环境提供；纯 CLI 手动调用时取不到则跳过桥内供应商。
    let bot_key = std::env::var("AGENT_BRIDGE_BOT_KEY").ok();
    let provider = bot_key
        .as_deref()
        .and_then(crate::config::Config::provider_for_bot_key);
    let inject = provider
        .as_ref()
        .map(|p| crate::agent::build_injection(backend, Some(p)))
        .transpose()?; // 类型不匹配（如 codex+anthropic 供应商）直接报错返回，不进子进程

    // codex：owner 语义沙箱默认域 = 当前目录（workspace-write），额外可写根 = bridge_dir
    //（与 agent.rs 同款，保住 $ABB_BIN job/deliver 落盘域）；codex < 0.146 无 --add-dir →
    // 传空回退 bypass（保持现状行为）。
    let codex_writable_roots: Vec<std::path::PathBuf> = if backend == crate::agent::Backend::Codex
        && crate::deps::codex_version(resolved.to_str().unwrap_or("codex"))
            .map(|v| crate::deps::version_at_least(&v, "0.146"))
            .unwrap_or(false)
    {
        vec![crate::bridge_dir()]
    } else {
        Vec::new()
    };

    let mut cmd = generation_command(
        backend,
        &resolved,
        inject
            .as_ref()
            .map(|i| i.extra_args.as_slice())
            .unwrap_or(&[]),
        &codex_writable_roots,
    );
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped()); // #123：不再吞 stderr，失败原因可透传
    if let Some(inj) = &inject {
        if let Some(env) = &inj.env {
            cmd.envs(env);
        }
    }
    // 不覆写 PATH（继承父进程 env）：composed_path() 在本机 Git Bash 环境下可长达
    // 10k+ 字符，超出 cmd.exe 的 ~8191 字符 PATH 截断上限 → codex.cmd shim 里的
    // `"node"` 解析失败（实测 2026-08-27）；原实现不覆写 PATH，继承父 env 即可用。

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
    let raw = extract_reply_text(backend, &stdout);

    validate_team_plan_json(&raw).map_err(|e| generation_error(e, &stdout, &stderr))
}

/// 构造一次生成调用的命令（纯函数，可单测）。
/// codex 分支**直接复用** `agent.rs::codex_command`（#123 参数对齐：exec + `--json` +
/// `--skip-git-repo-check` + 沙箱 + `-c` 供应商注入），杜绝两处实现漂移；
/// pi 分支追加桥内供应商 `--provider/--model` 参数（与 agent.rs run_once 同款）。
fn generation_command(
    backend: crate::agent::Backend,
    resolved: &std::path::Path,
    extra_args: &[String],
    writable_roots: &[std::path::PathBuf],
) -> tokio::process::Command {
    let mut cmd = match backend {
        crate::agent::Backend::Claude => {
            let mut c = tokio::process::Command::from(crate::agent::shim_command(resolved));
            c.arg("-p").arg("--output-format").arg("text");
            c
        }
        crate::agent::Backend::Codex => crate::agent::codex_command(
            resolved,
            false,
            "",
            extra_args,
            false, // team generate 是 owner 侧一次性指令，无受限模式
            writable_roots,
        ),
        crate::agent::Backend::Pi => {
            // pi 非交互 JSON 模式：stdin 读 prompt，stdout JSONL（message_end 权威文本）。
            let mut c = tokio::process::Command::from(crate::agent::shim_command(resolved));
            c.arg("-p")
                .arg("--mode")
                .arg("json")
                .arg("--session-id")
                .arg(uuid::Uuid::new_v4().to_string());
            // 桥内供应商 → --provider/--model（api key 走 env，与 agent.rs 同款）
            for a in extra_args {
                c.arg(a);
            }
            c
        }
    };
    #[cfg(windows)]
    {
        crate::deps::apply_no_window_tokio(&mut cmd);
    }
    cmd
}

/// 从模型 stdout 提取回复文本：
/// - pi：JSONL 里最后一条 message_end（assistant）的文本；
/// - codex（--json）：JSONL 里最后一条 item.completed（agent_message）的 text
///   （对齐 agent.rs process_line，避免把事件流当空输出误报）；
/// - claude：stdout 直接是文本。
fn extract_reply_text(backend: crate::agent::Backend, stdout: &str) -> String {
    match backend {
        crate::agent::Backend::Pi => {
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
        crate::agent::Backend::Codex => {
            let mut last = String::new();
            for line in stdout.lines() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    if v.get("type").and_then(|t| t.as_str()) == Some("item.completed") {
                        let item = &v["item"];
                        if item.get("type").and_then(|t| t.as_str()) == Some("agent_message") {
                            if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                                let t = t.trim();
                                if !t.is_empty() {
                                    last = t.to_string();
                                }
                            }
                        }
                    }
                }
            }
            last
        }
        crate::agent::Backend::Claude => stdout.to_string(),
    }
}

/// #123：生成失败错误文案——带模型 stdout 片段 + **stderr 透传**（失败原因可定位，
/// 不再只剩「空输出」这种无法归因的报错）。
fn generation_error(base: String, stdout: &str, stderr: &str) -> String {
    let mut msg = format!("{base}\n（模型原始输出片段：{}）", preview(stdout, 200));
    let err_txt = stderr.trim();
    if !err_txt.is_empty() {
        msg.push_str(&format!("\n（模型 stderr：{}）", preview(err_txt, 300)));
    }
    msg
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

    // ── #123 回归：teambuilder 生成命令参数对齐 agent.rs 主链路（100-A8 静态核对）──

    /// 取命令 argv（去掉程序名）。
    fn argv(c: &tokio::process::Command) -> Vec<String> {
        c.as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn codex_command_matches_agent_main_chain() {
        // 100-A8：codex 分支必须带 exec + --json + --skip-git-repo-check + workspace-write
        // 沙箱 + --add-dir(bridge_dir) + -c 供应商注入——与 agent.rs::codex_command 同源。
        let extra = vec!["model_provider=\"agent_bridge\"".to_string()];
        let roots = vec![std::path::PathBuf::from("C:\\Users\\x\\.agent-bridge")];
        let c = generation_command(
            crate::agent::Backend::Codex,
            std::path::Path::new("codex"),
            &extra,
            &roots,
        );
        let a = argv(&c);
        assert!(a.iter().any(|x| x == "exec"));
        assert!(a.iter().any(|x| x == "--json"));
        assert!(a.iter().any(|x| x == "--skip-git-repo-check"));
        assert!(a.iter().any(|x| x == "--sandbox"));
        assert!(a.iter().any(|x| x == "workspace-write"));
        let add_dirs: Vec<_> = a
            .windows(2)
            .filter(|w| w[0] == "--add-dir")
            .map(|w| w[1].clone())
            .collect();
        assert_eq!(add_dirs, vec!["C:\\Users\\x\\.agent-bridge"]);
        assert!(a.iter().any(|x| x == "-c"));
        assert!(a.iter().any(|x| x == "model_provider=\"agent_bridge\""));
        assert!(
            !a.iter()
                .any(|x| x == "--dangerously-bypass-approvals-and-sandbox"),
            "codex >= 0.146 走 workspace-write，不得回退全权限"
        );
    }

    #[test]
    fn codex_command_old_version_falls_back_to_bypass() {
        // codex < 0.146（无 --add-dir）：空 writable_roots → 回退 bypass（与 agent.rs 同款）。
        let c = generation_command(
            crate::agent::Backend::Codex,
            std::path::Path::new("codex"),
            &[],
            &[],
        );
        let a = argv(&c);
        assert!(a.iter().any(|x| x == "exec"));
        assert!(a.iter().any(|x| x == "--json"));
        assert!(a.iter().any(|x| x == "--skip-git-repo-check"));
        assert!(a
            .iter()
            .any(|x| x == "--dangerously-bypass-approvals-and-sandbox"));
        assert!(!a.iter().any(|x| x == "--sandbox"));
    }

    #[test]
    fn pi_command_appends_provider_args() {
        // 桥内供应商 → pi 追加 --provider/--model（与 agent.rs run_once 同款）。
        let extra = vec![
            "--provider".to_string(),
            "anthropic".to_string(),
            "--model".to_string(),
            "claude-x".to_string(),
        ];
        let c = generation_command(
            crate::agent::Backend::Pi,
            std::path::Path::new("pi"),
            &extra,
            &[],
        );
        let a = argv(&c);
        assert!(a.iter().any(|x| x == "-p"));
        assert!(a.iter().any(|x| x == "--mode"));
        assert!(a.iter().any(|x| x == "json"));
        assert!(a.iter().any(|x| x == "--provider"));
        assert!(a.iter().any(|x| x == "anthropic"));
        assert!(a.iter().any(|x| x == "--model"));
        assert!(a.iter().any(|x| x == "claude-x"));
    }

    #[test]
    fn claude_command_plain_text_mode() {
        let c = generation_command(
            crate::agent::Backend::Claude,
            std::path::Path::new("claude"),
            &[],
            &[],
        );
        let a = argv(&c);
        assert!(a.iter().any(|x| x == "-p"));
        assert!(a.iter().any(|x| x == "--output-format"));
        assert!(a.iter().any(|x| x == "text"));
    }

    #[test]
    fn extract_codex_jsonl_takes_last_agent_message() {
        // codex --json：thread/turn/reasoning 事件忽略，取最后一条 agent_message 文本。
        let out = r#"{"type":"thread.started","thread_id":"t-1"}
{"type":"turn.started"}
{"type":"item.completed","item":{"type":"reasoning","summary":["想"]}}
{"type":"item.completed","item":{"type":"agent_message","text":"{\"team_name\":\"记账App团队\",\"roles\":[]}"}}
{"type":"item.completed","item":{"type":"agent_message","text":"{\"team_name\":\"记账App团队\",\"roles\":[{\"role_name\":\"产品经理\"}]}"}}"#;
        let raw = extract_reply_text(crate::agent::Backend::Codex, out);
        assert_eq!(
            raw,
            "{\"team_name\":\"记账App团队\",\"roles\":[{\"role_name\":\"产品经理\"}]}"
        );
    }

    #[test]
    fn extract_codex_jsonl_empty_when_no_message() {
        // 只有事件流、无 agent_message（如 codex 报错被 stderr 承接）→ 空文本。
        let out = r#"{"type":"thread.started","thread_id":"t-1"}
{"type":"turn.started"}"#;
        assert_eq!(extract_reply_text(crate::agent::Backend::Codex, out), "");
    }

    #[test]
    fn generation_error_surfaces_stderr() {
        // #123 验收 100-A7：失败原因可定位——错误信息含模型实际 stderr。
        let e = generation_error(
            "未能在模型输出中找到 JSON 团队方案".to_string(),
            "",
            "Not inside a trusted directory and --skip-git-repo-check was not specified.",
        );
        assert!(e.contains("模型 stderr"));
        assert!(e.contains("Not inside a trusted directory"));
        // stderr 为空时不追加空段落
        let e2 = generation_error("解析失败".to_string(), "", "   \n ");
        assert!(!e2.contains("模型 stderr"));
    }
}
