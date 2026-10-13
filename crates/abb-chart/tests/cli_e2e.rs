//! 端到端集成测试：跑真实 CLI 二进制（不是库函数），验证「命令行 → PNG」这条路真的通。
//!
//! 为什么要有这一层（库内单测覆盖不到的地方）：
//! - 退出码约定（0 / 1 / 2）与 stderr 文案——它们是**给模型看的接口**，属于契约；
//! - `--out` 缺省落点、目录自动创建、`--spec -` 走 stdin 这些只在 main 里存在的路径；
//! - 规格文件里的相对 `data` 路径（库测试走的是我们自己传的 `spec_dir`，CLI 走的是
//!   从 `--spec` 推出来的目录——两条路不同）。
//!
//! golden 基线：`tests/golden/cjk_bar.{json,csv,png}`。
//! `cjk_bar.png` 是**人工目视一次的参考产物**（中文标题 + 中文轴标题 + 小数刻度）。
//! 它不参与逐字节比对（不同平台/字体渲染本就不同），但这条测试会重新渲染同一规格并断言
//! 「该有字的地方真有墨迹、该有的尺寸真对得上」——这才是跨平台可判的部分。

use std::process::Command;

/// 被测二进制（cargo 会为集成测试设好 `CARGO_BIN_EXE_<name>`）。
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_abb-chart")
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run(args: &[&str]) -> Run {
    let out = Command::new(bin())
        .args(args)
        .output()
        .expect("启动 abb-chart 失败");
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// 顶部一段（标题带）里有多少列有墨迹。
fn title_band_ink(png: &[u8]) -> usize {
    let img = image::load_from_memory(png).unwrap().to_luma8();
    let rows = (img.height() as f32 * 0.12) as u32;
    (0..img.width())
        .filter(|x| (0..rows).any(|y| img.get_pixel(*x, y).0[0] < 200))
        .count()
}

#[test]
fn help_exits_zero_and_documents_the_contract() {
    let r = run(&["--help"]);
    assert_eq!(r.code, 0, "帮助必须成功退出");
    // 帮助里要有一个能直接抄的完整例子，以及"出图后怎么发"——调用方是模型。
    assert!(r.stdout.contains("abb-chart render --spec"), "{}", r.stdout);
    assert!(
        r.stdout.contains("deliver --to-current"),
        "帮助要写出图后怎么发给用户：{}",
        r.stdout
    );
    // 尺寸口径必须写清楚（否则会被 scale_factor 的 2 倍隐式放大坑到）。
    assert!(r.stdout.contains("最终 PNG 的像素数"), "{}", r.stdout);
}

#[test]
fn render_subcommand_help_is_reachable() {
    let r = run(&["render", "--help"]);
    assert_eq!(r.code, 0);
    assert!(r.stdout.contains("图规格"), "{}", r.stdout);
}

/// 用法错误 = 退出码 2（与「渲染失败」= 1 区分，脚本/agent 要能分辨）。
#[test]
fn usage_errors_exit_with_code_2() {
    let r = run(&["render", "--out", "x.png"]);
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stderr.contains("--spec"), "{}", r.stderr);

    let r = run(&["render", "--bogus"]);
    assert_eq!(r.code, 2, "{}", r.stderr);
}

/// 渲染/规格语义错误 = 退出码 1。
#[test]
fn render_errors_exit_with_code_1() {
    let dir = tempfile::tempdir().unwrap();
    let spec = dir.path().join("s.json");
    // 给上数据，让「未知 mark」这一条真正被触发（否则先报的是缺数据）。
    std::fs::write(
        &spec,
        r#"{"mark":"pie3d","x":"a","y":"b","rows":[{"a":1,"b":2}]}"#,
    )
    .unwrap();
    let r = run(&[
        "render",
        "--spec",
        spec.to_str().unwrap(),
        "--out",
        dir.path().join("o.png").to_str().unwrap(),
    ]);
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stderr.contains("pie3d"), "{}", r.stderr);
}

/// 显式指定本机不存在的字体族必须**非零退出**（上游是静默回落内置西文字体）。
#[test]
fn missing_font_family_is_refused_by_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let spec = dir.path().join("s.json");
    std::fs::write(
        &spec,
        r#"{"mark":"point","title":"销售","x":"x","y":"y","rows":[{"x":1,"y":2}]}"#,
    )
    .unwrap();
    let r = run(&[
        "render",
        "--spec",
        spec.to_str().unwrap(),
        "--out",
        dir.path().join("o.png").to_str().unwrap(),
        "--font",
        "NoSuchFontXYZ123",
    ]);
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stderr.contains("NoSuchFontXYZ123"), "{}", r.stderr);
}

/// golden 规格端到端：相对 `data` 路径按规格文件目录解析、尺寸精确、中文真有墨迹。
#[test]
fn golden_cjk_spec_renders_with_text_ink_and_exact_size() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let spec = dir.join("cjk_bar.json");
    let out = tempfile::tempdir().unwrap();
    let png_path = out.path().join("cjk_bar.png");

    let r = run(&[
        "render",
        "--spec",
        spec.to_str().unwrap(),
        "--out",
        png_path.to_str().unwrap(),
    ]);
    assert_eq!(r.code, 0, "stderr={}", r.stderr);
    // stdout 要报出产物路径与大小：调用方要拿这个路径去 deliver。
    assert!(r.stdout.contains("cjk_bar.png"), "{}", r.stdout);

    let png = std::fs::read(&png_path).unwrap();
    assert!(png.starts_with(&[0x89, b'P', b'N', b'G']), "应为 PNG");

    // 尺寸 = 规格里写的像素数（不得被 scale_factor 隐式放大 2 倍）。
    let img = image::load_from_memory(&png).unwrap();
    assert_eq!((img.width(), img.height()), (900, 520));

    // 标题带必须有墨迹：字体解析失败时得到的是空白标题（中文全丢），这条能兜住。
    let ink = title_band_ink(&png);
    assert!(ink > 40, "标题带墨迹太少（{ink} 列），中文可能没画出来");

    // 参考产物必须与规格同源存在（人工目视基线），防止只留下 png 而丢掉生成它的一半。
    assert!(dir.join("cjk_bar.png").is_file(), "缺少 golden 参考 PNG");
    assert!(dir.join("cjk_bar.csv").is_file(), "缺少 golden 输入 CSV");
}

/// `--spec -` 走 stdin；此时相对 `data` 路径按进程 CWD 解析。
#[test]
fn spec_from_stdin_is_supported() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("d.csv"), "k,v\na,1\nb,2\n").unwrap();
    let out = dir.path().join("o.png");

    use std::io::Write;
    let mut child = Command::new(bin())
        .args([
            "render",
            "--spec",
            "-",
            "--out",
            out.to_str().unwrap(),
            "--width",
            "300",
            "--height",
            "180",
        ])
        .current_dir(dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(br#"{"mark":"bar","x":"k","y":"v","data":"d.csv"}"#)
        .unwrap();
    let out_result = child.wait_with_output().unwrap();
    assert_eq!(
        out_result.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out_result.stderr)
    );
    let img = image::load_from_memory(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!((img.width(), img.height()), (300, 180));
}

/// 缺省 `--out`：落在**规格文件同目录**（而不是 CWD）——受限会话里 CWD 不一定在
/// 工作区内，而产物必须在工作区内才能 `deliver --file` 发出去。
#[test]
fn default_out_lands_next_to_the_spec_not_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let spec = dir.path().join("chart.json");
    std::fs::write(
        &spec,
        r#"{"mark":"point","x":"x","y":"y","rows":[{"x":1,"y":2},{"x":2,"y":3}]}"#,
    )
    .unwrap();
    // 故意在别的目录里跑，验证产物不落在 CWD。
    let cwd = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["render", "--spec", spec.to_str().unwrap()])
        .current_dir(cwd.path())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.path().join("chart.png").is_file(), "应落在规格同目录");
    assert!(!cwd.path().join("chart.png").exists(), "不应落在 CWD");
}

/// 输出目录不存在时自动创建（模型常写 `--out out/chart.png`）。
#[test]
fn missing_out_dir_is_created() {
    let dir = tempfile::tempdir().unwrap();
    let spec = dir.path().join("s.json");
    std::fs::write(
        &spec,
        r#"{"mark":"point","x":"x","y":"y","rows":[{"x":1,"y":2}]}"#,
    )
    .unwrap();
    let out = dir.path().join("nested/deeper/o.png");
    let r = run(&[
        "render",
        "--spec",
        spec.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(out.is_file());
}
