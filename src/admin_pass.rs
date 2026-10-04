//! 「停止 / 重启服务」的管理密码（2026-10-04 owner 要求：停止需要管理员密码）。
//!
//! 与既有授权链的关系（**关键，别把两道门混成一道**）：
//! - 本模块只回答「用户输入的密码对不对」。它是 **ABB 自己的第二道门** —— 挡的是误停、随手停、
//!   以及「agent 或别的同用户进程顺手把服务停了」这类事；
//! - 真正让普通进程**停不掉**服务的是 OS 级事实：Windows 上服务跑在 HighestAvailable 计划任务下
//!   （普通权限 kill 会被拒），macOS 上是 launchd 托管的 agent。停服务仍然要走
//!   platform::stop_service_authorized() 那条平台授权路径（UAC / 管理员密码框），密码通过
//!   **不能**替代它。
//! - 所以：密码是「加一道」，不是「换一道」。别把平台授权删掉换成这里（那把安全等级降到
//!   「任何能读 config.json 的同用户进程都能停服务」）。
//!
//! 存储格式：sha256$<iters>$<salt_hex>$<hash_hex>（**从不存明文**）。
//! 迭代：h = SHA256(salt || password)，之后 h = SHA256(h || salt) 重复 iters-1 次。
//! 盐：16 字节，来自「时间纳秒 + pid + 计数」——盐的职责是**唯一**（防彩虹表 / 防两处相同
//! 密码撞库），不是保密，故不需要密码学随机源，也就不为此新增依赖。

use sha2::{Digest, Sha256};

/// 记录前缀（将来换算法靠它分派，格式不认识一律判失败）。
const PREFIX: &str = "sha256";
/// 默认迭代次数：单核约 50-100 ms（一次停止动作只算一次，用户感知不到）。
pub const DEFAULT_ITERS: u32 = 100_000;
/// 盐长度（字节）。
const SALT_LEN: usize = 16;
/// 迭代次数上限：防有人把 config 改成天文数字把 UI 卡死。
const MAX_ITERS: u32 = 10_000_000;

/// 是否已设置管理密码（空串 = 未设置）。
pub fn is_set(record: &str) -> bool {
    !record.trim().is_empty()
}

/// 生成一条新记录（自带盐）。
pub fn new_record(password: &str) -> String {
    let salt = fresh_salt();
    record_with(password, &salt, DEFAULT_ITERS)
}

/// 可注入盐/迭代次数的构造（单测用）。
pub fn record_with(password: &str, salt: &[u8], iters: u32) -> String {
    let iters = iters.clamp(1, MAX_ITERS);
    let hash = hex(&derive(password, salt, iters));
    [PREFIX, &iters.to_string(), &hex(salt), &hash].join("$")
}

/// 校验密码是否匹配记录。
///
/// **任何解析失败都返回 false，绝不 panic**：config.json 是用户可编辑的，半截 / 写坏的记录
/// 只能意味着「验证不通过」，不能让 UI 崩。
pub fn verify_record(record: &str, password: &str) -> bool {
    let Some((iters, salt, want)) = parse(record) else {
        return false;
    };
    let got = derive(password, &salt, iters);
    // 定长比较 + 累积异或：不做早退，避免把「前几字节对了」这种信息从耗时里漏出去。
    if got.len() != want.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in got.iter().zip(want.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// 解析记录 -> (iters, salt, hash)；格式不认识返回 None。
fn parse(record: &str) -> Option<(u32, Vec<u8>, Vec<u8>)> {
    let mut parts = record.trim().split('$');
    if parts.next()? != PREFIX {
        return None;
    }
    let iters: u32 = parts.next()?.parse().ok()?;
    if iters == 0 || iters > MAX_ITERS {
        return None;
    }
    let salt = unhex(parts.next()?)?;
    let hash = unhex(parts.next()?)?;
    if parts.next().is_some() || salt.is_empty() || hash.is_empty() {
        return None;
    }
    Some((iters, salt, hash))
}

/// KDF：iters 轮 SHA256（salt 每轮参与）。
fn derive(password: &str, salt: &[u8], iters: u32) -> Vec<u8> {
    let mut h: Vec<u8> = {
        let mut d = Sha256::new();
        d.update(salt);
        d.update(password.as_bytes());
        d.finalize().to_vec()
    };
    for _ in 1..iters.max(1) {
        let mut d = Sha256::new();
        d.update(&h);
        d.update(salt);
        h = d.finalize().to_vec();
    }
    h
}

/// 盐：时间纳秒 + pid + 进程内计数（唯一性足够，不需要保密）。
fn fresh_salt() -> Vec<u8> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seed = [
        nanos.to_string(),
        std::process::id().to_string(),
        SEQ.fetch_add(1, Ordering::Relaxed).to_string(),
    ]
    .join("-");
    let d = Sha256::digest(seed.as_bytes());
    d[..SALT_LEN].to_vec()
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.is_empty() || !s.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let hi = (b[i] as char).to_digit(16)?;
        let lo = (b[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 迭代次数调小（100k 一次约 50-100ms，单测跑几十次太慢）；断言的是**语义**不是强度。
    const FAST: u32 = 8;

    #[test]
    fn round_trip_accepts_the_right_password() {
        let rec = record_with("hunter2", b"0123456789abcdef", FAST);
        assert!(is_set(&rec));
        assert!(verify_record(&rec, "hunter2"));
    }

    #[test]
    fn rejects_wrong_password_and_empty() {
        let rec = record_with("hunter2", b"0123456789abcdef", FAST);
        assert!(!verify_record(&rec, "hunter3"));
        assert!(!verify_record(&rec, ""));
        assert!(!verify_record(&rec, "hunter2 ")); // 不 trim：密码就是密码
    }

    /// 盐不同 => 同一密码的记录不同（防彩虹表、防「两处同密码撞库」）。
    #[test]
    fn salt_makes_records_differ_for_same_password() {
        let a = record_with("same", b"aaaaaaaaaaaaaaaa", FAST);
        let b = record_with("same", b"bbbbbbbbbbbbbbbb", FAST);
        assert_ne!(a, b);
        assert!(verify_record(&a, "same") && verify_record(&b, "same"));
    }

    /// config.json 是用户可编辑的：任何坏记录都只能判 false，绝不 panic。
    /// 门必须真的接在动作上：托盘与设置窗的启动/停止/重启**六个入口**都得走 `ask_admin`。
    ///
    /// 判别力：把任一处改回直接 `tx.send(UiCmd::Stop)`（绕过门），这条必红 —— 「加了门但只
    /// 接一半」正是这类改动最容易出的漏。
    #[test]
    fn service_actions_are_gated_by_admin_password() {
        let ui = include_str!("ui.rs");
        for verb in ["Start", "Stop", "Restart"] {
            let needle = format!("move || ask(UiCmd::{verb})");
            let n = ui.matches(&needle).count();
            assert!(
                n >= 2,
                "托盘与设置窗都要过门：{needle} 应出现 >=2 次（实际 {n}）"
            );
        }
        assert!(
            ui.contains("admin_pass::verify_record"),
            "门里必须真的校验密码（verify_record），不能只看有没有弹窗"
        );
        assert!(
            ui.contains("admin_pass::new_record") && ui.contains("首次设置管理密码"),
            "未设置时必须能首设（new_record + 明确文案）"
        );
    }

    /// 「取消」必须真的取消：管理密码确认框得接 `canceled` 回调。
    ///
    /// 判别力：v2.23.88 只接了 `on_confirmed`（Slint 的取消按钮不会自己关窗）⇒ 点「取消」
    /// 没有任何反应，窗不关、待执行的服务动作还挂着（owner 2026-10-04 实报）。
    #[test]
    fn admin_dialog_cancel_is_wired() {
        let ui = include_str!("ui.rs");
        assert!(
            ui.contains("svc_pw.on_canceled"),
            "管理密码确认框必须接 canceled：否则点「取消」不关窗、也不丢弃待执行动作"
        );
        assert!(
            ui.contains("[gui] 服务动作已取消（未执行）"),
            "取消要留痕（谁取消了哪个动作，日志里应能查到）"
        );
    }

    #[test]
    fn malformed_records_are_rejected_without_panicking() {
        for bad in [
            "",
            "   ",
            "plaintext",
            "sha256$0$aa$bb",
            "sha256$8$$",
            "sha256$8$zz$bb",
            "sha256$8$aa$bb$cc",
            "md5$8$aa$bb",
        ] {
            assert!(
                !verify_record(bad, "whatever"),
                "坏记录必须判失败：{:?}",
                bad
            );
        }
        assert!(!is_set(""));
        assert!(is_set("sha256$8$aa$bb"));
    }
}
