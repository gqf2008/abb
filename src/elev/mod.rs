//! Windows 提权出口（A1 第一批，线程 `abb-default-admin-elevation-20260921`）。
//!
//! 背景：ABB 以 asInvoker 运行，写 `HKLM\...\Winlogon` 需要管理员权限。这里给出的
//! 是一条**具名白名单**的提权出口：用到时才用 `ShellExecuteExW "runas"` 拉起短命
//! helper `abb-elev-helper`，干完即退；**表外操作在触碰任何系统调用之前就被拒绝**，
//! 因此它不会退化成「一个通用管理员 shell」。
//!
//! 本模块**平台无关**（纯逻辑 + 单测）：白名单表、参数校验、审计记录与脱敏、
//! 返回契约/错误码、令牌校验、请求编解码与分发、密码零化。真正碰系统的部分
//! （命名管道 / 注册表 / runas）在 [`win`]（`#[cfg(windows)]`）。
//!
//! # 安全不变量
//! 1. 密码只在内存里传递：校验 → 交给后端 → 立刻 [`wipe_bytes`]，**不进日志、
//!    不进审计、不进错误消息、不落盘**；错误消息一律不回显原始参数值。
//! 2. 审计写失败 ⇒ 不执行（`audit_failed`）；结果审计写失败 ⇒ **不回成功**
//!    （三个操作都幂等，调用方可安全重试）。
//! 3. 未鉴权请求（令牌不匹配）先记审计再拒绝，且不透露白名单、不碰系统调用。
//! 4. 返回给调用方的只有「错误码 + 人话原因」；系统 API 的原始错误串只进 helper
//!    自己的内部日志（stderr），绝不回给调用方。
//!
//! # 已知残余风险（本批不解决，记录在案）
//! - 令牌经 helper 命令行 `--token` 传递：提权进程的命令行同用户可读，因此令牌只
//!   能防**跨用户**冒用、并抬高同用户冒用的成本，不能替代管道 DACL。
//! - 我们只能保证**自己持有**的副本被清零：[`wipe_bytes`] 覆盖请求体与密码缓冲；
//!   `serde_json` 解析器内部的临时缓冲（转义字符串时才用到）不受我们控制。
//! - 调用方进程自己的密码副本（以及将来把密码投递到这条通道的上游）不在本批范围。

use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(target_os = "windows")]
pub mod win;

pub mod codes {
    //! 返回契约里的**固定**错误码集（表外名字不得新增）。

    /// 成功（不是错误码，仅用于响应/审计的 `code` 字段）。
    pub const OK: &str = "ok";
    /// 操作名不在白名单内。
    pub const INVALID_OP: &str = "invalid_op";
    /// 参数校验失败（含请求体格式非法）。
    pub const BAD_PARAMS: &str = "bad_params";
    /// 令牌不匹配（未鉴权请求）。
    pub const UNAUTHORIZED: &str = "unauthorized";
    /// 无权限执行（helper 未提权等）。
    pub const DENIED: &str = "denied";
    /// 系统调用失败。
    pub const SYSCALL_FAILED: &str = "syscall_failed";
    /// 超时。
    pub const TIMEOUT: &str = "timeout";
    /// 审计写入失败。
    pub const AUDIT_FAILED: &str = "audit_failed";

    /// 全量错误码（单测据此断言「表是封闭的」）。
    pub const ALL: [&str; 7] = [
        INVALID_OP,
        BAD_PARAMS,
        UNAUTHORIZED,
        DENIED,
        SYSCALL_FAILED,
        TIMEOUT,
        AUDIT_FAILED,
    ];
}

// ─────────────────────────── 白名单 ───────────────────────────

/// 操作：`set-auto-login`。
pub const OP_SET_AUTO_LOGIN: &str = "set-auto-login";
/// 操作：`clear-auto-login`。
pub const OP_CLEAR_AUTO_LOGIN: &str = "clear-auto-login";
/// 操作：`get-auto-login`。
pub const OP_GET_AUTO_LOGIN: &str = "get-auto-login";
/// 审计里给「解析不出 op」的请求用的占位名。
pub const OP_UNKNOWN: &str = "<unknown>";

/// 提权出口允许执行的**全部**操作。表外一律 `invalid_op`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// 写 `AutoAdminLogon` / `DefaultUserName` / `DefaultPassword`（三值一起写）。
    SetAutoLogin,
    /// 清除上述三值。
    ClearAutoLogin,
    /// 只读：返回 `enabled` + `username`（**永不返回密码**）。
    GetAutoLogin,
}

impl Op {
    /// 全部白名单操作（遍历用）。
    pub const ALL: [Op; 3] = [Op::SetAutoLogin, Op::ClearAutoLogin, Op::GetAutoLogin];

    /// 线上名字（协议里就是它）。
    pub fn name(self) -> &'static str {
        match self {
            Op::SetAutoLogin => OP_SET_AUTO_LOGIN,
            Op::ClearAutoLogin => OP_CLEAR_AUTO_LOGIN,
            Op::GetAutoLogin => OP_GET_AUTO_LOGIN,
        }
    }

    /// 严格解析：只认精确的小写连字符形式。大小写变体、前后空白、空串一律 `None`
    /// ——白名单匹配不做任何「宽容」，宽一点就是一个绕过面。
    pub fn from_name(name: &str) -> Option<Op> {
        match name {
            "set-auto-login" => Some(Op::SetAutoLogin),
            "clear-auto-login" => Some(Op::ClearAutoLogin),
            "get-auto-login" => Some(Op::GetAutoLogin),
            _ => None,
        }
    }
}

// ─────────────────────────── 限额 ───────────────────────────

/// 用户名长度上限（字符数）。
pub const MAX_USERNAME_CHARS: usize = 256;
/// 密码长度上限（字符数）。
pub const MAX_PASSWORD_CHARS: usize = 512;
/// 单个请求体上限（字节）。
pub const MAX_BODY: usize = 64 * 1024;
/// helper 兜底超时：到点还没处理完就自杀，绝不常驻。
pub const HELPER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// 审计里给「解析不出 op」的记录用的名字截断长度。
const OP_LABEL_MAX_CHARS: usize = 64;

// ─────────────────────────── 参数校验（strict） ───────────────────────────

/// Windows 账户名禁用字符（控制字符单独判，它不止这些）。
const USERNAME_BANNED: [char; 15] = [
    '\\', '/', '[', ']', ':', ';', '|', '=', ',', '+', '*', '?', '<', '>', '"',
];

/// 用户名校验。失败原因**只描述规则，不回显值**。
pub fn validate_username(username: &str) -> Result<(), &'static str> {
    if username.is_empty() {
        return Err("用户名不能为空");
    }
    if username.chars().count() > MAX_USERNAME_CHARS {
        return Err("用户名过长（上限 256 字符）");
    }
    if username.chars().any(char::is_control) {
        return Err("用户名含控制字符");
    }
    if username.chars().any(|c| USERNAME_BANNED.contains(&c)) {
        return Err("用户名含 Windows 账户名禁用字符");
    }
    if username.ends_with('.') || username.ends_with(' ') {
        return Err("用户名不能以点或空格结尾");
    }
    Ok(())
}

/// 密码校验：非空、长度上限、无控制字符（含 NUL/`\r`/`\n`）；其余字符（含非 ASCII）
/// 一律允许——我们不做任何「密码强度」判断，那不是本层职责。
pub fn validate_password(password: &str) -> Result<(), &'static str> {
    if password.is_empty() {
        return Err("密码不能为空");
    }
    if password.chars().count() > MAX_PASSWORD_CHARS {
        return Err("密码过长（上限 512 字符）");
    }
    if password.chars().any(char::is_control) {
        return Err("密码含控制字符");
    }
    Ok(())
}

/// 密码字节校验（密码在内存里以字节流转，先判 UTF-8 再按字符判规则）。
pub fn validate_password_bytes(password: &[u8]) -> Result<(), &'static str> {
    let s = std::str::from_utf8(password).map_err(|_| "密码不是合法 UTF-8")?;
    validate_password(s)
}

/// 把整段字节清零并回读校验；返回是否确认清零（读回全 0 才算）。
///
/// `write_volatile` + 编译器栅栏：阻止优化把「写完就没人读」的覆写整段删掉。
pub fn wipe_slice(buf: &mut [u8]) -> bool {
    let len = buf.len();
    if len == 0 {
        return true;
    }
    let ptr = buf.as_mut_ptr();
    // SAFETY: `ptr` 来自 `&mut [u8]`；`0..len` 全部落在该切片内，本次调用期间不做任何
    // 重分配、也没有别的别名写入；`write_volatile` 逐字节写 0 是良定义的。
    unsafe {
        for i in 0..len {
            std::ptr::write_volatile(ptr.add(i), 0u8);
        }
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    buf.iter().all(|&b| b == 0)
}

/// 覆写并**清空**一个字节缓冲（容量保留：`shrink_to_fit` 会把内存还给分配器，
/// 清零的意义随之消失）。
pub fn wipe_bytes(buf: &mut Vec<u8>) {
    let wiped = wipe_slice(buf.as_mut_slice());
    debug_assert!(wiped, "wipe_slice 未能确认清零");
    let _ = wiped;
    buf.clear();
}

// ─────────────────────────── 后端（系统调用的抽象） ───────────────────────────

/// `get-auto-login` 的读取结果（**不含密码**）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AutoLoginState {
    /// `AutoAdminLogon` 是否为 "1"。
    pub enabled: bool,
    /// `DefaultUserName`（缺失/空 → `None`）。
    pub username: Option<String>,
}

/// 后端失败分类 → 固定错误码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SysError {
    /// 无权限（如 helper 未提权）。
    Denied,
    /// 系统调用失败。
    Failed,
    /// 超时。
    Timeout,
}

impl SysError {
    /// 对应的线上错误码。
    pub fn code(self) -> &'static str {
        match self {
            SysError::Denied => codes::DENIED,
            SysError::Failed => codes::SYSCALL_FAILED,
            SysError::Timeout => codes::TIMEOUT,
        }
    }
}

/// 真正执行系统动作的抽象：本模块只依赖这个 trait，所以纯逻辑可以单测
/// （测试用假后端，既不碰注册表，也不跑 runas）。
pub trait SysBackend {
    /// 写三个 Winlogon 值。
    fn set_auto_login(&self, username: &str, password: &str) -> Result<(), SysError>;
    /// 清三个 Winlogon 值（幂等）。
    fn clear_auto_login(&self) -> Result<(), SysError>;
    /// 只读两个值（永不读密码）。
    fn get_auto_login(&self) -> Result<AutoLoginState, SysError>;
}

// ─────────────────────────── 审计 ───────────────────────────

/// 审计文件名（append-only，一行一条 JSON）。
pub const AUDIT_FILE: &str = "elev-helper-audit.log";
/// 审计目录环境变量；缺省 `%USERPROFILE%\.agent-bridge\logs`。
pub const AUDIT_DIR_ENV: &str = "ABB_ELEV_AUDIT_DIR";
/// 脱敏后的密码占位符。
pub const REDACTED: &str = "<redacted>";

/// 审计 `result`：前置记录（已决定执行，尚未出结果）。
pub const RESULT_ATTEMPT: &str = "attempt";
/// 审计 `result`：成功。
pub const RESULT_OK: &str = "ok";
/// 审计 `result`：执行失败。
pub const RESULT_FAIL: &str = "fail";
/// 审计 `result`：被拒绝（表外 / 未鉴权 / 参数非法）。
pub const RESULT_DENIED: &str = "denied";

/// 一次请求的审计上下文（`ts` 由调用方给，测试因此不依赖时钟）。
pub struct AuditCtx<'a> {
    /// 审计目录。
    pub dir: &'a Path,
    /// RFC3339 时间戳。
    pub ts: &'a str,
    /// 来源（如调用方进程 `pid:1234`；取不到时退化为 helper 自身身份）。
    pub source: &'a str,
    /// 写审计的进程 pid。
    pub pid: u32,
}

/// 一条审计记录。字段集**封闭**——造行时用 `json!` 显式列出，任何新字段都必须在这里
/// 显式添加，避免哪天顺手把整个 params 结构体序列化出去（那正是密码泄漏的经典路径）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRecord {
    /// 时间戳。
    pub ts: String,
    /// 操作名（白名单名或占位名）。
    pub op: String,
    /// 脱敏参数。
    pub params: serde_json::Value,
    /// 错误码 / `ok` / `pending`。
    pub code: String,
    /// 结果分类（见 `RESULT_*`）。
    pub result: String,
    /// 来源。
    pub source: String,
    /// pid。
    pub pid: u32,
}

impl AuditRecord {
    /// 序列化成**一行** JSON（不可见字符由 `serde_json` 转义，因此一条记录恒占一行）。
    pub fn to_json_line(&self) -> String {
        let v = serde_json::json!({
            "ts": self.ts,
            "op": self.op,
            "params": self.params,
            "code": self.code,
            "result": self.result,
            "source": self.source,
            "pid": self.pid,
        });
        // Value 由我们构造，除非法数字外不会失败；真失败宁可给 `{}`，也不要把内部
        // 结构 repr 打出去。
        serde_json::to_string(&v).unwrap_or_else(|_| "{}".to_string())
    }
}

/// 用户名脱敏：首字符 + `***` + 字符数（例：`g*** (11)`）。
pub fn redact_username(username: &str) -> String {
    match username.chars().next() {
        Some(c) => format!("{c}*** ({})", username.chars().count()),
        None => "<empty>".to_string(),
    }
}

/// 审计用的脱敏参数。`set-auto-login` 是唯一会带字段的操作，且密码恒为 [`REDACTED`]。
pub fn redact_params(op: Op, username: Option<&str>) -> serde_json::Value {
    match op {
        Op::SetAutoLogin => serde_json::json!({
            "username": username.map(redact_username).unwrap_or_else(|| "<none>".to_string()),
            "password": REDACTED,
        }),
        // 清 / 读不带参数，审计也就没有可脱敏的东西。
        Op::ClearAutoLogin | Op::GetAutoLogin => serde_json::json!({}),
    }
}

/// 解析审计目录：`ABB_ELEV_AUDIT_DIR` → `%USERPROFILE%\.agent-bridge\logs`
/// （非 Windows 回退 `$HOME`）。都拿不到 = 无法审计 ⇒ 调用方应当 fail-closed 拒绝。
pub fn audit_dir_from_env() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os(AUDIT_DIR_ENV) {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    let home = std::env::var_os("USERPROFILE")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()))
        .ok_or_else(|| format!("无法确定审计目录（{AUDIT_DIR_ENV} / USERPROFILE 均缺失）"))?;
    Ok(PathBuf::from(home).join(".agent-bridge").join("logs"))
}

/// append-only 落一条审计。失败必须由调用方当作「不执行」处理（fail-closed）。
pub fn append_audit_line(dir: &Path, line: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(AUDIT_FILE))?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")?;
    f.flush()
}

// ─────────────────────────── 令牌 ───────────────────────────

/// 生成请求令牌：uuid v4 的 32 位十六进制形式（128bit，取自系统随机源）。
///
/// 它**不是**本通道的唯一边界：真正的边界是管道 DACL（只放当前用户 SID）；令牌的作用
/// 是抬高「同用户其它进程冒用这条提权通道」的成本（见模块头残余风险）。
pub fn gen_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// 定长比较（不短路，减少按字节计时的侧信道；长度差异仍可见，但它不是秘密）。
pub fn token_matches(expected: &str, got: &str) -> bool {
    let (a, b) = (expected.as_bytes(), got.as_bytes());
    if a.is_empty() || a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

// ─────────────────────────── 请求 / 响应 ───────────────────────────

/// 帧长前缀：4 字节大端。
pub const FRAME_HEADER: usize = 4;

/// 把 payload 编成「4 字节大端长度 + payload」。
pub fn encode_frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_HEADER + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// 解析帧长前缀。
pub fn decode_frame_len(header: [u8; FRAME_HEADER]) -> usize {
    u32::from_be_bytes(header) as usize
}

/// 帧长是否在允许范围内。
pub fn frame_len_ok(len: usize) -> bool {
    len > 0 && len <= MAX_BODY
}

/// 请求体：`{"token":"…","op":"…","params":{…}}`。
#[derive(Debug, Clone)]
pub struct Request {
    /// 调用方生成的令牌。
    pub token: String,
    /// 操作名（**未校验**，由 [`Op::from_name`] 判定）。
    pub op: String,
    /// 参数（原样持有；密码取走后立即从文档里摘除）。
    pub params: serde_json::Value,
}

/// 解析请求体。失败原因只用于内部判断，不进响应（响应恒为 `bad_params` + 通用人话）。
pub fn parse_request(body: &[u8]) -> Result<Request, &'static str> {
    if body.is_empty() || body.len() > MAX_BODY {
        return Err("请求体为空或超长");
    }
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|_| "请求体不是合法 JSON")?;
    let token = v
        .get("token")
        .and_then(|t| t.as_str())
        .ok_or("缺少 token 字段")?
        .to_string();
    let op = v
        .get("op")
        .and_then(|t| t.as_str())
        .ok_or("缺少 op 字段")?
        .to_string();
    let params = v
        .get("params")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    Ok(Request { token, op, params })
}

/// 响应：`{"ok":bool,"code":"…","reason":"人话"}`，`get-auto-login` 另带 `data`。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Response {
    /// 是否成功。
    pub ok: bool,
    /// 固定错误码集或 `ok`。
    pub code: String,
    /// 人话原因（**不含**原始参数值、不含系统 API 原始错误串）。
    pub reason: String,
    /// 仅 `get-auto-login` 携带。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub data: Option<serde_json::Value>,
}

/// 造一个失败响应（helper 在拿到审计目录之前也要能拒绝）。
pub fn resp_err(code: &str, reason: &str) -> Response {
    Response {
        ok: false,
        code: code.to_string(),
        reason: reason.to_string(),
        data: None,
    }
}

/// 请求里的 op 名进审计时的截断（防超长/畸形串撑爆审计行）。
fn op_label(raw: &str) -> String {
    if raw.is_empty() {
        return OP_UNKNOWN.to_string();
    }
    raw.chars().take(OP_LABEL_MAX_CHARS).collect()
}

/// 从 `params` 里**摘走**一个字符串字段（摘走 = 从文档移除，之后文档不再持有它）。
fn take_string(params: &mut serde_json::Value, key: &str) -> Result<String, &'static str> {
    match params.get_mut(key) {
        None | Some(serde_json::Value::Null) => Err("缺少必填参数"),
        Some(serde_json::Value::String(s)) => Ok(std::mem::take(s)),
        Some(_) => Err("参数类型不是字符串"),
    }
}

const AUDIT_FAILED_REASON: &str = "审计写入失败，已拒绝执行（fail-closed）";
const AUDIT_FAILED_REASON_RESULT: &str =
    "结果审计写入失败，未回报成功（三个操作均幂等，可安全重试）";

/// 拒绝请求：**先记审计，再拒绝**；审计本身写不进去就报 `audit_failed`（仍然拒绝，
/// 且绝不执行）。
///
/// 拒绝路径的审计 `params` 恒为 `{}`：此时参数**没有被校验过**，任何回显都可能把未
/// 校验内容（包括密码）带进审计。`op` 记请求里声明的名字（截断），便于追责「谁试过
/// 什么」。
fn deny(ctx: &AuditCtx<'_>, op: &str, code: &str, reason: &str) -> Response {
    let rec = AuditRecord {
        ts: ctx.ts.to_string(),
        op: op.to_string(),
        params: serde_json::json!({}),
        code: code.to_string(),
        result: RESULT_DENIED.to_string(),
        source: ctx.source.to_string(),
        pid: ctx.pid,
    };
    if append_audit_line(ctx.dir, &rec.to_json_line()).is_err() {
        return resp_err(codes::AUDIT_FAILED, AUDIT_FAILED_REASON);
    }
    resp_err(code, reason)
}

/// 成功时的人话原因。
fn ok_reason(op: Op) -> &'static str {
    match op {
        Op::SetAutoLogin => "已写入自动登录配置",
        Op::ClearAutoLogin => "已清除自动登录配置",
        Op::GetAutoLogin => "读取成功",
    }
}

/// 失败时的人话原因（不含系统 API 原始错误串）。
fn sys_reason(err: SysError) -> &'static str {
    match err {
        SysError::Denied => "需要管理员权限（helper 未以管理员身份运行）",
        SysError::Failed => "系统调用失败（注册表访问被拒绝或系统错误）",
        SysError::Timeout => "操作超时",
    }
}

/// 写「结果」审计并造最终响应。
///
/// 结果审计写失败时**不回成功**：调用方拿不到成功信号就会重试，而这三个操作都幂等
/// （写同值 / 清已清 / 只读），所以这样比「悄悄放行一次无审计的成功」安全。
fn conclude(
    ctx: &AuditCtx<'_>,
    op: Op,
    params: serde_json::Value,
    outcome: Result<Option<serde_json::Value>, SysError>,
) -> Response {
    let (ok, code, reason, data, result) = match outcome {
        Ok(data) => (true, codes::OK, ok_reason(op), data, RESULT_OK),
        Err(err) => (false, err.code(), sys_reason(err), None, RESULT_FAIL),
    };
    let rec = AuditRecord {
        ts: ctx.ts.to_string(),
        op: op.name().to_string(),
        params,
        code: code.to_string(),
        result: result.to_string(),
        source: ctx.source.to_string(),
        pid: ctx.pid,
    };
    if append_audit_line(ctx.dir, &rec.to_json_line()).is_err() {
        return resp_err(codes::AUDIT_FAILED, AUDIT_FAILED_REASON_RESULT);
    }
    Response {
        ok,
        code: code.to_string(),
        reason: reason.to_string(),
        data,
    }
}

/// 前置审计（`pending`/`attempt`）。失败 ⇒ 调用方**必须**中止，不执行系统调用。
fn audit_attempt(ctx: &AuditCtx<'_>, op: Op, params: &serde_json::Value) -> Result<(), ()> {
    let rec = AuditRecord {
        ts: ctx.ts.to_string(),
        op: op.name().to_string(),
        params: params.clone(),
        code: "pending".to_string(),
        result: RESULT_ATTEMPT.to_string(),
        source: ctx.source.to_string(),
        pid: ctx.pid,
    };
    append_audit_line(ctx.dir, &rec.to_json_line()).map_err(|_| ())
}

fn set_auto_login(mut req: Request, ctx: &AuditCtx<'_>, backend: &dyn SysBackend) -> Response {
    let op = Op::SetAutoLogin;
    let username = match take_string(&mut req.params, "username") {
        Ok(u) => u,
        Err(reason) => return deny(ctx, op.name(), codes::BAD_PARAMS, reason),
    };
    let mut password = match take_string(&mut req.params, "password") {
        Ok(p) => p.into_bytes(),
        Err(reason) => return deny(ctx, op.name(), codes::BAD_PARAMS, reason),
    };
    if let Err(reason) = validate_username(&username) {
        wipe_bytes(&mut password);
        return deny(ctx, op.name(), codes::BAD_PARAMS, reason);
    }
    if let Err(reason) = validate_password_bytes(&password) {
        wipe_bytes(&mut password);
        return deny(ctx, op.name(), codes::BAD_PARAMS, reason);
    }
    let Ok(password_str) = std::str::from_utf8(&password) else {
        // 上面刚校验过 UTF-8，这里不可达；真到了也按参数错处理，绝不 panic、不回显。
        wipe_bytes(&mut password);
        return deny(ctx, op.name(), codes::BAD_PARAMS, "密码不是合法 UTF-8");
    };
    let params = redact_params(op, Some(&username));
    if audit_attempt(ctx, op, &params).is_err() {
        wipe_bytes(&mut password);
        return resp_err(codes::AUDIT_FAILED, AUDIT_FAILED_REASON);
    }
    let outcome = backend
        .set_auto_login(&username, password_str)
        .map(|()| None);
    // 密码生命周期到此为止：无论成败立刻清零。
    wipe_bytes(&mut password);
    conclude(ctx, op, params, outcome)
}

fn clear_auto_login(ctx: &AuditCtx<'_>, backend: &dyn SysBackend) -> Response {
    let op = Op::ClearAutoLogin;
    let params = redact_params(op, None);
    if audit_attempt(ctx, op, &params).is_err() {
        return resp_err(codes::AUDIT_FAILED, AUDIT_FAILED_REASON);
    }
    conclude(ctx, op, params, backend.clear_auto_login().map(|()| None))
}

fn get_auto_login(ctx: &AuditCtx<'_>, backend: &dyn SysBackend) -> Response {
    let op = Op::GetAutoLogin;
    let params = redact_params(op, None);
    if audit_attempt(ctx, op, &params).is_err() {
        return resp_err(codes::AUDIT_FAILED, AUDIT_FAILED_REASON);
    }
    match backend.get_auto_login() {
        Ok(state) => conclude(
            ctx,
            op,
            params,
            Ok(Some(serde_json::json!({
                "enabled": state.enabled,
                // 只回用户名；密码永不出现在响应里（后端根本读不到它）。
                "username": state.username,
            }))),
        ),
        Err(err) => conclude(ctx, op, params, Err(err)),
    }
}

/// 处理一个请求，返回响应。
///
/// 顺序是有意的：**鉴权 → 白名单 → 参数 → 审计 → 执行**。前三步都在任何系统调用之前，
/// 所以「表外操作」和「未鉴权请求」都不可能触发系统动作。
///
/// `body` 属于调用方：本函数不持有它，调用方用完应 [`wipe_bytes`]（里面可能有密码）。
pub fn handle_request(
    body: &[u8],
    expected_token: &str,
    ctx: &AuditCtx<'_>,
    backend: &dyn SysBackend,
) -> Response {
    let req = match parse_request(body) {
        Ok(req) => req,
        Err(_) => {
            return deny(
                ctx,
                OP_UNKNOWN,
                codes::BAD_PARAMS,
                "请求格式非法（不是合法 JSON 或缺少 token/op 字段）",
            );
        }
    };
    if !token_matches(expected_token, &req.token) {
        return deny(
            ctx,
            &op_label(&req.op),
            codes::UNAUTHORIZED,
            "令牌不匹配，拒绝未鉴权请求",
        );
    }
    let op = match Op::from_name(&req.op) {
        Some(op) => op,
        None => {
            return deny(
                ctx,
                &op_label(&req.op),
                codes::INVALID_OP,
                "操作不在白名单内，已拒绝",
            );
        }
    };
    match op {
        Op::SetAutoLogin => set_auto_login(req, ctx, backend),
        Op::ClearAutoLogin => clear_auto_login(ctx, backend),
        Op::GetAutoLogin => get_auto_login(ctx, backend),
    }
}

/// 常量名 → 管道名：`\\.\pipe\abb-elev-helper-<suffix>`。
pub fn pipe_name(suffix: &str) -> String {
    format!(r"\\.\pipe\abb-elev-helper-{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 假后端：记录调用、按需返回错误。**不碰注册表、不跑 runas**。
    #[derive(Default)]
    struct Fake {
        state: Mutex<AutoLoginState>,
        set_calls: Mutex<Vec<(String, String)>>,
        clear_calls: Mutex<usize>,
        get_calls: Mutex<usize>,
        fail: Option<SysError>,
    }

    impl Fake {
        fn with_state(state: AutoLoginState) -> Self {
            Fake {
                state: Mutex::new(state),
                ..Fake::default()
            }
        }
        fn failing(err: SysError) -> Self {
            Fake {
                fail: Some(err),
                ..Fake::default()
            }
        }
        /// 后端被调用过几次（用来断言「拒绝路径不触达系统」）。
        fn touched(&self) -> usize {
            self.set_calls.lock().unwrap().len()
                + *self.clear_calls.lock().unwrap()
                + *self.get_calls.lock().unwrap()
        }
    }

    impl SysBackend for Fake {
        fn set_auto_login(&self, username: &str, password: &str) -> Result<(), SysError> {
            if let Some(err) = self.fail {
                return Err(err);
            }
            self.set_calls
                .lock()
                .unwrap()
                .push((username.to_string(), password.to_string()));
            let mut st = self.state.lock().unwrap();
            st.enabled = true;
            st.username = Some(username.to_string());
            Ok(())
        }
        fn clear_auto_login(&self) -> Result<(), SysError> {
            if let Some(err) = self.fail {
                return Err(err);
            }
            *self.clear_calls.lock().unwrap() += 1;
            let mut st = self.state.lock().unwrap();
            st.enabled = false;
            st.username = None;
            Ok(())
        }
        fn get_auto_login(&self) -> Result<AutoLoginState, SysError> {
            if let Some(err) = self.fail {
                return Err(err);
            }
            *self.get_calls.lock().unwrap() += 1;
            Ok(self.state.lock().unwrap().clone())
        }
    }

    /// 每个用例独占一个临时目录（并行互不干扰，且绝不碰真实家目录）。
    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("abb-elev-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx(dir: &Path) -> AuditCtx<'_> {
        AuditCtx {
            dir,
            ts: "2026-09-24T00:00:00.000Z",
            source: "pid:4321",
            pid: 1234,
        }
    }

    fn audit_text(dir: &Path) -> String {
        std::fs::read_to_string(dir.join(AUDIT_FILE)).unwrap_or_default()
    }

    fn audit_lines(dir: &Path) -> Vec<serde_json::Value> {
        audit_text(dir)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("审计每行必须是合法 JSON"))
            .collect()
    }

    fn request(token: &str, op: &str, params: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "token": token,
            "op": op,
            "params": params,
        }))
        .unwrap()
    }

    fn code_of(resp: &Response) -> &str {
        &resp.code
    }

    // ───────────────────────── 契约 ─────────────────────────

    #[test]
    fn error_codes_are_a_closed_set() {
        assert_eq!(codes::ALL.len(), 7);
        let mut sorted = codes::ALL.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 7, "错误码不得重复");
        assert_eq!(
            codes::ALL,
            [
                "invalid_op",
                "bad_params",
                "unauthorized",
                "denied",
                "syscall_failed",
                "timeout",
                "audit_failed"
            ],
            "错误码集是对外契约，改动必须视为破坏性变更"
        );
        assert!(!codes::ALL.contains(&codes::OK));
    }

    // ───────────────────────── 白名单 ─────────────────────────

    #[test]
    fn whitelist_is_exactly_three_ops() {
        assert_eq!(Op::ALL.len(), 3);
        for op in Op::ALL {
            assert_eq!(Op::from_name(op.name()), Some(op));
        }
        assert_eq!(Op::SetAutoLogin.name(), "set-auto-login");
        assert_eq!(Op::ClearAutoLogin.name(), "clear-auto-login");
        assert_eq!(Op::GetAutoLogin.name(), "get-auto-login");
        assert_eq!(Op::SetAutoLogin.name(), OP_SET_AUTO_LOGIN);
    }

    #[test]
    fn reject_non_whitelisted_op() {
        // 解析层：表外名字（含大小写变体、空串、前后空白）一律 None。
        for bad in [
            "format-c",
            "rm-rf",
            "",
            "Set-Auto-Login",
            "SET-AUTO-LOGIN",
            " set-auto-login",
            "set-auto-login ",
            "get-auto-login\n",
            "install-service",
            "exec",
            "..",
        ] {
            assert_eq!(Op::from_name(bad), None, "不该接受操作名 {bad:?}");
        }

        // 分发层：表外操作返回 invalid_op，**不碰后端**，但必须留审计。
        let dir = tmp_dir("reject-non-whitelisted");
        let tok = gen_token();
        let backend = Fake::default();
        for bad in ["format-c", "rm-rf", "exec"] {
            let body = request(&tok, bad, serde_json::json!({}));
            let resp = handle_request(&body, &tok, &ctx(&dir), &backend);
            assert_eq!(code_of(&resp), codes::INVALID_OP);
            assert!(!resp.ok);
            assert_eq!(backend.touched(), 0, "表外操作绝不能触达系统后端");
            // 响应里不回显请求里的原始输入。
            let raw = serde_json::to_string(&resp).unwrap();
            assert!(!raw.contains(bad), "响应回显了操作名：{raw}");
        }
        let lines = audit_lines(&dir);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["op"], "format-c");
        assert_eq!(lines[0]["result"], RESULT_DENIED);
        assert_eq!(lines[0]["code"], codes::INVALID_OP);
    }

    #[test]
    fn long_op_name_is_truncated_in_audit() {
        let dir = tmp_dir("long-op");
        let tok = gen_token();
        let backend = Fake::default();
        let op = "x".repeat(5000);
        let body = request(&tok, &op, serde_json::json!({}));
        let resp = handle_request(&body, &tok, &ctx(&dir), &backend);
        assert_eq!(code_of(&resp), codes::INVALID_OP);
        let lines = audit_lines(&dir);
        assert_eq!(lines[0]["op"].as_str().unwrap().chars().count(), 64);
    }

    // ───────────────────────── 鉴权 ─────────────────────────

    #[test]
    fn unknown_token_rejected() {
        let dir = tmp_dir("unknown-token");
        let tok = gen_token();
        let backend = Fake::default();
        let secret = "Sup3rSecret!";
        let body = request(
            &tok,
            OP_SET_AUTO_LOGIN,
            serde_json::json!({ "username": "gqf", "password": secret }),
        );
        let resp = handle_request(
            &body,
            "deadbeefdeadbeefdeadbeefdeadbeef",
            &ctx(&dir),
            &backend,
        );
        assert_eq!(code_of(&resp), codes::UNAUTHORIZED);
        assert_eq!(backend.touched(), 0, "未鉴权请求绝不触达后端");
        let lines = audit_lines(&dir);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["code"], codes::UNAUTHORIZED);
        // 未鉴权请求连参数都不记（更不可能记密码）。
        assert_eq!(lines[0]["params"], serde_json::json!({}));
        assert!(!audit_text(&dir).contains(secret));
        assert!(!audit_text(&dir).contains("Sup3r"));
    }

    #[test]
    fn token_compare_and_generation() {
        assert!(token_matches("abc", "abc"));
        assert!(!token_matches("abc", "abd"));
        assert!(!token_matches("abc", "abcd"));
        assert!(!token_matches("abcd", "abc"));
        // 空期望令牌 = 配置错误，必须拒绝任何请求（含空令牌本身）。
        assert!(!token_matches("", ""));
        assert!(!token_matches("", "x"));

        let t1 = gen_token();
        let t2 = gen_token();
        assert_eq!(t1.len(), 32);
        assert!(t1.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(t1, t2, "每次生成的令牌应当不同");
    }

    // ───────────────────────── 参数校验 ─────────────────────────

    #[test]
    fn param_validation() {
        // 合法输入。
        for ok in ["gqf2008", "张三", "user.name", "a b", "_.-"] {
            assert!(validate_username(ok).is_ok(), "应当接受用户名 {ok:?}");
        }
        for ok in ["Sup3r Secret!", "密码123", "a", "中文密码"] {
            assert!(validate_password(ok).is_ok(), "应当接受密码 {ok:?}");
        }

        // 非法用户名：空 / 控制字符（含 NUL）/ 禁用字符 / 以点或空格结尾。
        for bad in [
            "", "a\\b", "a/b", "a[b", "a]b", "a:b", "a;b", "a|b", "a=b", "a,b", "a+b", "a*b",
            "a?b", "a<b", "a>b", "a\"b", "a\u{7}b", "a\u{0}b", "gqf ", "gqf.",
        ] {
            assert!(validate_username(bad).is_err(), "不该接受用户名 {bad:?}");
        }
        assert!(validate_username(&"a".repeat(MAX_USERNAME_CHARS)).is_ok());
        assert!(validate_username(&"a".repeat(MAX_USERNAME_CHARS + 1)).is_err());

        // 非法密码：空 / 控制字符（含 NUL、CR、LF）/ 超长。
        for bad in ["", "a\nb", "a\rb", "a\u{0}b", "a\u{1}b"] {
            assert!(validate_password(bad).is_err(), "不该接受密码 {bad:?}");
        }
        assert!(validate_password(&"p".repeat(MAX_PASSWORD_CHARS)).is_ok());
        assert!(validate_password(&"p".repeat(MAX_PASSWORD_CHARS + 1)).is_err());

        // 字节形态：合法 UTF-8 走同一套规则；非法 UTF-8 直接拒。
        assert!(validate_password_bytes("pw".as_bytes()).is_ok());
        assert!(validate_password_bytes(&[0xff, 0xfe]).is_err());
    }

    #[test]
    fn bad_params_rejected_before_backend() {
        let dir = tmp_dir("bad-params");
        let tok = gen_token();
        let backend = Fake::default();
        let cases: Vec<serde_json::Value> = vec![
            serde_json::json!({ "username": "", "password": "pw" }),
            serde_json::json!({ "username": "corp\\gqf", "password": "pw" }),
            serde_json::json!({ "username": "gqf\u{0}", "password": "pw" }),
            serde_json::json!({ "username": "a".repeat(MAX_USERNAME_CHARS + 1), "password": "pw" }),
            serde_json::json!({ "username": "gqf.", "password": "pw" }),
            serde_json::json!({ "username": "gqf", "password": "" }),
            serde_json::json!({ "username": "gqf", "password": "p\nw" }),
            serde_json::json!({ "username": "gqf" }),
            serde_json::json!({ "password": "pw" }),
            serde_json::json!({ "username": 42, "password": "pw" }),
            serde_json::json!({ "username": null, "password": "pw" }),
        ];
        let total = cases.len();
        for params in cases {
            let body = request(&tok, OP_SET_AUTO_LOGIN, params.clone());
            let resp = handle_request(&body, &tok, &ctx(&dir), &backend);
            assert_eq!(code_of(&resp), codes::BAD_PARAMS, "params={params}");
            assert!(!resp.ok);
            assert!(!resp.reason.is_empty(), "拒绝必须给一个人话原因");
        }
        assert_eq!(backend.touched(), 0, "参数不合法时绝不触达后端");
        assert_eq!(audit_lines(&dir).len(), total, "每条拒绝都要留审计");
    }

    #[test]
    fn malformed_request_is_bad_params_and_audited() {
        let dir = tmp_dir("malformed");
        let tok = gen_token();
        let backend = Fake::default();
        for body in [
            b"not json".to_vec(),
            br#"{"op":"get-auto-login"}"#.to_vec(),
            br#"{"token":"x","params":{}}"#.to_vec(),
            Vec::new(),
            vec![b'x'; MAX_BODY + 1],
        ] {
            let resp = handle_request(&body, &tok, &ctx(&dir), &backend);
            assert_eq!(code_of(&resp), codes::BAD_PARAMS);
        }
        assert_eq!(backend.touched(), 0);
        let lines = audit_lines(&dir);
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0]["op"], OP_UNKNOWN);
    }

    // ───────────────────────── 密码生命周期 ─────────────────────────

    #[test]
    fn password_not_in_audit_or_error() {
        let dir = tmp_dir("password-redaction");
        let tok = gen_token();
        let backend = Fake::default();
        let secret = "Sup3rSecret!";
        let body = request(
            &tok,
            OP_SET_AUTO_LOGIN,
            serde_json::json!({ "username": "gqf2008", "password": secret }),
        );
        let resp = handle_request(&body, &tok, &ctx(&dir), &backend);
        assert_eq!(code_of(&resp), codes::OK);
        assert!(resp.ok);

        // 后端确实拿到了原文（证明脱敏只作用于日志，不作用于通道本身）。
        assert_eq!(
            backend.set_calls.lock().unwrap().as_slice(),
            &[("gqf2008".to_string(), secret.to_string())]
        );

        // 审计：密码恒为 <redacted>，用户名只留首字符 + 字符数；两行都不含明文。
        let text = audit_text(&dir);
        assert!(!text.contains(secret), "审计里出现了密码明文：{text}");
        assert!(!text.contains("Sup3r"), "审计里出现了密码前缀");
        let lines = audit_lines(&dir);
        assert_eq!(lines.len(), 2, "attempt + ok 两条");
        assert_eq!(lines[0]["result"], RESULT_ATTEMPT);
        assert_eq!(lines[0]["code"], "pending");
        assert_eq!(lines[1]["result"], RESULT_OK);
        for line in &lines {
            assert_eq!(line["params"]["password"], REDACTED);
            assert_eq!(line["params"]["username"], "g*** (7)");
            assert_eq!(line["op"], OP_SET_AUTO_LOGIN);
            assert_eq!(line["source"], "pid:4321");
            assert_eq!(line["pid"].as_u64(), Some(1234));
            assert_eq!(line["ts"], "2026-09-24T00:00:00.000Z");
        }

        // 响应也不含密码。
        let raw = serde_json::to_string(&resp).unwrap();
        assert!(!raw.contains(secret));

        // 校验失败路径：错误原因同样不回显密码值。
        let bad = request(
            &tok,
            OP_SET_AUTO_LOGIN,
            serde_json::json!({ "username": "gqf", "password": format!("{secret}\n") }),
        );
        let resp = handle_request(&bad, &tok, &ctx(&dir), &backend);
        assert_eq!(code_of(&resp), codes::BAD_PARAMS);
        assert!(!resp.reason.contains(secret));
        assert!(!resp.reason.contains("Sup3r"));
        assert!(!audit_text(&dir).contains(secret));
    }

    #[test]
    fn redaction_helpers() {
        assert_eq!(redact_username("gqf2008"), "g*** (7)");
        assert_eq!(redact_username("张三"), "张*** (2)");
        assert_eq!(redact_username(""), "<empty>");
        assert_eq!(
            redact_params(Op::SetAutoLogin, Some("gqf2008")),
            serde_json::json!({ "username": "g*** (7)", "password": REDACTED })
        );
        assert_eq!(
            redact_params(Op::SetAutoLogin, None),
            serde_json::json!({ "username": "<none>", "password": REDACTED })
        );
        assert_eq!(
            redact_params(Op::ClearAutoLogin, None),
            serde_json::json!({})
        );
        assert_eq!(redact_params(Op::GetAutoLogin, None), serde_json::json!({}));
    }

    #[test]
    fn wipe_bytes_zeroes_then_clears() {
        let mut buf = b"Sup3rSecret!".to_vec();
        // 覆写 + 回读校验：返回 true 且内容确实全 0。
        assert!(wipe_slice(buf.as_mut_slice()));
        assert!(buf.iter().all(|&b| b == 0));
        // 幂等：再擦一次仍然为真（含空切片）。
        assert!(wipe_slice(buf.as_mut_slice()));
        assert!(wipe_slice(&mut []));
        wipe_bytes(&mut buf);
        assert!(buf.is_empty());
    }

    // ───────────────────────── 审计 fail-closed ─────────────────────────

    #[test]
    fn audit_write_failure_is_fail_closed() {
        // 审计目录位置放一个**文件**：create_dir_all 必然失败。
        let base = tmp_dir("audit-fail");
        let blocked = base.join("not-a-dir");
        std::fs::write(&blocked, b"x").unwrap();

        let tok = gen_token();
        let backend = Fake::default();
        // 1) 前置审计失败 ⇒ 不执行。
        let body = request(
            &tok,
            OP_SET_AUTO_LOGIN,
            serde_json::json!({ "username": "gqf", "password": "pw" }),
        );
        let resp = handle_request(&body, &tok, &ctx(&blocked), &backend);
        assert_eq!(code_of(&resp), codes::AUDIT_FAILED);
        assert!(!resp.ok);
        assert_eq!(backend.touched(), 0, "审计写不进去就不许执行");

        // 2) 拒绝路径同样如此（仍然拒绝，只是理由换成审计失败）。
        let bad_op = request(&tok, "format-c", serde_json::json!({}));
        let resp = handle_request(&bad_op, &tok, &ctx(&blocked), &backend);
        assert_eq!(code_of(&resp), codes::AUDIT_FAILED);
        assert_eq!(backend.touched(), 0);
    }

    // ───────────────────────── 正常路径 ─────────────────────────

    #[test]
    fn set_clear_get_roundtrip() {
        let dir = tmp_dir("roundtrip");
        let tok = gen_token();
        let backend = Fake::with_state(AutoLoginState::default());

        // set：后端拿到原文；响应无 data。
        let body = request(
            &tok,
            OP_SET_AUTO_LOGIN,
            serde_json::json!({ "username": "gqf2008", "password": "pw-1234" }),
        );
        let resp = handle_request(&body, &tok, &ctx(&dir), &backend);
        assert!(resp.ok);
        assert!(resp.data.is_none());
        assert_eq!(code_of(&resp), codes::OK);

        // get：只回 enabled + username，绝不回密码。
        let get_body = request(&tok, OP_GET_AUTO_LOGIN, serde_json::json!({}));
        let resp = handle_request(&get_body, &tok, &ctx(&dir), &backend);
        assert!(resp.ok);
        let data = resp.data.clone().unwrap();
        assert_eq!(data["enabled"], serde_json::json!(true));
        assert_eq!(data["username"], serde_json::json!("gqf2008"));
        assert!(data.get("password").is_none());
        assert!(!serde_json::to_string(&resp).unwrap().contains("pw-1234"));

        // clear：幂等（清已清也成功）。
        let clear_body = request(&tok, OP_CLEAR_AUTO_LOGIN, serde_json::json!({}));
        let resp = handle_request(&clear_body, &tok, &ctx(&dir), &backend);
        assert!(resp.ok);
        let resp2 = handle_request(&clear_body, &tok, &ctx(&dir), &backend);
        assert!(resp2.ok);

        // get 之后应看到已关闭。
        let resp = handle_request(&get_body, &tok, &ctx(&dir), &backend);
        assert_eq!(resp.data.unwrap()["enabled"], serde_json::json!(false));

        // 审计：一次请求两条（attempt + 结果），op 名正确。
        let lines = audit_lines(&dir);
        assert_eq!(lines.len(), 10, "5 次请求 × 2 条");
        assert_eq!(lines[0]["op"], OP_SET_AUTO_LOGIN);
        assert_eq!(lines[1]["op"], OP_SET_AUTO_LOGIN);
        assert_eq!(lines[2]["op"], OP_GET_AUTO_LOGIN);
        assert_eq!(lines[2]["params"], serde_json::json!({}));
        assert_eq!(lines[4]["op"], OP_CLEAR_AUTO_LOGIN);
    }

    #[test]
    fn syscall_errors_map_to_fixed_codes() {
        let dir = tmp_dir("syscall-errors");
        let tok = gen_token();
        let cases = [
            (SysError::Denied, codes::DENIED),
            (SysError::Failed, codes::SYSCALL_FAILED),
            (SysError::Timeout, codes::TIMEOUT),
        ];
        for (err, expected) in cases {
            let backend = Fake::failing(err);
            let body = request(&tok, OP_GET_AUTO_LOGIN, serde_json::json!({}));
            let resp = handle_request(&body, &tok, &ctx(&dir), &backend);
            assert_eq!(code_of(&resp), expected);
            assert!(!resp.ok);
            assert!(resp.data.is_none());
            assert!(!resp.reason.is_empty());
        }
        // 每条失败请求都留了 attempt + fail 两行审计。
        let lines = audit_lines(&dir);
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[1]["result"], RESULT_FAIL);
    }

    // ───────────────────────── 帧 / 管道名 / 审计目录 ─────────────────────────

    #[test]
    fn framing_roundtrip() {
        let payload = br#"{"token":"x"}"#;
        let framed = encode_frame(payload);
        assert_eq!(framed.len(), FRAME_HEADER + payload.len());
        let mut header = [0u8; FRAME_HEADER];
        header.copy_from_slice(&framed[..FRAME_HEADER]);
        assert_eq!(decode_frame_len(header), payload.len());
        assert_eq!(&framed[FRAME_HEADER..], payload);

        // 长度边界：0 与超过 MAX_BODY 都不接受。
        assert!(frame_len_ok(1));
        assert!(frame_len_ok(MAX_BODY));
        assert!(!frame_len_ok(0));
        assert!(!frame_len_ok(MAX_BODY + 1));
    }

    #[test]
    fn pipe_name_is_local_pipe() {
        let name = pipe_name("1234-abcd");
        assert_eq!(name, r"\\.\pipe\abb-elev-helper-1234-abcd");
        assert!(name.starts_with(r"\\.\pipe\"), "必须是本机命名管道");
    }

    #[test]
    fn audit_dir_env_override() {
        let dir = tmp_dir("audit-dir-env");
        let old = std::env::var_os(AUDIT_DIR_ENV);
        std::env::set_var(AUDIT_DIR_ENV, &dir);
        assert_eq!(audit_dir_from_env().unwrap(), dir);
        // 空值 = 未设置（走 USERPROFILE 缺省，审计落在 ~/.agent-bridge/logs）。
        std::env::set_var(AUDIT_DIR_ENV, "");
        let fallback = audit_dir_from_env().unwrap();
        assert!(fallback.ends_with(Path::new(".agent-bridge").join("logs").as_path()));
        match old {
            Some(v) => std::env::set_var(AUDIT_DIR_ENV, v),
            None => std::env::remove_var(AUDIT_DIR_ENV),
        }
    }
}
