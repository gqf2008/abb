//! 版本检测 + 自动升级安装（托盘菜单「检查更新」）。
//!
//! 链路：GitHub API 查 latest release → 与 CARGO_PKG_VERSION 比 semver →
//! 有新版本则按平台选资产（macOS=dmg / Windows=Setup exe / Linux 无预编译包）→
//! 下载到临时目录 → 平台安装：
//! - macOS：hdiutil 挂载 dmg → 旧 bundle 改名留备份 → ditto 新 bundle 原位 →
//!   分离式 sh 等本进程退出后 `open` 新实例（单实例锁随进程死亡释放，见 single_instance.rs）。
//! - Windows：直接启动 Inno Setup 安装包（PrivilegesRequired=lowest 免 UAC），
//!   **静默**参数装（用户点了升级就只有进度提示，不再走安装向导）+ 安装器 `[Run]` 段
//!   拉起新实例（见 `app-assets/ABB.iss` 的 `Check: WizardSilent` 那一条）；本进程随即
//!   退出让出文件锁。
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
                    log_update(&format!(
                        "[update] 拉 SHA256SUMS 失败（网络问题，可重试）：{e:#}"
                    ));
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
                    log_update(&format!("[update] 下载第 {attempt}/3 次失败：{e:#}"));
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
    verify_sha256_at(
        file,
        name_for_log,
        expected,
        Some(&crate::bridge_dir().join("logs")),
    )
}

/// [`verify_sha256`] 的目录可注入版；`logs_dir = None` = 只写 stdout、不落盘。
///
/// 单测走 `None`：隔离门禁里测试进程的 `bridge_dir` 是隔离 HOME，往那儿写文件会被判成
/// 「测试写入运行数据」（本批首跑就被隔离守卫抓到 `…/.agent-bridge/logs/update.log`），
/// 这正是 `LESSON_单测不得写用户真实运行数据须拆出注入缝.md` 说的缝。落盘那半由
/// `log_update_at` 自己的单测用临时目录钉住。
fn verify_sha256_at(
    file: &std::path::Path,
    name_for_log: &str,
    expected: Option<&str>,
    logs_dir: Option<&std::path::Path>,
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
        let line = format!("[update] 校验失败 {name_for_log}: 期望 {expected} 实得 {actual}");
        log_stdout(&line);
        if let Some(dir) = logs_dir {
            log_update_at(dir, &line);
        }
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
    log_update_event(&format!(
        "开始安装升级包 {}（当前版本 v{})",
        file.display(),
        CURRENT
    ));
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

/// 升级动作的**文件**留痕（`<bridge_dir>/logs/update.log`）。
///
/// 为什么不能只靠 `crate::log!`：它只写 stdout（`main.rs` 的 `write_log`），而升级是在
/// **GUI 进程**里发起的——Windows 下那是 `windows_subsystem = "windows"`（无控制台）、
/// 由安装器 `[Run]`/资源管理器拉起，macOS 下由 `open` 拉起时 0/1/2 全指 /dev/null，
/// 这条最需要留证据的信息会当场蒸发（同一结论的成文先例：`platform::log_autostart_event`，
/// 那里就是为自启自愈改成写文件的）。升级一旦静默失败，更新器又拿不到安装器退出码、
/// 本进程已退出，所以只能靠自己落盘。
///
/// 注意边界：**跑这次升级的是旧版二进制**，所以这行日志只从「装了本改动之后再升级」
/// 才存在；随后那次升级的失败原因同样要等下一次升级才有 update.log 可看。
fn log_update_event(msg: &str) {
    log_update_at(&crate::bridge_dir().join("logs"), msg);
}

/// [`log_update_event`] 的目录可注入版（单测用临时目录，不写真实 `~/.agent-bridge`）。
///
/// 「双写」里 stdout 那半在本机测试里无法断言（`crate::log!` 没有注入缝），能钉的是文件这半：
/// 因此单测断言的是「`logs/update.log` 里真的有这条」，另一半由 `log_update` 的源码守卫
/// （`updater.rs` 里除该 helper 外不许出现裸 `crate::log!`）兜底。
fn log_update_at(logs_dir: &std::path::Path, msg: &str) {
    crate::platform::append_event_log(logs_dir, "update.log", msg);
}

/// 升级链路的「两处都留痕」日志：stdout（有人在终端里跑时看得到）**加上** `logs/update.log`。
///
/// 为什么不要单用 `crate::log!`：升级发生在 GUI 进程里，而 GUI 的 stdout 在 Windows
/// （无控制台、由安装器 `[Run]`/资源管理器拉起）与 macOS（`open` 拉起时 0/1/2 指 /dev/null）
/// 上都会蒸发——「sha256 不符已拒绝安装」这种最该被看见的告警也会一起消失。
/// 复核 reviewer-40 的问题 3 就是这一条（B1 的同类病）。
fn log_update(msg: &str) {
    log_stdout(msg);
    log_update_event(msg);
}

/// 升级链路里**唯一**允许直写 stdout 的出口（源码守卫 `update_logs_never_use_bare_crate_log`
/// 按数量 + 位置钉住它：全文件的裸日志宏只许出现在这里）。
///
/// 「只想写 stdout」的调用点必须显式写 `log_stdout(...)`——评审一眼能看出这条日志在 GUI 下会
/// 蒸发；要留痕就用 `log_update(...)`。守卫拦的是「顺手又写一个裸宏」，不拦显式的 `log_stdout`
/// （后者是刻意的、review 看得见的选择）。
fn log_stdout(msg: &str) {
    crate::log!("{msg}");
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

/// 静默升级参数（点「升级」后**不再**让用户跑安装程序）。
///
/// - `/VERYSILENT`：无界面（连进度条都不显示）；
/// - `/SUPPRESSMSGBOXES`：任何对话框都不弹（失败也只留日志）；
/// - `/NORESTART`：不许安装器重启系统（app 的重启由安装脚本 `[Run]` 段完成）；
/// - `/CLOSEAPPLICATIONS`：若本进程还没退干净，直接关掉占用的实例，避免「文件占用」弹窗。
/// - `/LOG`：让安装器把过程写进**用户 TEMP 目录**下的日志。官方文档只说「按当前日期取唯一
///   文件名、不覆盖不追加」，具体形如 `Setup Log YYYY-MM-DD #N.txt` 是 Inno 的**惯例**
///   （不是文档承诺），所以排查时按 `%TEMP%\Setup Log *.txt` 这个宽口径找。**刻意用不带值的
///   裸 `/LOG`**：官方文档明写 `/LOG="<固定路径>"` 在「文件建不出来」时会让 Setup 直接
///   abort——而这条命令行要经 `cmd /c start` 转发（见 `windows_install`），带引号/空格的参数
///   在这条链上有被拆碎的风险（同 `LESSON_系列_Windows与安装包.md` 的 cmd/start 元字符坑）；
///   裸 `/LOG` 不引入任何引号/空格，参数形状与其余四个一致。静默安装失败时这就是唯一的归因面：
///   更新器拿不到安装器退出码（`cmd /c start` 派生后立即返回），本进程又已退出。
///
/// 抽成纯函数是为了让参数被单测钉住（漏掉 `/VERYSILENT` 就会退回「弹安装界面」的老行为，
/// 2026-09-28 owner 报的就是这个：装完了但没自动起来）。
#[cfg(any(target_os = "windows", test))]
fn windows_silent_args() -> Vec<&'static str> {
    vec![
        "/VERYSILENT",
        "/SUPPRESSMSGBOXES",
        "/NORESTART",
        "/CLOSEAPPLICATIONS",
        "/LOG",
    ]
}

/// Windows：启动 Inno 安装包（per-user 安装免 UAC；静默装完由安装脚本 `[Run]` 段拉起新实例）。
#[cfg(target_os = "windows")]
fn windows_install(setup: &Path) -> Result<()> {
    // start 把首个带引号参数当窗口标题，故先给空标题；CREATE_NO_WINDOW（统一走
    // crate::spawn）避免闪控制台。
    let args = windows_silent_args();
    let line = format!(
        "已静默启动安装包 {}（参数 {}；无窗口、不重启系统；安装器日志在 %TEMP%\\Setup Log *.txt）",
        setup.display(),
        args.join(" ")
    );
    // 先留痕再 spawn：安装器带 /CLOSEAPPLICATIONS，理论上本进程可能在 spawn 后被立刻关掉，
    // 那样这条记录就丢了（复核 reviewer-40 的问题 1）。顺序反过来零成本、无副作用。
    log_update_event(&line);
    crate::spawn::command("cmd")
        .arg("/c")
        .arg("start")
        .arg("")
        .arg(setup)
        .args(args)
        .spawn()
        .context("启动安装包失败")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 「双写」的文件半：给定临时目录时必须真的写出 `update.log`（追加、一行一记录）。
    #[test]
    fn log_update_at_writes_into_the_given_dir() {
        let dir = std::env::temp_dir().join(format!("abb-updater-log-{}", uuid::Uuid::new_v4()));
        log_update_at(&dir, "[update] 校验失败 x: 期望 a 实得 b");
        log_update_at(&dir, "[update] 已静默启动安装包 setup.exe");
        let text = std::fs::read_to_string(dir.join("update.log")).expect("update.log 应写出");
        assert_eq!(text.lines().count(), 2, "两次调用 = 两条记录：{text}");
        assert!(text.contains("校验失败"), "安全告警必须落盘：{text}");
        assert!(text.contains("已静默启动安装包"), "{text}");
        assert!(!text.contains("\n\n"), "记录之间不得夹空行：{text:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **源码护栏**：`updater.rs` 里除 `log_update` 自身外，不许再出现裸 `crate::log!`。
    ///
    /// 为什么：升级发生在 GUI 进程里，stdout 在 Windows（无控制台）与 macOS（`open` 拉起）
    /// 都会蒸发——单用 `crate::log!` 等于不留痕。复核 reviewer-40 的问题 3 指出「sha256 不符，
    /// 已拒绝安装」这条安全告警原本就是这么消失的；本守卫把「必须双写」变成可机器检查的不变量
    /// （与 `src/spawn.rs` 的 `creation_flags` 护栏同一模式）。
    ///
    /// 注释行（含 `///` 文档）不计——那里出现 `crate::log!` 只是说明文字。
    #[test]
    fn update_logs_never_use_bare_crate_log() {
        // 针在运行时拼出来：本测试自己的字符串/文档里也会出现这个宏名，写成字面量会把
        // 匹配数抬高（本批首跑就被自己的守卫抓红一次，与 F1 守卫的「按字节算距离」同源）。
        let needle = ["crate::", "log!"].concat();
        let src = include_str!("updater.rs");
        let hits: Vec<(usize, &str)> = src
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.trim_start().starts_with("//"))
            .filter(|(_, l)| l.contains(&needle))
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "只允许 log_update() 里那一处直写 stdout 的日志宏（双写的 stdout 半），实得 {hits:?}；\
             update 链路的新日志请走 log_update()/log_update_event()"
        );
        assert!(
            hits[0].1.trim_start().starts_with(&needle),
            "唯一那处应当就是 log_update 的 stdout 半，实得：{}",
            hits[0].1.trim()
        );
    }

    /// 静默升级参数必须齐全（尤其 `/VERYSILENT`：漏了就会弹安装界面、装完不自动起来）。
    /// 另钉两条：① `/LOG` 必须在（静默安装失败时它是唯一归因面）；
    /// ② **任何参数都不得含空白或引号**——这批参数要经 `cmd /c start` 转发，带空白/引号的
    ///    参数在那条链上可能被拆成多个参数（`/LOG=<带空格的路径>` 就会被拆碎）。
    #[test]
    fn windows_silent_args_are_locked() {
        let args = windows_silent_args();
        for need in [
            "/VERYSILENT",
            "/SUPPRESSMSGBOXES",
            "/NORESTART",
            "/CLOSEAPPLICATIONS",
            "/LOG",
        ] {
            assert!(args.contains(&need), "缺参数 {need}：{args:?}");
        }
        assert_eq!(args.len(), 5, "不要夹带其它参数：{args:?}");
        for a in &args {
            assert!(
                !a.chars().any(|c| c.is_whitespace() || c == '"'),
                "参数 {a:?} 含空白/引号：经 cmd /c start 转发时可能被拆碎"
            );
        }
    }

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
        // 不匹配 → 拒绝。走可注入版并传 None：这条会触发「校验失败」留痕，用公开入口
        // 会把 update.log 写进隔离 HOME（隔离守卫判红），落盘那半由 log_update_at 的
        // 单测（临时目录）覆盖。
        assert!(verify_sha256_at(&f, "x.dmg", Some(&"0".repeat(64)), None).is_err());
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
