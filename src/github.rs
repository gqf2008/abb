//! GitHub 能力（附挂到既有 IM bot 上，**不是**新 bot kind）。
//! 两个方向：
//! 1) IM → GitHub 指令（人触发）：分析/关闭/创建 issue，仓库白名单把关；分析走 agent，
//!    最终回复双写（issue 评论留档全文 + 群里截断摘要）。
//! 2) GitHub → IM 通知（watch 循环）：轮询白名单仓库新 issue 只通知不处理（Phase 2 再接 agent）。
//!
//! 零 regex——URL/命令手工字符解析（与 attachments::extract_urls / schedule::CronExpr 同款风格）。
//!
//! 用途两类：桥的 IM→GitHub 指令门与 service 的 GitHub→IM watch 通知循环。

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// GitHub issue（REST 响应子集）。字段全默认兜底：API 结构演进不炸解析。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct GhIssue {
    /// GitHub 全局 issue id（seen 去重用）。
    pub id: u64,
    pub number: u64,
    pub title: String,
    /// open | closed
    pub state: String,
    pub html_url: String,
    pub body: String,
    /// RFC3339 UTC（字典序 == 时间序，游标比较直接用字符串比）。
    pub created_at: String,
    pub updated_at: String,
    pub user: GhUser,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct GhUser {
    pub login: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct GhComment {
    pub body: String,
    pub user: GhUser,
}

/// IM → GitHub 指令的解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GhCmd {
    /// 分析：拉 issue+评论注入 prompt，agent 分析完双写（评论留档 + 群摘要）。
    Analyze { owner: String, repo: String, number: u64 },
    /// 直接关 issue（API 操作，不进 agent）。
    Close { owner: String, repo: String, number: u64 },
    /// 直接建 issue。owner/repo 为空 = 指令没带仓库，由桥按白名单解析。
    Create { owner: String, repo: String, title: String },
}

/// URL 边界字符（与 attachments::extract_urls 的终止集同款，中英标点都算）。
const URL_TERMINATORS: &[char] = &[
    ' ', '\t', '\n', '\r', '<', '>', '"', '\'', '`', '（', '）', '，', '。', '；', '：', '、',
    '！', '？', '【', '】', '《', '》', '“', '”', '‘', '’', '…',
];

/// 解析 github 指令。优先级：关闭 > 创建 > 分析。
/// 分析必须带 issue 链接——无链接的「分析/看看/处理」是普通消息，透传给 agent（透明）。
pub fn parse_github_cmd(text: &str) -> Option<GhCmd> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    // 关闭：动词 + issue 链接
    if contains_verb(t, &["关闭", "close"]) {
        if let Some((o, r, n)) = parse_issue_url(t) {
            return Some(GhCmd::Close { owner: o, repo: r, number: n });
        }
    }
    // 创建：动词短语 + 标题（可带 owner/repo 前缀）
    if let Some(after) = after_create_verb(t) {
        let rest = after.trim();
        if rest.is_empty() {
            return None;
        }
        let (owner, repo, title) = match extract_repo_ref(t) {
            // 仓库引用可能在动词短语之前（「在 o/r 建 issue 标题」）；标题只从短语后取，
            // 去掉标题里残留的仓库引用与「在」字。
            Some((o, r)) => {
                let t2 = rest.replace(&format!("{o}/{r}"), "").replace('在', "").trim().to_string();
                (o, r, t2)
            }
            None => (String::new(), String::new(), rest.to_string()),
        };
        if title.is_empty() {
            return None;
        }
        return Some(GhCmd::Create { owner, repo, title });
    }
    // 分析：动词 + issue 链接
    if contains_verb(t, &["分析", "看看", "处理", "analyze"]) {
        if let Some((o, r, n)) = parse_issue_url(t) {
            return Some(GhCmd::Analyze { owner: o, repo: r, number: n });
        }
    }
    None
}

/// 子串匹配：中文动词直接子串；ASCII 动词按词边界（避免 closure/processor 误触发）。
fn contains_verb(text: &str, verbs: &[&str]) -> bool {
    let lower = text.to_ascii_lowercase();
    verbs.iter().any(|v| {
        if v.chars().all(|c| c.is_ascii_alphabetic()) {
            let lv = v.to_ascii_lowercase();
            let mut start = 0;
            while let Some(idx) = lower[start..].find(&lv) {
                let i = start + idx;
                let before = i == 0
                    || !lower[..i].chars().next_back().map(|c| c.is_ascii_alphanumeric()).unwrap_or(false);
                let end = i + lv.len();
                let after = end >= lower.len()
                    || !lower[end..].chars().next().map(|c| c.is_ascii_alphanumeric()).unwrap_or(false);
                if before && after {
                    return true;
                }
                start = i + 1;
            }
            false
        } else {
            text.contains(v)
        }
    })
}

/// 找创建动词短语（建 issue / 建issue / 创建 issue / 创建issue / create issue，大小写不敏感），
/// 返回其后剩余文本。多个短语命中时取结束位置最靠后的（"创建 issue" 覆盖 "建 issue"）。
fn after_create_verb(text: &str) -> Option<&str> {
    let lower = text.to_ascii_lowercase();
    let mut best_end = 0usize;
    let mut found = false;
    for v in ["创建 issue", "创建issue", "建 issue", "建issue", "create issue", "createissue"] {
        if let Some(i) = lower.find(&v.to_ascii_lowercase()) {
            let end = i + v.len();
            if end > best_end {
                best_end = end;
                found = true;
            }
        }
    }
    found.then(|| &text[best_end.min(text.len())..])
}

/// 从文本提取 GitHub issue 三元组 (owner, repo, number)。
/// 形态一：github.com/owner/repo/issues/N（主机名大小写不敏感）；
/// 形态二：owner/repo#N。
/// 数字段容忍尾部脏字符（?utm=…、标点）。无链接返回 None。
pub fn parse_issue_url(text: &str) -> Option<(String, String, u64)> {
    let chars: Vec<char> = text.chars().collect();
    // 形态一：先找 "github.com/"（大小写不敏感）
    let host = "github.com/";
    for start in 0..chars.len() {
        if chars[start] != 'g' && chars[start] != 'G' {
            continue;
        }
        if chars.len() - start < host.len() {
            continue;
        }
        if chars[start..start + host.len()]
            .iter()
            .map(|c| c.to_ascii_lowercase())
            .eq(host.chars().map(|c| c.to_ascii_lowercase()))
        {
            let mut j = start + host.len();
            while j < chars.len() && !URL_TERMINATORS.contains(&chars[j]) {
                j += 1;
            }
            let path: String = chars[start + host.len()..j].iter().collect();
            let path = path.trim_end_matches(['.', ',', ')', ']', '}']);
            let mut parts = path.split('/');
            let owner = parts.next().filter(|s| !s.is_empty())?.to_string();
            let repo = parts.next().filter(|s| !s.is_empty())?.to_string();
            if parts.next()? != "issues" {
                return None;
            }
            // 数字段只取前导数字：容忍 ?foo / 尾部标点
            let num: u64 = parts.next()?.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()?;
            if parts.next().is_some() {
                return None;
            }
            return Some((owner, repo, num));
        }
    }
    // 形态二：找 '#'，往前扫 owner/repo
    for (i, c) in chars.iter().enumerate() {
        if *c != '#' {
            continue;
        }
        let mut num_end = i + 1;
        while num_end < chars.len() && chars[num_end].is_ascii_digit() {
            num_end += 1;
        }
        if num_end == i + 1 {
            continue; // '#' 后无数字
        }
        // 数字后必须是非字母数字边界（防嵌在英文单词里：o/r#5yyy）
        if num_end < chars.len() && chars[num_end].is_alphanumeric() {
            continue;
        }
        let Ok(num) = chars[i + 1..num_end].iter().collect::<String>().parse::<u64>() else {
            continue;
        };
        let mut j = i;
        while j > 0 && chars[j - 1] != '/' {
            j -= 1;
        }
        if j == 0 {
            continue;
        }
        let repo: String = chars[j..i].iter().collect();
        let mut k = j - 1;
        while k > 0 && chars[k - 1] != '/' && !URL_TERMINATORS.contains(&chars[k - 1]) {
            k -= 1;
        }
        let owner: String = chars[k..j - 1].iter().collect();
        if owner.is_empty() || repo.is_empty() {
            continue;
        }
        // owner 前不得紧贴字母数字（避免嵌在英文单词里）
        if k > 0 && chars[k - 1].is_alphanumeric() {
            continue;
        }
        return Some((owner, repo, num));
    }
    None
}

fn is_repo_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
}

/// 从文本提取裸 `owner/repo`（创建指令带仓库时用）。拒绝 URL 内的（前邻 ':' 或 '/'）。
pub fn extract_repo_ref(text: &str) -> Option<(String, String)> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if !is_repo_char(chars[i]) {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < chars.len() && is_repo_char(chars[j]) {
            j += 1;
        }
        if j < chars.len() && chars[j] == '/' {
            let mut k = j + 1;
            while k < chars.len() && is_repo_char(chars[k]) {
                k += 1;
            }
            if k > j + 1 {
                let prev_ok = i == 0
                    || (chars[i - 1] != '/' && chars[i - 1] != ':' && !chars[i - 1].is_alphanumeric());
                if prev_ok {
                    return Some((chars[i..j].iter().collect(), chars[j + 1..k].iter().collect()));
                }
            }
        }
        i = j.max(i + 1);
    }
    None
}

/// 仓库白名单（逗号/分号/空白分隔，同 is_owner_allowed 解析风格）：精确 `owner/repo` 或
/// `owner/*`（整个组织通配）。空白名单 = 全部放行（用户没配时别锁死功能）。
pub fn repo_in_whitelist(repo: &str, whitelist: &str) -> bool {
    let owner = repo.split('/').next().unwrap_or("");
    let entries: Vec<&str> = whitelist
        .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if entries.is_empty() {
        return true; // 空白名单 = 全部放行（用户没配时别锁死功能）
    }
    entries.iter().any(|w| {
        *w == repo || (w.ends_with("/*") && w[..w.len() - 2].eq_ignore_ascii_case(owner))
    })
}

/// 分析指令的注入上下文：issue 内容预渲染进 prompt，同时携带双写目标。
#[derive(Debug, Clone)]
pub struct GhContext {
    pub owner: String,
    pub repo: String,
    pub number: u64,
    /// 群摘要标题。
    pub title: String,
    /// 注入 prompt 的 [GitHub Issue] 段。
    pub render: String,
}

impl GhContext {
    // body 8000 字 / 评论 30 条 × 1000 字：prompt 有界，防超长 issue 撑爆上下文
    const BODY_MAX: usize = 8000;
    const COMMENTS_MAX: usize = 30;
    const COMMENT_LEN: usize = 1000;

    pub fn new(owner: String, repo: String, number: u64, issue: GhIssue, comments: Vec<GhComment>) -> GhContext {
        let title = issue.title.clone();
        let mut render = format!(
            "#{number} {title}\n链接: {url}\n作者: @{login}\n状态: {state}\n\n{body}",
            url = issue.html_url,
            login = issue.user.login,
            state = issue.state,
            body = crate::agent::truncate(&issue.body, Self::BODY_MAX)
        );
        if !comments.is_empty() {
            render.push_str("\n\n[评论]");
            for c in comments.iter().take(Self::COMMENTS_MAX) {
                render.push('\n');
                render.push_str(&format!(
                    "- @{}: {}",
                    c.user.login,
                    crate::agent::truncate(&c.body, Self::COMMENT_LEN)
                ));
            }
            if comments.len() > Self::COMMENTS_MAX {
                render.push_str(&format!(
                    "\n…（还有 {} 条评论未展示）",
                    comments.len() - Self::COMMENTS_MAX
                ));
            }
        }
        GhContext { owner, repo, number, title, render }
    }
}

/// 通知文案：「🔔 新 issue #N (title) by @author — url」
pub fn notify_text(repo: &str, iss: &GhIssue) -> String {
    format!(
        "🔔 新 issue #{} {} by @{}\nhttps://github.com/{}/issues/{}",
        iss.number, iss.title, iss.user.login, repo, iss.number
    )
}

/// 纯函数（可测）：从一页增量结果挑「新 issue」+ 计算新游标。
/// - since 为空（首轮/游标丢失）→ 只推进游标不通知（静默基线，避免首刷把存量 open issues 全发一遍）；
/// - 过滤 seen 里已有的 issue 全局 id（同秒多条/时间戳回拨防重）；
/// - 过滤 created_at < since 的（旧 issue 被评论/编辑刷出增量窗口，不算新 issue）；
/// - 过滤 echo_login 自己发的（自问自答不回显）。
///
/// 游标 = 列表中 updated_at 最大者（API 升序返回，即最后一个）；空列表保持原游标。
pub fn new_issues(issues: &[GhIssue], since: &str, seen: &[u64], echo_login: &str) -> (Vec<GhIssue>, String) {
    let mut fresh = Vec::new();
    for iss in issues {
        if seen.contains(&iss.id) {
            continue;
        }
        if !echo_login.is_empty() && iss.user.login == echo_login {
            continue;
        }
        // created_at 与 since 均为 RFC3339 UTC：字典序 = 时间序
        if !since.is_empty() && iss.created_at.as_str() < since {
            continue;
        }
        fresh.push(iss.clone());
    }
    let new_since = issues.last().map(|i| i.updated_at.clone()).unwrap_or_else(|| since.to_string());
    if since.is_empty() {
        fresh.clear(); // 静默基线
    }
    (fresh, new_since)
}

/// 每仓库的增量游标（watch 循环持久化）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoCursor {
    /// updated_at 游标（RFC3339）。
    pub since: String,
    /// 已通知的 issue 全局 id（上限 2000，超裁头）。
    pub seen: Vec<u64>,
}

/// watch 循环游标：按仓库分组，落盘 workspaces/<bot>/github_cursor.json。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GhCursor {
    pub by_repo: std::collections::BTreeMap<String, RepoCursor>,
}

impl GhCursor {
    /// 工作区加载；不存在/损坏 → 默认（首轮即静默基线）。与 PendingStore 同款容错。
    pub fn load(bot_key: &str) -> GhCursor {
        let path = crate::workspace_dir(bot_key).join("github_cursor.json");
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, bot_key: &str) {
        let dir = crate::workspace_dir(bot_key);
        if std::fs::create_dir_all(&dir).is_ok() {
            if let Ok(text) = serde_json::to_string(self) {
                let _ = crate::atomic_write_text(&dir.join("github_cursor.json"), &text);
            }
        }
    }

    pub fn repo_cursor(&self, repo: &str) -> RepoCursor {
        self.by_repo.get(repo).cloned().unwrap_or_default()
    }

    pub fn update(&mut self, repo: &str, since: &str, seen_extra: Vec<u64>) {
        let cur = self.by_repo.entry(repo.to_string()).or_default();
        cur.since = since.to_string();
        for id in seen_extra {
            if !cur.seen.contains(&id) {
                cur.seen.push(id);
            }
        }
        if cur.seen.len() > 2000 {
            cur.seen.drain(..cur.seen.len() - 2000);
        }
    }
}

/// GitHub API 抽象（仿 Messenger/AgentRunner 的 trait 注入）：桥与 watch 循环只依赖此 trait，
/// 生产 = GithubClient（reqwest），测试 = MockGithub。
#[async_trait::async_trait]
pub trait GithubApi: Send + Sync {
    async fn fetch_issue(&self, owner: &str, repo: &str, number: u64) -> anyhow::Result<GhIssue>;
    async fn list_comments(&self, owner: &str, repo: &str, number: u64) -> anyhow::Result<Vec<GhComment>>;
    async fn post_comment(&self, owner: &str, repo: &str, number: u64, body: &str) -> anyhow::Result<()>;
    async fn close_issue(&self, owner: &str, repo: &str, number: u64) -> anyhow::Result<()>;
    /// 创建 issue，返回 html_url（回复带链接）。
    async fn create_issue(&self, owner: &str, repo: &str, title: &str) -> anyhow::Result<String>;
    /// 增量拉取 open issues：since=updated_at 游标（RFC3339），sort=updated&direction=asc。
    async fn list_issues_since(&self, owner: &str, repo: &str, since: &str) -> anyhow::Result<Vec<GhIssue>>;
}

pub struct GithubClient {
    http: reqwest::Client,
    token: String,
}

impl GithubClient {
    pub fn new(token: &str) -> GithubClient {
        // rustls 静态证书（与 feishu/wechat 同款 builder）：本机无系统 CA 也能 HTTPS
        let http = reqwest::Client::builder()
            .user_agent("agent-bridge")
            .build()
            .expect("reqwest client");
        GithubClient { http, token: token.trim().to_string() }
    }

    fn authed(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let b = self.http.request(method, format!("https://api.github.com{path}"));
        if self.token.is_empty() {
            b
        } else {
            b.header(reqwest::header::AUTHORIZATION, format!("Bearer {}", self.token))
        }
    }

    /// 统一响应处理（wechat get_updates 同款「先取文本再解析」）：非 2xx → Err 带 status+预览。
    /// 顺带读 X-RateLimit-Remaining：剩余 ≤ 10 打告警日志（限流意识，防 403 断供）。
    async fn send(&self, rb: reqwest::RequestBuilder) -> anyhow::Result<serde_json::Value> {
        let resp = rb.send().await.context("github 网络错误")?;
        if let Some(v) = resp
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<u32>().ok())
        {
            if v <= 10 {
                crate::log!("[github] ⚠️ 剩余 API 配额 {v} 次，注意限流");
            }
        }
        let status = resp.status();
        let txt = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("github {status} 响应: {}", crate::agent::truncate(&txt, 300));
        }
        serde_json::from_str(&txt).context("github 响应非 JSON")
    }
}

#[async_trait::async_trait]
impl GithubApi for GithubClient {
    async fn fetch_issue(&self, owner: &str, repo: &str, number: u64) -> anyhow::Result<GhIssue> {
        let v = self
            .send(self.authed(reqwest::Method::GET, &format!("/repos/{owner}/{repo}/issues/{number}")))
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    async fn list_comments(&self, owner: &str, repo: &str, number: u64) -> anyhow::Result<Vec<GhComment>> {
        // per_page=100：超过 100 条截断（评论区极少达到，且 prompt 只取 30 条，见 GhContext）
        let v = self
            .send(self.authed(
                reqwest::Method::GET,
                &format!("/repos/{owner}/{repo}/issues/{number}/comments?per_page=100"),
            ))
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    async fn post_comment(&self, owner: &str, repo: &str, number: u64, body: &str) -> anyhow::Result<()> {
        self.send(
            self.authed(reqwest::Method::POST, &format!("/repos/{owner}/{repo}/issues/{number}/comments"))
                .json(&serde_json::json!({"body": body})),
        )
        .await?;
        Ok(())
    }

    async fn close_issue(&self, owner: &str, repo: &str, number: u64) -> anyhow::Result<()> {
        self.send(
            self.authed(reqwest::Method::PATCH, &format!("/repos/{owner}/{repo}/issues/{number}"))
                .json(&serde_json::json!({"state": "closed"})),
        )
        .await?;
        Ok(())
    }

    async fn create_issue(&self, owner: &str, repo: &str, title: &str) -> anyhow::Result<String> {
        let v = self
            .send(
                self.authed(reqwest::Method::POST, &format!("/repos/{owner}/{repo}/issues"))
                    .json(&serde_json::json!({"title": title})),
            )
            .await?;
        v["html_url"]
            .as_str()
            .map(|s| s.to_string())
            .context("github 创建 issue 响应缺 html_url")
    }

    async fn list_issues_since(&self, owner: &str, repo: &str, since: &str) -> anyhow::Result<Vec<GhIssue>> {
        // since 按 updated_at 过滤；升序 → 游标 = 列表最后一跳的 updated_at。
        // since 值经 percent_encode_query 编码（RFC3339 含 ':' 不能裸进 query）。
        let mut path = format!("/repos/{owner}/{repo}/issues?state=open&sort=updated&direction=asc&per_page=100");
        if !since.is_empty() {
            path.push_str("&since=");
            path.push_str(&crate::wechat::percent_encode_query(since));
        }
        let v = self.send(self.authed(reqwest::Method::GET, &path)).await?;
        Ok(serde_json::from_value(v)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISSUE_FIXTURE: &str = r#"{
        "id": 123456789,
        "number": 42,
        "title": "登录偶发 401",
        "state": "open",
        "html_url": "https://github.com/o/r/issues/42",
        "body": "token 缓存竞态导致偶发 401。",
        "created_at": "2026-08-14T03:00:00Z",
        "updated_at": "2026-08-14T04:00:00Z",
        "user": {"login": "alice"}
    }"#;

    const COMMENTS_FIXTURE: &str = r#"[
        {"body": "复现了，见日志。", "user": {"login": "bob"}},
        {"body": "建议加锁。", "user": {"login": "alice"}}
    ]"#;

    fn issue(id: u64, number: u64, login: &str, created: &str, updated: &str) -> GhIssue {
        GhIssue {
            id,
            number,
            title: format!("issue {number}"),
            state: "open".into(),
            html_url: format!("https://github.com/o/r/issues/{number}"),
            body: "body".into(),
            created_at: created.into(),
            updated_at: updated.into(),
            user: GhUser { login: login.into() },
        }
    }

    #[test]
    fn parse_issue_url_full_and_shorthand() {
        // 完整 URL 形态
        assert_eq!(
            parse_issue_url("分析下 https://github.com/o/r/issues/12 这个"),
            Some(("o".into(), "r".into(), 12))
        );
        // 主机大小写不敏感
        assert_eq!(
            parse_issue_url("GitHub.com/O/R/issues/5"),
            Some(("O".into(), "R".into(), 5))
        );
        // 尾部脏字符：标点 + 查询串
        assert_eq!(
            parse_issue_url("看 https://github.com/o/r/issues/12?utm=x），谢谢"),
            Some(("o".into(), "r".into(), 12))
        );
        // 简写 owner/repo#N
        assert_eq!(
            parse_issue_url("o/r#5 这个 bug"),
            Some(("o".into(), "r".into(), 5))
        );
        // 简写嵌在中文里（前有标点边界）
        assert_eq!(
            parse_issue_url("（o/r#5）"),
            Some(("o".into(), "r".into(), 5))
        );
    }

    #[test]
    fn parse_issue_url_negatives() {
        assert_eq!(parse_issue_url("今天天气不错"), None);
        assert_eq!(parse_issue_url("https://github.com/o/r/pulls/1"), None);
        assert_eq!(parse_issue_url("https://github.com/a/b/c/issues/1"), None);
        assert_eq!(parse_issue_url("https://github.com/x"), None);
        assert_eq!(parse_issue_url("o/r#"), None);
        // 简写不得嵌在英文单词里
        assert_eq!(parse_issue_url("xxxo/r#5yyy"), None);
    }

    #[test]
    fn parse_github_cmd_analyze_close_create() {
        // 分析：中英动词 × 链接形态
        assert_eq!(
            parse_github_cmd("分析 https://github.com/o/r/issues/3"),
            Some(GhCmd::Analyze { owner: "o".into(), repo: "r".into(), number: 3 })
        );
        assert_eq!(
            parse_github_cmd("看看 o/r#3"),
            Some(GhCmd::Analyze { owner: "o".into(), repo: "r".into(), number: 3 })
        );
        assert_eq!(
            parse_github_cmd("处理一下 github.com/o/r/issues/3 的问题"),
            Some(GhCmd::Analyze { owner: "o".into(), repo: "r".into(), number: 3 })
        );
        assert_eq!(
            parse_github_cmd("analyze https://github.com/o/r/issues/3"),
            Some(GhCmd::Analyze { owner: "o".into(), repo: "r".into(), number: 3 })
        );
        // 关闭
        assert_eq!(
            parse_github_cmd("关闭 https://github.com/o/r/issues/7"),
            Some(GhCmd::Close { owner: "o".into(), repo: "r".into(), number: 7 })
        );
        assert_eq!(
            parse_github_cmd("close o/r#7"),
            Some(GhCmd::Close { owner: "o".into(), repo: "r".into(), number: 7 })
        );
        // 创建：无仓库（由桥按白名单解析）
        assert_eq!(
            parse_github_cmd("建 issue 修复登录 401"),
            Some(GhCmd::Create { owner: String::new(), repo: String::new(), title: "修复登录 401".into() })
        );
        // 创建：带仓库，标题去掉仓库引用与「在」
        assert_eq!(
            parse_github_cmd("在 o/r 建 issue 修复 bug"),
            Some(GhCmd::Create { owner: "o".into(), repo: "r".into(), title: "修复 bug".into() })
        );
        assert_eq!(
            parse_github_cmd("create issue o/r 新增文档"),
            Some(GhCmd::Create { owner: "o".into(), repo: "r".into(), title: "新增文档".into() })
        );
        // 优先级：关闭 > 创建 > 分析（同一句含多动词时关闭优先）
        assert_eq!(
            parse_github_cmd("关闭 https://github.com/o/r/issues/7 再分析"),
            Some(GhCmd::Close { owner: "o".into(), repo: "r".into(), number: 7 })
        );
    }

    #[test]
    fn parse_github_cmd_without_link_is_none() {
        // 无链接的分析类动词 → 透传 agent（透明），不误拦
        assert_eq!(parse_github_cmd("分析今天天气"), None);
        assert_eq!(parse_github_cmd("处理一下"), None);
        assert_eq!(parse_github_cmd("看看这个文件"), None);
        // 关闭无链接也透传（可能是人话「关闭窗口」）
        assert_eq!(parse_github_cmd("关闭窗口"), None);
    }

    #[test]
    fn repo_in_whitelist_rules() {
        // 精确匹配
        assert!(repo_in_whitelist("o/r", "o/r"));
        // 组织通配 owner/*
        assert!(repo_in_whitelist("o/anything", "o/*"));
        assert!(!repo_in_whitelist("other/r", "o/*"));
        // 分隔符：逗号/空白
        assert!(repo_in_whitelist("a/b", "a/b c/d"));
        assert!(repo_in_whitelist("c/d", "a/b, c/d"));
        // 空白名单 = 全放行
        assert!(repo_in_whitelist("anything/x", ""));
        // 拒绝
        assert!(!repo_in_whitelist("o/r", "o/x"));
        assert!(!repo_in_whitelist("o/r", "x/*"));
    }

    #[test]
    fn new_issues_baseline_silent_advances_cursor() {
        let iss = vec![issue(1, 1, "alice", "2026-08-14T01:00:00Z", "2026-08-14T02:00:00Z")];
        // 空 since（首轮）：静默基线——不通知但游标推进
        let (fresh, since) = new_issues(&iss, "", &[], "");
        assert!(fresh.is_empty());
        assert_eq!(since, "2026-08-14T02:00:00Z");
        // 下一轮：基线之后**新建**的 issue 才通知（created_at >= since）
        let iss2 = vec![issue(2, 2, "bob", "2026-08-14T02:30:00Z", "2026-08-14T03:00:00Z")];
        let (fresh, since) = new_issues(&iss2, "2026-08-14T02:00:00Z", &[], "");
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].id, 2);
        assert_eq!(since, "2026-08-14T03:00:00Z");
    }

    #[test]
    fn new_issues_filters_seen_echo_and_old_updates() {
        let iss = vec![
            // 已在 seen 里（通知过）
            issue(1, 1, "alice", "2026-08-14T02:30:00Z", "2026-08-14T03:00:00Z"),
            // echo_login 自己发的
            issue(2, 2, "bot", "2026-08-14T03:30:00Z", "2026-08-14T04:00:00Z"),
            // 老 issue 被评论刷进增量窗口（created < since，updated 新）→ 不算新 issue
            issue(3, 3, "bob", "2026-08-14T00:30:00Z", "2026-08-14T04:30:00Z"),
            // 真正的新 issue
            issue(4, 4, "carol", "2026-08-14T03:10:00Z", "2026-08-14T03:20:00Z"),
        ];
        let (fresh, _) = new_issues(&iss, "2026-08-14T03:00:00Z", &[1], "bot");
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].id, 4);
    }

    #[test]
    fn ghcursor_roundtrip_persists() {
        let key = format!("abb-test-{}", uuid::Uuid::new_v4());
        let dir = crate::workspace_dir(&key);
        let _ = std::fs::remove_dir_all(&dir);
        // 缺失文件 → 默认
        let c = GhCursor::load(&key);
        assert!(c.by_repo.is_empty());
        // 写入 → 读回
        let mut c = c;
        c.update("o/r", "2026-08-14T05:00:00Z", vec![1, 2]);
        c.save(&key);
        let c2 = GhCursor::load(&key);
        assert_eq!(c2.repo_cursor("o/r").since, "2026-08-14T05:00:00Z");
        assert_eq!(c2.repo_cursor("o/r").seen, vec![1, 2]);
        // 清理
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_fixture_parses_and_notify_text() {
        let iss: GhIssue = serde_json::from_str(ISSUE_FIXTURE).unwrap();
        assert_eq!(iss.number, 42);
        assert_eq!(iss.user.login, "alice");
        let comments: Vec<GhComment> = serde_json::from_str(COMMENTS_FIXTURE).unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[1].user.login, "alice");
        assert_eq!(
            notify_text("o/r", &iss),
            "🔔 新 issue #42 登录偶发 401 by @alice\nhttps://github.com/o/r/issues/42"
        );
    }

    #[test]
    fn gh_context_bounds_render() {
        let iss: GhIssue = serde_json::from_str(ISSUE_FIXTURE).unwrap();
        let comments: Vec<GhComment> = serde_json::from_str(COMMENTS_FIXTURE).unwrap();
        let ctx = GhContext::new("o".into(), "r".into(), 42, iss, comments);
        assert!(ctx.render.contains("登录偶发 401"));
        assert!(ctx.render.contains("@alice"));
        assert!(ctx.render.contains("[评论]"));
        assert!(ctx.render.contains("@bob: 复现了，见日志。"));
    }
}
