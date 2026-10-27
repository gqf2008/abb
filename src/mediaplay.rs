//! **音视频内联播放**（稿 v16 的 A 方案）。Slint 没有 `audio`/`video` 元素，所以播放由我们
//! 自己做：调本机 `ffmpeg`/`ffprobe`/`ffplay`（macOS 上 `afplay` 也在，但 `ffplay -nodisp`
//! 对「视频里的音轨」同样有效）。
//!
//! 三件事分开：
//! 1. [`probe`]：`ffprobe` 取时长/分辨率/有没有音视频轨（纯读，微秒级）；
//! 2. [`poster`]：`ffmpeg -frames:v 1` 抽**首帧**当海报，落 `media-cache`（与图片缓存同目录），
//!    按「路径 + 目标宽」hash 命名 ⇒ 同一文件只抽一次；
//! 3. [`Player`]：`ffmpeg ... -f rawvideo -pix_fmt rgba -` 把帧**流式**喂回来（内存有界，
//!    不落整段帧到磁盘），音频另起 `ffplay -nodisp -autoexit`。
//!
//! ⚠️ 设计边界（如实记）：
//! - 只在 **UI 线程之外**读管道（后台线程 + `mpsc`），UI 侧每拍只取**最新**一帧（丢中间帧＝
//!   掉帧，不排队堆积）；
//! - 暂停/继续是**重启 ffmpeg**（带 `-ss` 偏移），不是真的 pause 进程；对短片子足够，
//!   对长片子会有一次重新解码的延迟；
//! - 音视频**不严格同步**（两条独立子进程）：漂移在短视频里可以接受，长视频需要合并成一条
//!   带音频的管线（未做）。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, TryRecvError};

/// `ffprobe` 读出来的媒体信息。
#[derive(Debug, Clone, PartialEq)]
pub struct MediaInfo {
    /// 时长（毫秒；0 = 未知）。
    pub duration_ms: u64,
    /// 视频轨的宽高（只有音频时为 `None`）。
    pub video: Option<(u32, u32)>,
    pub has_audio: bool,
}

fn have_ffmpeg() -> bool {
    have("ffmpeg")
}

fn have(bin: &str) -> bool {
    Command::new(bin)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 探测媒体信息（`ffprobe` 不在/文件坏 ⇒ `None`，调用方回落成「一行链接」）。
pub fn probe(path: &Path) -> Option<MediaInfo> {
    if !have("ffprobe") || !path.is_file() {
        return None;
    }
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            // ⚠️ `-show_entries` 只能给一次（重复出现后者覆盖前者）：宽高与时长要写在同一串里，
            // 分两条写的话 `stream=width,height` 会被 `format=duration` 顶掉 ⇒ 视频尺寸恒为 None。
            "-show_entries",
            "stream=width,height:format=duration",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(path)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let mut w = 0u32;
    let mut h = 0u32;
    let mut dur_s = 0f64;
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k {
            "width" => w = v.trim().parse().unwrap_or(0),
            "height" => h = v.trim().parse().unwrap_or(0),
            "duration" => dur_s = v.trim().parse().unwrap_or(0.0),
            _ => {}
        }
    }
    // 有没有音轨：单独再问一次（上面的 `-select_streams v:0` 只看视频）。
    let audio = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .ok()
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false);
    Some(MediaInfo {
        duration_ms: (dur_s * 1000.0) as u64,
        video: (w > 0 && h > 0).then_some((w, h)),
        has_audio: audio,
    })
}

/// `probe` 的**进程内 memo**：行会在流式期间被反复重建，而 `ffprobe` 是子进程调用
/// （~10–30ms）⇒ 不缓存的话每个分片都要 fork 一次。键 = 路径。
pub fn probe_cached(path: &Path) -> Option<MediaInfo> {
    thread_local! {
        static MEMO: std::cell::RefCell<
            std::collections::HashMap<std::path::PathBuf, Option<MediaInfo>>,
        > = std::cell::RefCell::new(std::collections::HashMap::new());
    }
    let key = path.to_path_buf();
    if let Some(hit) = MEMO.with(|m| m.borrow().get(&key).cloned()) {
        return hit;
    }
    let v = probe(path);
    MEMO.with(|m| {
        let mut g = m.borrow_mut();
        if g.len() > 256 {
            g.clear();
        }
        g.insert(key, v.clone());
    });
    v
}

/// 海报帧（首帧）的缓存路径：`<media-cache>/poster-<hash>-<宽>.png`（`pub` 供测试与清理）。
pub fn poster_cache_path(path: &Path, width: u32) -> PathBuf {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(path.to_string_lossy().as_bytes());
    h.update(width.to_le_bytes());
    let hex = format!("{:x}", h.finalize());
    crate::syschat::dir()
        .join("media-cache")
        .join(format!("poster-{hex}.png"))
}

/// 抽首帧当海报（已缓存直接返回）。只有音频轨 / ffmpeg 缺失 / 解码失败 ⇒ `None`。
pub fn poster(path: &Path, width: u32) -> Option<PathBuf> {
    let dest = poster_cache_path(path, width);
    if dest.is_file() {
        return Some(dest);
    }
    if !have_ffmpeg() || !path.is_file() {
        return None;
    }
    std::fs::create_dir_all(dest.parent()?).ok()?;
    let tmp = dest.with_extension("tmp.png");
    let ok = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(path)
        .args(["-frames:v", "1", "-vf", &format!("scale={width}:-2")])
        .arg(&tmp)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok || !tmp.is_file() {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    std::fs::rename(&tmp, &dest).ok()?;
    Some(dest)
}

/// 一帧 RGBA（`w × h`，长度 = `w*h*4`）。
pub struct Frame {
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
}

/// 正在播放的媒体：视频帧由后台线程读 ffmpeg 的 stdout，音频由 `ffplay -nodisp` 出声。
pub struct Player {
    video: Option<Child>,
    audio: Option<Child>,
    rx: Option<Receiver<Frame>>,
    /// 已推给 UI 的帧数（进度 = `frames / fps`）。
    pub frames: u64,
    fps: f64,
    /// 起播偏移（ms）——seek 靠「停掉再用新偏移重启」实现。
    pub from_ms: u64,
}

impl Player {
    /// 起播。`from_ms` = 偏移；`width` = 目标宽（高按源比例算）。
    /// `has_audio` 时另起一条 `ffplay`（`-nodisp` 只出声不开窗）。
    pub fn start(path: &Path, from_ms: u64, width: u32, info: &MediaInfo) -> Option<Self> {
        if !have_ffmpeg() {
            return None;
        }
        let width = width.max(64) & !1; // 偶数，省得 scale 出半像素
        let fps = 24.0;
        let mut video = None;
        let mut rx = None;
        if let Some((sw, sh)) = info.video {
            // 等比缩放到目标宽（高取偶数）。
            let height = (((sh as f64 / sw as f64) * width as f64).round() as u32).max(2) & !1;
            let mut cmd = Command::new("ffmpeg");
            cmd.args([
                "-v",
                "error",
                "-ss",
                &format!("{:.3}", from_ms as f64 / 1000.0),
            ])
            .arg("-i")
            .arg(path)
            .args([
                "-vf",
                &format!("scale={width}:{height}"),
                "-r",
                &format!("{fps}"),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "-",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .stdin(Stdio::null());
            let mut child = cmd.spawn().ok()?;
            let mut out = child.stdout.take()?;
            let (tx, rxc) = std::sync::mpsc::channel::<Frame>();
            let frame_bytes = (width * height * 4) as usize;
            std::thread::spawn(move || {
                let mut buf = vec![0u8; frame_bytes];
                loop {
                    // 读满一帧；EOF/短读就结束（丢半帧）。
                    let mut filled = 0usize;
                    while filled < frame_bytes {
                        match out.read(&mut buf[filled..]) {
                            Ok(0) => return,
                            Ok(n) => filled += n,
                            Err(_) => return,
                        }
                    }
                    if tx
                        .send(Frame {
                            rgba: buf.clone(),
                            w: width,
                            h: height,
                        })
                        .is_err()
                    {
                        return; // UI 侧不要了（停了/换片子了）
                    }
                }
            });
            video = Some(child);
            rx = Some(rxc);
        }
        let mut audio = None;
        if info.has_audio {
            // `-nodisp`：只出声不开窗；`-autoexit`：播完自己退。
            let child = Command::new("ffplay")
                .args([
                    "-nodisp",
                    "-autoexit",
                    "-loglevel",
                    "error",
                    "-ss",
                    &format!("{:.3}", from_ms as f64 / 1000.0),
                ])
                .arg(path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .stdin(Stdio::null())
                .spawn()
                .ok();
            audio = child;
        }
        Some(Player {
            video,
            audio,
            rx,
            frames: 0,
            fps,
            from_ms,
        })
    }

    /// 取**最新**一帧（不排队：中间帧直接丢）。没有新帧返回 `None`。
    pub fn next_frame(&mut self) -> Option<Frame> {
        let rx = self.rx.as_ref()?;
        let mut latest = None;
        loop {
            match rx.try_recv() {
                // ⚠️ 帧计数要按**收下的帧数**加（不是「drain 了几次」）：进度 = 帧数/fps，
                // 一次 drain 可能吞掉好几帧（走神那一拍），按 drain 计数会让进度落后于画面。
                Ok(f) => {
                    self.frames += 1;
                    latest = Some(f);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        latest
    }

    /// 已播到的位置（ms）——按帧数 / fps 估（音视频两条管线不严格同步，见模块顶注）。
    pub fn position_ms(&self) -> u64 {
        self.from_ms + (self.frames as f64 / self.fps * 1000.0) as u64
    }

    /// 停：杀掉两条子进程（幂等）。
    pub fn stop(&mut self) {
        for c in [self.video.as_mut(), self.audio.as_mut()]
            .into_iter()
            .flatten()
        {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.video = None;
        self.audio = None;
        self.rx = None;
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一段 1 秒的测试视频（需要 ffmpeg；没有就跳过，不让门禁在这台机器之外变红）。
    fn make_sample(dir: &Path) -> Option<PathBuf> {
        if !have_ffmpeg() {
            return None;
        }
        std::fs::create_dir_all(dir).ok()?;
        let p = dir.join("sample.mp4");
        let ok = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=160x120:rate=10:duration=1",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&p)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        ok.then_some(p)
    }

    /// 探测：能读到时长与视频尺寸；坏文件/缺 ffprobe ⇒ `None`。
    #[test]
    fn probe_reads_duration_and_size() {
        let dir = std::env::temp_dir().join(format!("abb-mediaplay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let Some(p) = make_sample(&dir) else {
            return; // 没有 ffmpeg：跳过（不是失败）
        };
        let info = probe(&p).expect("应能探测到测试视频");
        assert_eq!(info.video, Some((160, 120)));
        assert!((info.duration_ms as i64 - 1000).abs() < 300, "{info:?}");
        assert!(!probe(Path::new("/no/such/file.mp4")).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 海报：抽出首帧且**第二次命中缓存**（同路径同宽 ⇒ 同一个文件）。
    #[test]
    fn poster_is_extracted_and_cached() {
        let dir = std::env::temp_dir().join(format!("abb-mediaplay-poster-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let Some(p) = make_sample(&dir) else {
            return;
        };
        let a = poster(&p, 96).expect("应能抽到海报");
        assert!(a.is_file());
        let first_len = std::fs::metadata(&a).map(|m| m.len()).unwrap_or(0);
        let b = poster(&p, 96).expect("第二次应命中缓存");
        assert_eq!(a, b, "同一路径 + 同一宽 ⇒ 同一个缓存文件");
        assert!(first_len > 0);
        let _ = std::fs::remove_dir_all(&dir);
        // 海报缓存落在 AGENT_BRIDGE_HOME 下；隔离门禁要求测试不留运行数据文件。
        let _ = std::fs::remove_file(&a);
    }

    /// 播放器：起播后能拿到帧，`stop()` 幂等且能再次起播。
    #[test]
    fn player_streams_frames_and_stops() {
        let dir = std::env::temp_dir().join(format!("abb-mediaplay-play-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let Some(p) = make_sample(&dir) else {
            return;
        };
        let info = probe(&p).expect("probe");
        let mut pl = Player::start(&p, 0, 96, &info).expect("起播");
        // `next_frame` 的口径是「**取最新一帧**」（中间帧故意丢：UI 每拍只画一张，排队只会越积
        // 越多）⇒ 不能按「调用几次拿到几帧」计数，要看播放器内部的帧计数 `frames`。
        let mut seen: Option<Frame> = None;
        for _ in 0..200 {
            if let Some(f) = pl.next_frame() {
                assert_eq!(f.w, 96);
                assert_eq!(f.h, 72);
                assert_eq!(f.rgba.len(), (f.w * f.h * 4) as usize);
                seen = Some(f);
            }
            if pl.frames >= 3 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(seen.is_some(), "应至少拿到一帧");
        assert!(pl.frames >= 3, "内部帧计数应 ≥ 3（现在 {}）", pl.frames);
        assert!(pl.position_ms() > 0);
        pl.stop();
        pl.stop(); // 幂等
        let _ = std::fs::remove_dir_all(&dir);
    }
}
