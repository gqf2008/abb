//! Windows：bridge 常驻**计划任务**（`ABB-Bridge`）的纯逻辑。
//!
//! 真机执行分两处：注册/删除/停止由提权 helper（`src/elev/**`）跑 `schtasks`；
//! 查询与重启由 `platform` 在普通权限下跑。本模块只放**可跨平台单测**的部分——
//! XML 构建与「查询输出是否登记的是当前二进制 + `--service`」的判据（`include_str!`
//! 式的文本判据在 Windows 侧没法本机验，所以做成纯函数、在 macOS 上跑）。
//!
//! 为什么用计划任务而不是 Run 键（批 `abb-svc-persist-password-gate-20260928`）：
//! Run 键拉起的是**托盘**，托盘一退 bridge 就没了；计划任务直接拉 `--service`，且
//! `RunLevel=HighestAvailable` 让 bridge 以**高完整性**跑 —— 同用户的中完整性进程
//! （普通 `taskkill` / 任务管理器）`TerminateProcess` 会被 UAC 的 mandatory integrity
//! control 拒绝，这就是「登录后服务不能被随便杀死」在 Windows 上的实现基础。

#![cfg(any(target_os = "windows", test))]

/// 计划任务名（`schtasks /tn` 用它；改名等于让存量任务变孤儿，只能新增不能改）。
pub const TASK_NAME: &str = "ABB-Bridge";

/// bridge 参数（任务 Action 的命令行）。
pub const SERVICE_ARG: &str = "--service";

/// XML 文本值转义（写侧）。`&`/`<`/`>` 不转义会产出非法 XML，`schtasks /create /xml`
/// 直接报错——与我们「回显说真话」的取向一致：能转义就别让它失败。
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// 生成任务 XML。
///
/// 逐条对齐需求（每条都被单测钉住）：
/// - `LogonTrigger` + `UserId`：**只在该用户登录时**起（不抢别的用户会话）；
/// - `RunLevel=HighestAvailable`：高完整性 → 同用户中完整性进程杀不掉（见模块头）；
/// - `RestartOnFailure` 1 分钟 × 999：真被提权杀/崩溃也会被拉回；
/// - `ExecutionTimeLimit=PT0S`：常驻任务**不超时**（否则会被计划任务自己掐掉）；
/// - `MultipleInstancesPolicy=IgnoreNew`：与 bridge 自身单实例锁双保险；
/// - 电池两项 `false`：笔记本上也要跑（默认会因省电策略不启动/中途停）；
/// - `StartWhenAvailable`：错过的登录触发在可用时补跑。
pub fn task_xml(exe: &str, user: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>ABB bridge（agent-bridge --service）：登录后常驻，托盘退出不影响。</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>{arg}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        user = xml_escape(user),
        exe = xml_escape(exe),
        arg = SERVICE_ARG,
    )
}

/// 判 `schtasks /query /tn … /xml` 的输出是否登记着**当前二进制 + `--service`**。
///
/// 为什么按字节扫而不是解析 XML：`schtasks /query /xml` 的输出编码随控制台代码页走
/// （实测有 UTF-16LE 带 BOM 的情形），且任务 XML 的元素顺序/属性不保证稳定；这里只关心
/// 「exe 路径」与「`--service`」两个串是否出现 —— 在 UTF-8 与 UTF-16LE 两种编码下各试一次。
///
/// 已知边界：这是**判漂移**而不是判等（不校验其它设置项）；设置项由 `task_xml` 单测锁住，
/// 这里只在「换过安装路径/旧版任务」时判 Drifted 并触发重建。
pub fn task_output_matches_exe(output: &[u8], exe: &str) -> bool {
    let has = |needle: &str| {
        // 两种形态各试：XML 里会转义 `&<>`（评审 P6——以前只比裸串，含这些字符的路径永远
        // 判 Drifted），而 `--service` 这种无特殊字符的串转义后与原串相同、不会重复匹配。
        [needle.to_string(), xml_escape(needle)].iter().any(|n| {
            let utf8 = n.as_bytes();
            let utf16: Vec<u8> = n.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
            contains_bytes(output, utf8) || contains_bytes(output, &utf16)
        })
    };
    has(exe) && has(SERVICE_ARG)
}

/// 裸 `windows` crate 之外的字节查找（不引依赖）。
fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > hay.len() {
        return false;
    }
    hay.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = r"C:\Users\sqb\AppData\Local\Programs\ABB\agent-bridge.exe";
    const USER: &str = r"DESKTOP-ABC\sqb";

    /// XML 必须带上那七项设置——本批的「不能被随便杀死」全靠 `RunLevel` + `RestartOnFailure`。
    #[test]
    fn task_xml_pins_the_security_and_persistence_settings() {
        let xml = task_xml(EXE, USER);
        for needle in [
            "<LogonTrigger>",
            "<UserId>DESKTOP-ABC\\sqb</UserId>",
            "<RunLevel>HighestAvailable</RunLevel>",
            "<LogonType>InteractiveToken</LogonType>",
            "<RestartOnFailure>",
            "<Interval>PT1M</Interval>",
            "<Count>999</Count>",
            "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
            "<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
            "<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
            "<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
            "<StartWhenAvailable>true</StartWhenAvailable>",
            SERVICE_ARG,
        ] {
            assert!(xml.contains(needle), "缺 {needle}：\n{xml}");
        }
        assert!(xml.contains(EXE), "必须直接跑当前二进制：\n{xml}");
        // 任务名不在 XML 里（它在 `schtasks /tn` 参数上）。评审 P8 指出原来那条
        // `assert!(xml.contains("ABB-Bridge") || true, …)` 恒真、零判别力 —— 直接删掉，
        // 「注册时确实带着 TASK_NAME」由 `svc_task::TASK_NAME` 的调用点（platform/elev）保证。
    }

    /// exe / 用户名里的 `&`、`<` 必须转义，否则 `schtasks /create /xml` 会拒绝整份 XML。
    #[test]
    fn task_xml_escapes_special_chars() {
        let xml = task_xml(r"C:\a&b\<odd>\agent-bridge.exe", "DOM&AIN\\u<ser");
        assert!(xml.contains("a&amp;b"), "{xml}");
        assert!(xml.contains("&lt;odd&gt;"), "{xml}");
        assert!(xml.contains("DOM&amp;AIN"), "{xml}");
        assert!(!xml.contains("a&b"), "裸 & 不许留：{xml}");
    }

    /// 漂移判据：UTF-8 与 UTF-16LE 两种编码都要认；换过路径/旧版任务要判否。
    #[test]
    fn task_output_match_handles_both_encodings_and_drift() {
        let utf8 = format!("<Command>{EXE}</Command><Arguments>{SERVICE_ARG}</Arguments>");
        assert!(task_output_matches_exe(utf8.as_bytes(), EXE));

        let utf16: Vec<u8> = utf8.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert!(
            task_output_matches_exe(&utf16, EXE),
            "UTF-16LE 输出（schtasks /xml 的常见形态）必须认"
        );

        // 旧安装路径 → 判否（触发重建）
        let other = format!(
            r"<Command>C:\Old\ABB\agent-bridge.exe</Command><Arguments>{SERVICE_ARG}</Arguments>"
        );
        assert!(!task_output_matches_exe(other.as_bytes(), EXE));

        // 没有 --service（旧版任务/写错）→ 判否
        let no_arg = format!("<Command>{EXE}</Command>");
        assert!(!task_output_matches_exe(no_arg.as_bytes(), EXE));

        // 空输出（任务不存在时的 stderr 等）→ 判否
        assert!(!task_output_matches_exe(b"", EXE));
    }
}
