//! `abb-chart` 入口：解析参数 → 渲染 → 写文件。
//!
//! 退出码约定（调用方是模型写的 shell 命令，必须可判）：
//! - `0` 成功（含 `--help`）；
//! - `2` 用法错误（参数写错，改命令即可）；
//! - `1` 渲染/装载失败（数据、字体、规格语义问题）。
//!
//! 所有失败都往 stderr 写**可操作**的一句话（缺什么、怎么写），因为拿到这句话的是模型：
//! 它要能照着改，而不是只看到 "error"。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use abb_chart::cli::{parse_args, Invocation};
use abb_chart::spec::render_to_png;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(msg)) => {
            eprintln!("{msg}\n");
            eprintln!("{}", short_usage());
            ExitCode::from(2)
        }
        Err(Failure::Render(msg)) => {
            eprintln!("出图失败：{msg}");
            ExitCode::from(1)
        }
    }
}

enum Failure {
    Usage(String),
    Render(String),
}

impl From<abb_chart::UsageError> for Failure {
    fn from(value: abb_chart::UsageError) -> Self {
        Failure::Usage(value.0)
    }
}

impl From<abb_chart::RenderError> for Failure {
    fn from(value: abb_chart::RenderError) -> Self {
        Failure::Render(value.0)
    }
}

/// 错误后面附的短用法（不用整篇帮助刷屏，但要让对方能立刻改对）。
fn short_usage() -> &'static str {
    "用法：abb-chart render --spec <spec.json|-> [--out a.png] [--width N] [--height N]\n\
     \x20     用 `abb-chart render --help` 看完整说明与例子。"
}

fn run(args: &[String]) -> Result<(), Failure> {
    let (spec_arg, mut options) = match parse_args(args)? {
        Invocation::Help(text) => {
            println!("{text}");
            return Ok(());
        }
        Invocation::Render { spec, options } => (spec, options),
    };

    // 规格来源：文件或 stdin。
    let (spec_text, spec_dir) = match spec_arg.as_deref() {
        Some(p) if p.as_os_str() != "-" => {
            let text = std::fs::read_to_string(p).map_err(|e| {
                Failure::Render(format!("规格文件读取失败：{}（{e}）", p.display()))
            })?;
            let dir = p
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .map(Path::to_path_buf);
            (text, dir)
        }
        Some(_) => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| Failure::Render(format!("从标准输入读规格失败：{e}")))?;
            (buf, None)
        }
        None => {
            return Err(Failure::Usage(
                "缺少 `--spec`（规格文件路径，或 `-` 表示标准输入）".into(),
            ))
        }
    };

    if options.out.as_os_str().is_empty() {
        options.out = default_out_path(spec_arg.as_deref(), &spec_text)?;
    }
    if let Some(parent) = options.out.parent().filter(|p| !p.as_os_str().is_empty()) {
        if !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Failure::Render(format!("输出目录创建失败：{}（{e}）", parent.display()))
            })?;
        }
    }

    let png = render_to_png(&spec_text, spec_dir.as_deref(), &options)?;
    std::fs::write(&options.out, &png).map_err(|e| {
        Failure::Render(format!(
            "写入 {} 失败：{e}（注意：受限会话里输出必须落在工作区内）",
            options.out.display()
        ))
    })?;
    // 成功时把路径与大小打印到 stdout：调用方（模型）要拿这个路径去 deliver。
    println!("已生成 {}（{} 字节）", options.out.display(), png.len());
    Ok(())
}

/// 缺省输出路径。
///
/// 刻意**落在规格文件同目录**而不是 CWD：受限会话的 CWD 与会话工作区不一定是同一处，
/// 而「输出在工作区内」是 `deliver --file` 能成功的前提（guard 会校验）。
fn default_out_path(spec_arg: Option<&Path>, spec_text: &str) -> Result<PathBuf, Failure> {
    if let Some(p) = spec_arg.filter(|p| p.as_os_str() != "-") {
        let stem = p.file_stem().unwrap_or_default();
        return Ok(p.with_file_name(format!("{}.png", stem.to_string_lossy())));
    }
    // stdin：规格里若有 `out` 之类的建议位置就无从得知，落到 CWD 的 chart.png。
    if spec_text.trim().is_empty() {
        return Err(Failure::Usage("标准输入的规格是空的".into()));
    }
    Ok(PathBuf::from("chart.png"))
}
