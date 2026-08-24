//! 版本检测 + 自动升级安装（托盘菜单「检查更新」）。
//!
//! 链路：GitHub API 查 latest release → 与 CARGO_PKG_VERSION 比 semver →
//! 有新版本则按平台选资产（macOS=dmg / Windows=Setup exe / Linux 无预编译包）→
//! 下载到临时目录 → 平台安装：
//! - macOS：hdiutil 挂载 dmg → 旧 bundle 改名留备份 → ditto 新 bundle 原位 →
//!   分离式 sh 等本进程退出后 `open` 新实例（单实例锁随进程死亡释放，见 single_instance.rs）。
//! - Windows：直接启动 Inno Setup 安装包（PrivilegesRequired=lowest 免 UAC），
//!   安装器自己处理覆盖与装完重启；本进程随即退出让出文件锁。
//! - Linux：CI 不出包，调用方拿到 asset_url=None，UI 提示手动构建。

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};

/// 当前版本（构建期锁定）。
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

const REPO: &str = "gqf2008/abb";

/// 一次版本检查的结果。
#[derive(Debug, Clone)]
pub struct LatestRelease {
    /// 去掉 v 前缀的版本号，如 "2.15.0"。
    pub version: String,
    /// 本机平台的资产下载 URL；该平台没有预编译包（Linux）时为 None。
    pub asset_url: Option<String>,
    /// 本机平台安装包在 SHA256SUMS 里的期望哈希（hex 小写）；校验不可用时为
    /// None——下载后校验环节会拒绝安装（fail-closed），成因看 sums_state。
    pub asset_sha256: Option<String>,
    /// 校验不可用的成因：清单缺失（release 有缺陷）vs 拉取失败（网络问题可重试）。
    #[allow(dead_code)] // install 路径暂只消费 Ok/非 Ok 二分；细分供日志/提示用
    pub sums_state: SumsState,
}

/// SHA256SUMS 可用状态。verify_sha256 对 Missing/FetchFailed 都拒装，
/// 区分成因只为日志/提示能区分"该上报 release 问题"和"该重试"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SumsState {
    /// 已取到本平台期望哈希。
    Ok(String),
    /// release 未附 SHA256SUMS 或无本平台条目——发版缺陷，如实上报。
    Missing,
    /// 清单存在但拉取失败——网络抖动，重试可能恢复。
    FetchFailed(String),
}

/// 有状态的升级器：复用同一个 reqwest Client（连接池/rustls 配置）。
pub struct Updater {
    client: reqwest::Client,
}

impl Updater {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            // GitHub API 没有 UA 直接 403
            .user_agent(concat!("abb-updater/", env!("CARGO_PKG_VERSION")))
            // 死路由快速失败（默认等 OS TCP 超时 ~75s，重试全耗在等待上）
            .connect_timeout(std::time::Duration::from_secs(20))
            .build()
            .context("构建 HTTP client 失败")?;
        Ok(Self { client })
    }

    /// 查 GitHub latest release。只取 tag_name + 资产名/URL，不解析 body。
    pub async fn check_latest(&self) -> Result<LatestRelease> {
        let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
        let resp = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .context("请求 GitHub releases 失败（网络/代理？）")?;
        if !resp.status().is_success() {
            bail!("GitHub API 返回 {}", resp.status());
        }
        let v: serde_json::Value = resp.json().await.context("解析 release JSON 失败")?;
        let tag = v
            .get("tag_name")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow!("release 缺 tag_name"))?;
        let version = tag.strip_prefix('v').unwrap_or(tag).to_string();
        let names: Vec<String> = v
            .get("assets")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| a.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let urls: Vec<String> = v
            .get("assets")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| {
                        a.get("browser_download_url")
                            .and_then(|n| n.as_str())
                            .map(String::from)
                    })
                    .collect()
            })
            .unwrap_or_default();
        // 本平台无资产（Linux / 资产命名漂移）→ 保持旧版 None 语义：
        // UI 的 update_can_install=false 分支显示"请从源码更新"，不当作检查失败。
        let (asset_name, asset_url) = pick_asset(&names, &version)
            .and_then(|picked| {
                names
                    .iter()
                    .position(|n| *n == picked)
                    .and_then(|i| urls.get(i).cloned().map(|u| (picked, u)))
            })
            .map(|(name, url)| (Some(name), Some(url)))
            .unwrap_or((None, None));
        // SHA256SUMS：release 附带则解析出本平台安装包的期望哈希（校验用）。
        // 拉取失败与清单缺失区分开：前者 FetchFailed（网络抖动可重试），
        // 后者 Missing（发版缺陷，fail-closed 拒装并如实上报）。
        let sums_state = match v.get("assets").and_then(|a| a.as_array()).and_then(|arr| {
            arr.iter().find_map(|a| {
                let n = a.get("name").and_then(|n| n.as_str())?;
                if n != SHASUMS_NAME {
                    return None;
                }
                a.get("browser_download_url")
                    .and_then(|u| u.as_str())
                    .map(String::from)
            })
        }) {
            Some(u) => match self.fetch_shasums(&u).await {
                Ok(map) => match asset_name.as_deref().and_then(|n| map.get(n)) {
                    Some(h) => SumsState::Ok(h.clone()),
                    None => SumsState::Missing, // 清单在但没有本平台条目（发版配置错误）
                },
                Err(e) => {
                    crate::log!("[update] 拉 SHA256SUMS 失败（网络问题，可重试）：{e:#}");
                    SumsState::FetchFailed(e.to_string())
                }
            },
            None => SumsState::Missing,
        };
        Ok(LatestRelease {
            version,
            asset_url,
            asset_sha256: match &sums_state {
                SumsState::Ok(h) => Some(h.clone()),
                _ => None,
            },
            sums_state,
        })
    }

    /// 拉取并解析 SHA256SUMS（`<sha256-hex>  <文件名>` 两空格格式，sha256sum -c 兼容）。
    async fn fetch_shasums(&self, url: &str) -> Result<std::collections::HashMap<String, String>> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .context("请求 SHA256SUMS 失败")?;
        if !resp.status().is_success() {
            bail!("SHA256SUMS 下载返回 {}", resp.status());
        }
        let text = resp.text().await.context("读 SHA256SUMS 失败")?;
        Ok(parse_shasums(&text))
    }

    /// 流式下载到目标文件（逐块写盘，不把整个 dmg 堆进内存）。
    /// `on_progress(已下载字节, 总字节)`：总字节取 content-length，响应头没有时为 None。
    /// GitHub 资产会 302 到下载 CDN，部分网络下 connect 抖动（实测连续超时后重试又能下完）：
    /// 最多 3 次、递增退避；不做断点续传（包不大，整体重下简单可靠）。
    pub async fn download_to(
        &self,
        url: &str,
        dest: &Path,
        on_progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
    ) -> Result<()> {
        let mut last_err = None;
        for attempt in 1..=3u32 {
            match self.download_once(url, dest, on_progress).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    crate::log!("[update] 下载第 {attempt}/3 次失败：{e:#}");
                    last_err = Some(e);
                    if attempt < 3 {
                        tokio::time::sleep(std::time::Duration::from_secs(5 * u64::from(attempt)))
                            .await;
                    }
                }
            }
        }
        Err(last_err.expect("至少尝试过一次"))
            .context("下载重试 3 次均失败：本机网络到 GitHub 下载 CDN 不通，可挂代理后重试，或到 release 页手动下载安装")
    }

    /// 单次下载尝试（download_to 的重试单元）。
    async fn download_once(
        &self,
        url: &str,
        dest: &Path,
        on_progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
    ) -> Result<()> {
        use futures_util::StreamExt;
        use std::io::Write;
        let resp = self.client.get(url).send().await.context("下载请求失败")?;
        if !resp.status().is_success() {
            bail!("下载返回 {}", resp.status());
        }
        let total = resp.content_length();
        let mut file = std::fs::File::create(dest).context("创建临时文件失败")?;
        let mut stream = resp.bytes_stream();
        let mut done: u64 = 0;
        on_progress(0, total);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("下载流中断")?;
            file.write_all(&chunk).context("写临时文件失败")?;
            done += chunk.len() as u64;
            on_progress(done, total);
        }
        file.flush().ok();
        Ok(())
    }
}

/// 解析 "2.15.0" / "v2.15.0" / "2.15.0-beta1" 为 (2,15,0)。非数字段截断；缺段补 0。
fn parse_semver(s: &str) -> (u32, u32, u32) {
    let s = s.strip_prefix('v').unwrap_or(s);
    let mut parts = s.split('.').map(|seg| {
        let digits: String = seg.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse().unwrap_or(0)
    });
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

/// latest 是否比 current 新（纯 semver 元组比较）。
pub fn is_newer(latest: &str, current: &str) -> bool {
    parse_semver(latest) > parse_semver(current)
}

/// release 资产里的校验清单文件名（CI 打包时生成上传）。
pub const SHASUMS_NAME: &str = "SHA256SUMS";

/// 解析 SHA256SUMS 文本 → {文件名: hex小写哈希}。容忍多空白与 CRLF；
/// 注释行（# 开头）与残缺行跳过。
fn parse_shasums(text: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        let Some(hash) = it.next() else { continue };
        let Some(name) = it.next() else { continue };
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        map.insert(name.to_string(), hash.to_ascii_lowercase());
    }
    map
}

/// 下载完成后校验产物哈希。expected=None = 校验不可用（清单缺失或拉取失败）
/// → 拒绝安装（fail-closed：无校验不装，宁可不升级也不执行来源未证明的安装包）。
pub fn verify_sha256(
    file: &std::path::Path,
    name_for_log: &str,
    expected: Option<&str>,
) -> Result<()> {
    use sha2::Digest;
    let expected = expected.ok_or_else(|| {
        anyhow!(
            "release 未附 {}，无法校验安装包完整性（安全策略拒绝安装；若网络波动可稍后重试检查更新）",
            SHASUMS_NAME
        )
    })?;
    // 流式哈希：与 download_to 的流式写盘同风格，不把整个安装包堆进内存。
    let f =
        std::fs::File::open(file).with_context(|| format!("读安装包失败: {}", file.display()))?;
    let mut reader = std::io::BufReader::new(f);
    let mut h = sha2::Sha256::new();
    std::io::copy(&mut reader, &mut h).context("流式读取安装包失败")?;
    let actual: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if actual != expected.to_ascii_lowercase() {
        crate::log!(
            "[update] 校验失败 {}: 期望 {} 实得 {}",
            name_for_log,
            expected,
            actual
        );
        bail!(
            "安装包 sha256 与 {} 不符（下载损坏或被篡改），已拒绝安装",
            SHASUMS_NAME
        );
    }
    Ok(())
}

/// 按平台从资产名列表里挑安装包（纯函数，便于单测）：
/// macOS → ABB-x.y.z.dmg；Windows → ABB-Setup-x.y.z.exe；Linux → None。
/// 先精确匹配版本号，再退后缀匹配（防 CI 命名微调）。
pub fn pick_asset(names: &[String], version: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let exact = format!("ABB-{version}.dmg");
        names
            .iter()
            .find(|n| **n == exact)
            .or_else(|| names.iter().find(|n| n.ends_with(".dmg")))
            .cloned()
    }
    #[cfg(target_os = "windows")]
    {
        let exact = format!("ABB-Setup-{version}.exe");
        names
            .iter()
            .find(|n| **n == exact)
            .or_else(|| {
                names
                    .iter()
                    .find(|n| n.starts_with("ABB-Setup-") && n.ends_with(".exe"))
            })
            .cloned()
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = (names, version);
        None
    }
}

/// 下载产物在本机的落点（临时目录，按版本命名防串）。
pub fn download_dest(version: &str) -> PathBuf {
    #[cfg(target_os = "macos")]
    let name = format!("ABB-{version}.dmg");
    #[cfg(target_os = "windows")]
    let name = format!("ABB-Setup-{version}.exe");
    #[cfg(all(unix, not(target_os = "macos")))]
    let name = format!("ABB-{version}.bin");
    std::env::temp_dir().join(name)
}

/// 本平台安装包的资产文件名（校验/日志用；与 pick_asset/download_dest 命名一致）。
pub fn asset_file_name(version: &str) -> String {
    #[cfg(target_os = "macos")]
    let name = format!("ABB-{version}.dmg");
    #[cfg(target_os = "windows")]
    let name = format!("ABB-Setup-{version}.exe");
    #[cfg(all(unix, not(target_os = "macos")))]
    let name = format!("ABB-{version}.bin");
    name
}

/// 安装并重启。成功返回后**调用方负责退出本进程**（macOS 由分离 sh 等进程死后拉起新实例；
/// Windows 安装器装完自己拉起）。Linux 不应被调用（无资产）。
pub fn install_and_relaunch(file: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos_install(file)
    }
    #[cfg(target_os = "windows")]
    {
        windows_install(file)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = file;
        bail!("Linux 暂无预编译包，请 git pull && cargo build --release 手动升级")
    }
}

/// macOS：dmg → 替换当前 bundle → 分离脚本等本进程死后 open 新实例。
#[cfg(target_os = "macos")]
fn macos_install(dmg: &Path) -> Result<()> {
    use std::process::Command;
    // 0. 当前必须跑在 .app 里（开发版 target/debug/... 直接拒，引导手动装）
    let exe = std::env::current_exe().context("取当前 exe 路径失败")?;
    // ABB.app/Contents/MacOS/agent-bridge → 上 3 级 = bundle
    let bundle = exe
        .ancestors()
        .nth(3)
        .filter(|p| p.extension().is_some_and(|e| e == "app"))
        .map(|p| p.to_path_buf())
        .ok_or_else(|| anyhow!("当前不是安装版（未在 .app 内运行），请从 release 页手动下载"))?;

    // 1. 挂载 dmg 到独立挂载点（-nobrowse 不弹 Finder 窗）
    let mnt = std::env::temp_dir().join(format!("abb-update-mnt-{}", std::process::id()));
    let st = Command::new("hdiutil")
        .arg("attach")
        .arg("-nobrowse")
        .arg("-readonly")
        .arg("-mountpoint")
        .arg(&mnt)
        .arg(dmg)
        .status()
        .context("hdiutil attach 启动失败")?;
    if !st.success() {
        bail!("hdiutil attach 失败（dmg 损坏？）：{st}");
    }
    // 挂载后的一切失败都要尝试 detach，别留垃圾挂载
    let r = macos_install_from_mnt(&mnt, &bundle);
    let _ = Command::new("hdiutil")
        .arg("detach")
        .arg("-quiet")
        .arg(&mnt)
        .status();
    r
}

#[cfg(target_os = "macos")]
fn macos_install_from_mnt(mnt: &Path, bundle: &Path) -> Result<()> {
    use std::process::Command;
    let new_app = mnt.join("ABB.app");
    if !new_app.exists() {
        bail!("dmg 里没有 ABB.app（包内容变了？）");
    }
    // 2. 旧 bundle 改名留备份（同目录 rename，快；失败可回滚）
    let backup = bundle.with_file_name(format!("ABB.old-{}.app", std::process::id()));
    std::fs::rename(bundle, &backup).context("移走旧版失败（/Applications 无写权限？）")?;
    // 3. ditto 新 bundle 到原位（保留签名/资源；cp -R 也行，ditto 更稳）
    let st = Command::new("ditto")
        .arg(&new_app)
        .arg(bundle)
        .status()
        .context("ditto 启动失败")?;
    if !st.success() {
        // 回滚旧版
        let _ = std::fs::remove_dir_all(bundle);
        let _ = std::fs::rename(&backup, bundle);
        bail!("ditto 拷新包失败：{st}（已回滚旧版）");
    }
    // 4. 删备份（尽力；失败留到下次清理也无碍）
    let _ = std::fs::remove_dir_all(&backup);
    // 5. 分离 sh：等本进程彻底退出（单实例锁释放）后再 open 新实例。
    //    不用 -n：进程已死，普通 open 即可；万一 open 早于退出，重试兜底。
    let pid = std::process::id();
    let b = bundle.to_string_lossy();
    Command::new("sh")
        .arg("-c")
        .arg(format!(
            "while kill -0 {pid} 2>/dev/null; do sleep 0.2; done; sleep 0.3; open \"{b}\""
        ))
        .spawn()
        .context("启动重启辅助脚本失败")?;
    Ok(())
}

/// Windows：启动 Inno 安装包（per-user 安装免 UAC；安装器装完按其 [Run] 段拉起新实例）。
#[cfg(target_os = "windows")]
fn windows_install(setup: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    // start 把首个带引号参数当窗口标题，故先给空标题；CREATE_NO_WINDOW 避免闪控制台。
    std::process::Command::new("cmd")
        .arg("/c")
        .arg("start")
        .arg("")
        .arg(setup)
        .creation_flags(0x0800_0000)
        .spawn()
        .context("启动安装包失败")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shasums_parse() {
        let text = "# comment\naaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  ABB-2.15.0.dmg\r\ndef456  ABB-Setup-2.15.0.exe\nshort  bad.txt\n";
        let m = parse_shasums(text);
        assert_eq!(
            m.get("ABB-2.15.0.dmg").map(String::as_str),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(m.len(), 1); // 残缺行跳过（def456/short 均不足 64 位 hex）
                                // 大写哈希归一为小写
        let up = parse_shasums(
            "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789  x.dmg",
        );
        assert_eq!(
            up.get("x.dmg").map(String::as_str),
            Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789")
        );
    }

    #[test]
    fn sums_state_distinction_in_verify_error() {
        // Missing 与 FetchFailed 都映射为 expected=None → 拒装；
        // 错误信息统一引导（区分成因在 check_latest 日志层完成）。
        let f = std::env::temp_dir().join(format!("abb-sums-state-{}", uuid::Uuid::new_v4()));
        std::fs::write(&f, b"x").unwrap();
        assert!(verify_sha256(&f, "x", None).is_err());
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn verify_rejects_missing_sums_fail_closed() {
        let f = std::env::temp_dir().join(format!("abb-verify-test-{}", uuid::Uuid::new_v4()));
        std::fs::write(&f, b"data").unwrap();
        // release 无 SHA256SUMS → 拒绝安装（fail-closed）
        assert!(verify_sha256(&f, "x.dmg", None).is_err());
        // 有清单且哈希匹配 → 通过
        let hash = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(b"data");
            h.finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        assert!(verify_sha256(&f, "x.dmg", Some(&hash)).is_ok());
        // 不匹配 → 拒绝
        assert!(verify_sha256(&f, "x.dmg", Some(&"0".repeat(64))).is_err());
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn semver_compare() {
        assert!(is_newer("2.15.0", "2.14.2"));
        assert!(is_newer("v2.15.0", "2.14.2")); // v 前缀容忍
        assert!(is_newer("3.0.0", "2.99.99"));
        assert!(!is_newer("2.14.2", "2.14.2")); // 同版不升级
        assert!(!is_newer("2.14.1", "2.14.2")); // 旧版不升级
        assert!(!is_newer("2.14", "2.14.2")); // 缺段补 0 → 2.14.0 < 2.14.2
        assert!(is_newer("2.15.0-beta1", "2.14.9")); // 后缀截断按数字段比
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pick_asset_macos() {
        let names = vec![
            "ABB-2.15.0.dmg".to_string(),
            "ABB-Setup-2.15.0.exe".to_string(),
        ];
        assert_eq!(
            pick_asset(&names, "2.15.0"),
            Some("ABB-2.15.0.dmg".to_string())
        );
        // 精确匹配失败时退后缀
        let loose = vec!["ABB-2.15.0-arm64.dmg".to_string()];
        assert_eq!(
            pick_asset(&loose, "2.15.0"),
            Some("ABB-2.15.0-arm64.dmg".to_string())
        );
        // 没有 dmg → None
        let none = vec!["ABB-Setup-2.15.0.exe".to_string()];
        assert_eq!(pick_asset(&none, "2.15.0"), None);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn pick_asset_windows() {
        let names = vec![
            "ABB-2.15.0.dmg".to_string(),
            "ABB-Setup-2.15.0.exe".to_string(),
        ];
        assert_eq!(
            pick_asset(&names, "2.15.0"),
            Some("ABB-Setup-2.15.0.exe".to_string())
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn pick_asset_linux_none() {
        let names = vec!["ABB-2.15.0.dmg".to_string()];
        assert_eq!(pick_asset(&names, "2.15.0"), None);
    }
}
