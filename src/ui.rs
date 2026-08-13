//! GUI —— Slint 托盘控制器 + 多 bot 设置窗（兼作 service 的看门）。
//!
//! 设置窗为多 bot 主从结构：左列表（VecModel<BotRow>）+ 右编辑选中项。
//! 编辑在一份「工作副本」（Rc<RefCell<Vec<BotConfig>>>）上进行，保存时才写回 config.json。
//!
//! 看门：GUI 启动拉起 service 子进程；托盘 Timer 周期探测，崩溃自动重拉（见 install.rs）。
//! 打开日志/目录走 platform::open_path（跨平台）。

use crate::config::{BotConfig, Config, ProviderConfig};
use crate::feishu::FeishuClient;
use crate::install;
use crate::platform;
use anyhow::Result;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;
use tokio::sync::mpsc;

slint::include_modules!();

enum UiCmd {
    Start,
    Stop,
    Restart,
    Save(Config),
    /// 对某个 bot 拉 name/open_id（仅飞书支持；微信走扫码）
    FetchBotInfo {
        idx: i32,
        app_id: String,
        app_secret: String,
    },
    /// 微信扫码登录（对 bots[idx]）。后台拉二维码+长轮询，结果经 wx_rx 回主线程。
    WxLogin {
        idx: i32,
        bot_key: String,
    },
    /// 安装某个依赖（claude/codex/node/python3/lark-cli/dingtalk-cli）。结果经 dep_rx 回主线程。
    InstallDep(String),
    /// 测试某个供应商连通性（快照里 api_key 已就绪）。结果经 prov_rx 回主线程。
    TestProvider {
        idx: i32,
        snapshot: ProviderConfig,
    },
    OpenLogs,
    OpenFolder,
}

/// 微信扫码登录的阶段结果（后台 → 主线程）。
enum WxEvt {
    /// 二维码已就绪：携带落盘的 PNG 路径，主线程 load_from_path 渲染进 QrDialog。
    QrReady(std::path::PathBuf),
    /// 登录成功：写回该 bot 的 wx_* 字段。
    Confirmed(i32, crate::wechat::WeixinLogin),
    /// 失败/过期。
    Failed(String),
}

/// 通道类型的中文显示名（托盘/设置窗概览行；原始 kind 值对内，展示用中文）。
fn kind_label(kind: &str) -> String {
    match kind {
        "feishu" => "飞书".to_string(),
        "wechat" => "微信".to_string(),
        "dingtalk" => "钉钉".to_string(),
        other => other.to_string(),
    }
}

/// 把已查好的服务状态写进 Tray 属性。主线程调用（status 由调用方查，避免重复 fork ps）。
/// 托盘菜单里 bot 的显示名：空名回落通道类型；过长（如微信「微信 {完整 ilink user_id}」）
/// 截断加省略号，否则会把原生 NSMenu 撑得过宽。完整名仍在设置窗/配置里。
fn display_name(name: &str, kind: &str) -> String {
    const MAX: usize = 16;
    let base = if name.is_empty() { kind_label(kind) } else { name.to_string() };
    let chars: Vec<char> = base.chars().collect();
    if chars.len() <= MAX {
        base
    } else {
        chars.iter().take(MAX - 1).collect::<String>() + "…"
    }
}

fn push_status(tray: &Tray, st: &install::ServiceStatus) {
    tray.set_service_running(st.running);
    // 各 bot 运行态（来自 service 心跳）
    let bots = crate::botstatus::snapshot();
    // 逐 bot 状态（子菜单行）：状态小圆点图标（与托盘图标同款配色）+ 通道类型
    let rows: Vec<BotMenuRow> = bots
        .iter()
        .map(|b| {
            let icon = match b.conn.as_str() {
                "在线" => tray.get_icon_online(),
                "重连中" | "连接中" => tray.get_icon_connecting(),
                "已停用" => tray.get_icon_disabled(), // 用户主动停用（非故障，灰）
                _ => tray.get_icon_offline(), // 会话过期 / 离线 / 其它
            };
            let label = display_name(&b.name, &b.kind);
            BotMenuRow {
                title: format!("{label} · {} · {}", b.conn, kind_label(&b.kind)).into(),
                icon,
            }
        })
        .collect();
    tray.set_bots_menu(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
    tray.set_configured(configured_cached());
    tray.set_autostart(platform::autostart_enabled());

    // 托盘图标整体状态：所有启用 bot 在线=绿；有在连/重连=黄；有离线/会话过期=红；
    // 未跑服务、无 bot 或全部停用=原灰（不显红吓人）。
    let active: Vec<&crate::botstatus::BotStatus> = bots
        .iter()
        .filter(|b| b.conn != "已停用")
        .collect();
    let status = if !st.running || active.is_empty() {
        "none"
    } else if active.iter().any(|b| b.conn == "连接中" || b.conn == "重连中") {
        "connecting"
    } else if active.iter().all(|b| b.conn == "在线") {
        "online"
    } else {
        "offline"
    };
    tray.set_tray_status(status.into());
}

/// 把 service 状态同步到设置窗：动态标题（带 bot 数）+ 首页 hero/状态卡的结构化状态 + 底部运行概览行。
/// 设置窗隐藏时也更新（廉价），下次打开即是最新。
fn push_settings_status(settings: &SettingsWindow, st: &install::ServiceStatus) {
    let bots = crate::botstatus::snapshot();
    let online = bots.iter().filter(|b| b.conn == "在线").count();
    let title = if bots.is_empty() {
        "ABB 设置".to_string()
    } else {
        format!("ABB 设置 — {} 个 bot · {} 在线", bots.len(), online)
    };
    settings.set_window_title(title.into());
    // hero 大按钮 / 状态卡用的结构化字段（running-line 保留给底部状态行）
    settings.set_service_running(st.running);
    settings.set_autostart(platform::autostart_enabled());
    settings.set_bot_count(bots.len() as i32);
    settings.set_online_count(online as i32);
    let line = if !st.running {
        "● 服务已停止 — 点首页「启动服务」或托盘菜单「启动」".to_string()
    } else if bots.is_empty() {
        "● 服务运行中，暂无 bot 上报（稍候…）".to_string()
    } else {
        let items: Vec<String> = bots
            .iter()
            .map(|b| {
                // 微信名是完整 ilink id，过长会把这行撑满整宽 → 与托盘菜单一致截断
                let label = display_name(&b.name, &b.kind);
                format!("{label}·{}", b.conn)
            })
            .collect();
        format!("● {} 个 bot 在线：{}", bots.len(), items.join("  "))
    };
    settings.set_running_line(line.into());
}

/// 提升进程为 regular（显示 dock 图标 + 激活抢前台），用于打开设置/扫码窗。
fn bring_app_to_front() {
    platform::set_dock_visible(true);
}

/// 从回调里首次显示一个「创建后一直隐藏」的窗口。顺序很关键（macOS 实测坑）：
/// ① **必须 `request_redraw`**：Slint winit 后端在 `set_visible` 里对「首次 show」会预渲染首帧
///    （`winitwindowadapter.rs` ~1132 行的 `self.draw()`），但 macOS Metal 下该预渲染发生在
///    窗口 **map 之前**、surface 尚未就绪 → 首帧画空。而 macOS 又不像 iOS（同文件 ~1154 行的
///    guard）那样对「晚 show 的窗口」自动补一次 redraw，于是**内容区永久透明、只剩系统标题栏**。
///    显式 `request_redraw` 等于把 iOS 那条 guard 移植到 macOS，让窗口 map 后补画一帧。
/// ② **激活要在 show 之前**：`bring_app_to_front` 把策略从 accessory 切 Regular 会**重排窗口**。
///    若窗口已可见（先 show 后激活），重排会瞬间清掉刚画好的 Metal 内容、再由 redraw 补回 →
///    肉眼可见「先出标题栏、后出内容」的闪烁。先激活（窗口此时还藏着，重排不闪）再 show，
///    就不会扰动一个已可见的窗口。
/// 综上顺序：`bring_app_to_front` → `show` → `request_redraw`。`QrDialog`/`SettingsWindow` 都走这个。
fn show_window_and_focus<W: slint::ComponentHandle>(w: &W) {
    bring_app_to_front();
    let _ = w.show();
    w.window().request_redraw();
}

/// 把设置窗四份工作副本汇总成待写盘的 Config（「保存」与「草稿自动保存」共用同一份逻辑，
/// 保证两种路径行为一致：bots 保留运行期字段、供应商密钥留空沿用旧值、默认供应商防悬空）。
/// 返回 (Config, 丢弃的未命名供应商数)。
fn snapshot_config(
    work: &RefCell<Vec<BotConfig>>,
    providers_work: &RefCell<Vec<ProviderConfig>>,
    default_provider_work: &RefCell<String>,
    cross_delivery_work: &RefCell<bool>,
) -> (Config, usize) {
    let mut c = Config::load().unwrap_or_default();
    // 用工作副本整体替换 bots（保留每个 bot 运行期的 primary_chat_id）
    let old = std::mem::take(&mut c.bots);
    let mut newb = work.borrow().clone();
    for nb in newb.iter_mut() {
        if let Some(ob) = old.iter().find(|o| o.key() == nb.key()) {
            nb.primary_chat_id = ob.primary_chat_id.clone();
            if nb.bot_name.is_empty() {
                nb.bot_name = ob.bot_name.clone();
            }
            if nb.bot_open_id.is_empty() {
                nb.bot_open_id = ob.bot_open_id.clone();
            }
        }
    }
    c.bots = newb;

    // 跨会话投递总开关：全局生效（所有 bot 共享）
    c.cross_delivery_enabled = *cross_delivery_work.borrow();

    // 供应商：用工作副本替换，但 api_key 留空=保留旧值（密码框不回显，编辑其它字段不该清密钥）。
    // 丢弃空 name 行（无效），并计数。
    let old_providers = std::mem::take(&mut c.providers);
    let mut dropped = 0;
    let mut newp: Vec<ProviderConfig> = Vec::new();
    for mut p in providers_work.borrow().clone().into_iter() {
        if p.name.trim().is_empty() {
            dropped += 1;
            continue;
        }
        if p.api_key.is_empty() {
            // 留空 → 沿用同名旧供应商的密钥；没有同名旧供应商则保持空
            if let Some(op) = old_providers.iter().find(|o| o.name == p.name) {
                p.api_key = op.api_key.clone();
            }
        }
        newp.push(p);
    }
    c.providers = newp;
    let d = default_provider_work.borrow().clone();
    // 默认供应商名若已不在列表里（被删/改名）→ 清空，避免悬空引用
    c.default_provider = if !d.is_empty() && c.providers.iter().any(|p| p.name == d) {
        d
    } else {
        String::new()
    };
    (c, dropped)
}

/// is_configured 的廉价缓存：按 config.json 的 mtime 失效，避免每 2s 全量读+解析。
fn configured_cached() -> bool {
    use std::sync::Mutex;
    use std::time::SystemTime;
    static CACHE: Mutex<Option<(Option<SystemTime>, bool)>> = Mutex::new(None);
    let mtime = std::fs::metadata(Config::path())
        .and_then(|m| m.modified())
        .ok();
    let mut c = CACHE.lock().unwrap();
    if let Some((cached_mtime, val)) = *c {
        if cached_mtime == mtime {
            return val;
        }
    }
    let val = Config::load().map(|c| c.is_configured()).unwrap_or(false);
    *c = Some((mtime, val));
    val
}

/// 把工作副本 Vec<BotConfig> 同步进 Slint 的 VecModel<BotRow>。
fn sync_model(model: &slint::VecModel<BotRow>, bots: &[BotConfig]) {
    model.set_vec(bots.iter().map(bot_to_row).collect::<Vec<_>>());
}

fn bot_to_row(b: &BotConfig) -> BotRow {
    let granted = slint::VecModel::from(
        b.granted_infos
            .iter()
            .map(|i| OwnerRow {
                name: (if i.name.is_empty() {
                    i.open_id.clone()
                } else {
                    i.name.clone()
                })
                .into(),
                open_id: i.open_id.clone().into(),
            })
            .collect::<Vec<_>>(),
    );
    let ding_granted = slint::VecModel::from(
        b.ding_granted_infos
            .iter()
            .map(|i| OwnerRow {
                name: (if i.name.is_empty() {
                    i.open_id.clone()
                } else {
                    i.name.clone()
                })
                .into(),
                open_id: i.open_id.clone().into(),
            })
            .collect::<Vec<_>>(),
    );
    BotRow {
        name: b.name.clone().into(),
        kind: b.kind.clone().into(),
        enabled: b.enabled,
        // UI 上后端必须有个确定值显示：per-bot 为空（跟随全局）时显示 claude 兜底
        backend: (if b.backend.is_empty() {
            "claude"
        } else {
            b.backend.as_str()
        })
        .into(),
        owner_open_id: b.owner_open_id.clone().into(),
        owners: slint::ModelRc::from(Rc::new(slint::VecModel::from(Vec::<OwnerRow>::new()))),
        granted: slint::ModelRc::from(Rc::new(granted)),
        open_access: b.open_access,
        app_id: b.app_id.clone().into(),
        app_secret: b.app_secret.clone().into(),
        bot_name: b.bot_name.clone().into(),
        bot_open_id: b.bot_open_id.clone().into(),
        // per-bot 供应商名（""=跟随全局默认），直接显示原值（下拉第一项是 ""）
        provider: b.provider.clone().into(),
        ding_user_id: b.ding_user_id.clone().into(),
        ding_owner_ids: b.ding_owner_ids.clone().into(),
        ding_granted: slint::ModelRc::from(Rc::new(ding_granted)),
        ding_open_access: b.ding_open_access,
        ding_robot_code: b.ding_robot_code.clone().into(),
    }
}

/// 供应商工作副本 → Slint ProviderRow。is-default 由名字与 default_provider_work 比对得出。
/// api_key 不回填（密码框留空，保存时留空=保留旧值）。
fn provider_to_row(p: &ProviderConfig, default_name: &str) -> ProviderRow {
    ProviderRow {
        name: p.name.clone().into(),
        kind: p.kind.clone().into(),
        base_url: p.base_url.clone().into(),
        api_key: "".into(), // 安全：密钥不回显
        model: p.model.clone().into(),
        is_default: !p.name.is_empty() && p.name == default_name,
    }
}

fn sync_providers_model(
    model: &slint::VecModel<ProviderRow>,
    providers: &[ProviderConfig],
    default_name: &str,
) {
    model.set_vec(
        providers
            .iter()
            .map(|p| provider_to_row(p, default_name))
            .collect::<Vec<_>>(),
    );
}

/// config.json 签名（mtime, size）：检测外部（service 消费授权码等）改盘。
fn config_sig() -> Option<(std::time::SystemTime, u64)> {
    std::fs::metadata(crate::config::Config::path())
        .ok()
        .and_then(|m| Some((m.modified().ok()?, m.len())))
}

/// 检测 config.json 被外部改（service 消费授权码改 owner 白名单等）且当前无未保存编辑 →
/// 重载 bots 区（保留 selected，不动状态行），让 Owner 白名单 / 授权码展示跟随实际授权状态，
/// 也避免用户之后点「保存」用旧快照把 service 刚写入的授权覆盖掉（lost update）。
/// dirty=true（正在编辑）→ 跳过重载，只推进签名（保存路径会 load_into 拉最新）。
fn reload_bots_if_external_change(
    w: &SettingsWindow,
    work: &RefCell<Vec<BotConfig>>,
    model: &slint::VecModel<BotRow>,
    dirty: &Cell<bool>,
    last_sig: &Cell<Option<(std::time::SystemTime, u64)>>,
) {
    let cur = config_sig();
    if cur == last_sig.get() {
        return;
    }
    last_sig.set(cur);
    if dirty.get() {
        return;
    }
    let cfg = match crate::config::Config::load() {
        Ok(c) => c,
        Err(_) => return,
    };
    let sel = w.get_selected();
    *work.borrow_mut() = cfg.bots.clone();
    sync_model(model, &cfg.bots);
    w.set_selected(sel);
    refresh_owner_code_info(w, work);
}

/// 按当前选中 bot 重算互斥 CheckBox 的勾选态（后端 + 对话权限）。
/// 切 bot / 装载设置窗时调用。整体替换 option model → for 循环重建 CheckBox 实例，
/// 绕开 slint「用户交互移除 checked 绑定、状态残留到其它 bot」的坑。
fn refresh_exclusive_checks(w: &SettingsWindow, work: &RefCell<Vec<BotConfig>>) {
    let bot = work.borrow().get(w.get_selected() as usize).cloned();
    let be = bot.as_ref().map(|b| b.backend.clone()).unwrap_or_default();
    let mk_opts = |opts: &[(&str, &str)], sel: &str| -> Vec<OptionRow> {
        opts.iter()
            .map(|(name, val)| OptionRow {
                name: (*name).into(),
                checked: *val == sel,
            })
            .collect()
    };
    // 空 = 跟随全局 → claude 选中
    let be_sel = if be.is_empty() { "claude" } else { be.as_str() };
    w.set_backend_options(slint::ModelRc::from(Rc::new(slint::VecModel::from(mk_opts(
        &[("claude", "claude"), ("codex", "codex"), ("pi", "pi")],
        be_sel,
    )))));
    let is_ding = bot.as_ref().map(|b| b.is_dingtalk()).unwrap_or(false);
    let open = bot
        .map(|b| {
            if is_ding {
                b.ding_open_access
            } else {
                b.open_access
            }
        })
        .unwrap_or(false);
    let access_sel = if open { "open" } else { "private" };
    let access_model = slint::ModelRc::from(Rc::new(slint::VecModel::from(mk_opts(
        &[
            ("仅授权用户（owner+授权者）", "private"),
            ("任何人都可以对话", "open"),
        ],
        access_sel,
    ))));
    w.set_access_options(access_model.clone());
    w.set_ding_access_options(access_model);
}

/// 刷新当前选中 bot 的授权码展示（切换 bot / 生成后调用）：管理员码与普通授权码分两行。
fn refresh_owner_code_info(w: &SettingsWindow, work: &RefCell<Vec<BotConfig>>) {
    let idx = w.get_selected() as usize;
    let codes = work
        .borrow()
        .get(idx)
        .map(|b| crate::config::Config::pending_owner_codes(&b.key()))
        .unwrap_or_default();
    let now = crate::chrono_lite::unix_secs();
    let mut owner_line = String::new();
    let mut grant_line = String::new();
    for (role, code, expires) in codes {
        let mins = expires.saturating_sub(now) / 60;
        let line = format!("🔑 授权码：{code}（剩 {mins} 分钟）");
        if role == "owner" {
            owner_line = format!("👑 管理员{line}");
        } else {
            grant_line = line;
        }
    }
    w.set_owner_code_info(owner_line.into());
    w.set_grant_code_info(grant_line.into());
}

/// 跑一次依赖检测并把全部 7 项状态回填到设置窗（claude/codex/pi/node/python3/lark-cli/dingtalk-cli）。
fn push_deps_to_window(w: &SettingsWindow) {
    let all = crate::deps::detect_all();
    let get = |id: &str| {
        all.iter()
            .find(|d| d.id == id)
            .map(|d| d.found)
            .unwrap_or(false)
    };
    // #8 M0：claude/codex/pi 任一未装 → 顶部横幅（首次启动也据此自动弹设置窗引导安装）
    w.set_missing_agent(!get("claude") || !get("codex") || !get("pi"));
    w.set_claude_installed(get("claude"));
    w.set_codex_installed(get("codex"));
    w.set_pi_installed(get("pi"));
    w.set_node_installed(get("node"));
    w.set_python_installed(get("python3"));
    w.set_lark_installed(get("lark-cli"));
    w.set_dingtalk_installed(get("dingtalk-cli"));
}

/// 把系统权限状态回填到设置窗（0=未授权 1=被拒绝 2=已授权）。
/// macOS 六项 TCC 权限；Windows 一项管理员身份；Linux 不显示。
fn push_perms_to_window(w: &SettingsWindow) {
    // 平台标识（UI 据此决定显示哪套权限）
    #[cfg(target_os = "macos")]
    w.set_platform("macos".into());
    #[cfg(target_os = "windows")]
    w.set_platform("windows".into());
    #[cfg(all(unix, not(target_os = "macos")))]
    w.set_platform("linux".into());

    let perms = crate::deps::detect_permissions();
    w.set_perms_supported(!perms.is_empty());
    let code = |id: &str| -> i32 {
        use crate::deps::PermState;
        perms
            .iter()
            .find(|p| p.id == id)
            .map(|p| match p.state {
                PermState::Granted => 2,
                PermState::Denied => 1,
                PermState::NotDetermined => 0,
            })
            .unwrap_or(0)
    };
    w.set_perm_full_disk(code("full-disk"));
    w.set_perm_accessibility(code("accessibility"));
    w.set_perm_screen(code("screen"));
    w.set_perm_automation(code("automation"));
    w.set_perm_camera(code("camera"));
    w.set_perm_microphone(code("microphone"));
    w.set_perm_admin(code("admin")); // Windows
}

/// 配置 winit 后端（须在创建任何组件前调用）。
fn configure_backend() -> Result<()> {
    // macOS：标题栏透明 + 标题文字隐藏 + 内容铺满整窗（对齐 ../aerodesk 官方做法）：
    // 保留原生红绿灯与原生拖动。内容延伸进标题栏后，所有 Window 的根必须用铺满的
    // 不透明 Rectangle 背景（app.slint 已保证），且根 VerticalBox/HorizontalBox 的
    // 默认 layout-padding 要归零，否则顶部会透出根背景形成一条「标题栏白条」。
    #[cfg(target_os = "macos")]
    {
        use i_slint_backend_winit::Backend;
        use winit::platform::macos::WindowAttributesExtMacOS;
        let backend = Backend::builder()
            .with_window_attributes_hook(|attrs| {
                attrs
                    .with_titlebar_transparent(true)
                    .with_title_hidden(true)
                    .with_fullsize_content_view(true)
            })
            .build()
            .expect("slint winit backend");
        slint::platform::set_platform(Box::new(backend)).expect("set slint platform");
    }
    #[cfg(not(target_os = "macos"))]
    slint::BackendSelector::new().select()?;
    Ok(())
}

pub fn run_gui() -> Result<()> {
    configure_backend()?;

    // 旧「单 bot 平铺」数据 → workspaces/<key>/（幂等）
    if let Ok(c) = Config::load() {
        if let Some(b) = c.bots.first() {
            platform::migrate_legacy_state(&b.key());
        }
    }

    let tray = Tray::new()?;
    let settings = SettingsWindow::new()?;
    let qr_dialog = QrDialog::new()?;
    let unsaved_dialog = UnsavedDialog::new()?;
    // 设置窗编辑脏标记：任何字段/开关被改过 → true；保存/重新加载 → false。
    // 有未保存修改时，关闭/取消要先弹确认，避免静默丢编辑（红点/按钮都走这条保护）。
    let dirty = Rc::new(Cell::new(false));
    // 启动路径（自动引导/半配置）是否已显式 show 设置窗：是则预热的「队列里 hide」要跳过，
    // 否则窗口会在事件循环启动瞬间被隐藏（启动即显示变成黑窗/无窗）。
    // 用 Arc<AtomicBool>：预热的 invoke_from_event_loop 闭包要求 Send，Rc 进不去。
    let startup_shown = Arc::new(AtomicBool::new(false));

    use slint::ComponentHandle;

    // 点窗口红点（traffic-light）关闭 → 有未保存修改先弹确认，否则降回 accessory / 直接隐藏。
    // （取消/保存按钮走各自回调 hide；红点这条系统路径单独拦：CloseRequested 返回
    //  PreventDefault 可阻止 Slint 默认 hide，跨平台生效）
    {
        use slint::winit_030::{winit::event::WindowEvent, EventResult, WinitWindowAccessor};
        let dirty = dirty.clone();
        let dlg = unsaved_dialog.as_weak();
        settings.window().on_winit_window_event(move |_w, ev| {
            if matches!(ev, WindowEvent::CloseRequested) {
                if dirty.get() {
                    if let Some(d) = dlg.upgrade() {
                        show_window_and_focus(&d);
                    }
                    return EventResult::PreventDefault; // 窗口不关，先让用户选保存/不保存
                }
                #[cfg(target_os = "macos")]
                platform::hide_dock();
            }
            EventResult::Propagate // 让 Slint 照常把窗口 hide
        });
        qr_dialog.window().on_winit_window_event(|_w, ev| {
            if matches!(ev, WindowEvent::CloseRequested) {
                #[cfg(target_os = "macos")]
                platform::hide_dock();
            }
            EventResult::Propagate
        });
    }

    // 预热设置窗的 Metal layer，消除「每次启动后首次打开闪一帧」。
    // 缘由：Slint 在 macOS Metal 下，窗口**首次** show 的预渲染帧是空的（surface 在窗口 map
    // 前没就绪），内容要到下一个事件循环 tick 的 redraw 才补上 → 首次打开「先标题栏后内容」闪一下。
    // 但 Metal layer 会**保留上一帧**（已实测：第二次打开不闪）。故启动时把窗口挪到屏外 show +
    // redraw 一次（屏外不可见、无闪烁、无激活），让 layer 预先填上内容；下一帧再 hide 并复位位置。
    // 此后用户首次「打开」走的是「非首次 show」路径，layer 已有预热内容 → 不闪。
    // （二维码弹窗同理，但它只在微信登录时偶发，且每次内容不同，不值得预热，留作可接受的偶发闪烁。）
    #[cfg(target_os = "macos")]
    {
        let saved = settings.window().position();
        settings
            .window()
            .set_position(slint::PhysicalPosition::new(-16_000, -16_000));
        let _ = settings.show();
        settings.window().request_redraw();
        let sw = settings.as_weak();
        let startup_shown_pw = startup_shown.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = sw.upgrade() {
                // 启动路径已经 show 过（自动引导/半配置）→ 只复位位置，不 hide
                if !startup_shown_pw.load(Ordering::Relaxed) {
                    let _ = w.hide();
                }
                w.window().set_position(saved);
            }
        });
    }

    // 取消按钮：关掉二维码弹窗（登录轮询会自然超时结束）
    {
        let qw = qr_dialog.as_weak();
        qr_dialog.on_close_clicked(move || {
            if let Some(d) = qw.upgrade() {
                let _ = d.hide();
                platform::hide_dock();
            }
        });
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<UiCmd>();
    let (bot_tx, bot_rx) =
        std_mpsc::channel::<(i32, std::result::Result<(String, String), String>)>();
    let (wx_tx, wx_rx) = std_mpsc::channel::<WxEvt>();
    // 依赖安装 / 供应商测试结果（后台 → 主线程）
    let (dep_tx, dep_rx) = std_mpsc::channel::<(String, std::result::Result<String, String>)>();
    let (prov_tx, prov_rx) = std_mpsc::channel::<(i32, std::result::Result<String, String>)>();

    // ── 设置窗工作副本 + 列表 model ──
    let bots_model: Rc<slint::VecModel<BotRow>> = Rc::new(slint::VecModel::default());
    let work: Rc<RefCell<Vec<BotConfig>>> = Rc::new(RefCell::new(Vec::new()));
    settings.set_bots(slint::ModelRc::from(bots_model.clone()));

    // config.json 外部变化监控：service 侧消费授权码会改 owner 白名单/pending_codes（GUI 是独立
    // 进程，读不到那份快照）。设置窗开着且无未保存编辑时，周期检测签名变化 → 热刷新 bots 区
    // （Owner 白名单、授权码展示跟着实际授权状态走，避免「bot 已回复授权成功、GUI 还显示旧的」
    // 以及后续保存把 service 写入覆盖掉）。Timer 在 run() 作用域持有，随事件循环存活。
    let watch_sig = Rc::new(Cell::new(config_sig()));
    let watch_w = settings.as_weak();
    let watch_work = work.clone();
    let watch_model = bots_model.clone();
    let watch_dirty = dirty.clone();
    let config_watch = slint::Timer::default();
    config_watch.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(3000),
        move || {
            if let Some(w) = watch_w.upgrade() {
                reload_bots_if_external_change(
                    &w,
                    &watch_work,
                    &watch_model,
                    &watch_dirty,
                    &watch_sig,
                );
            }
        },
    );
    // 供应商工作副本 + model + 全局默认名工作副本
    let providers_model: Rc<slint::VecModel<ProviderRow>> = Rc::new(slint::VecModel::default());
    let providers_work: Rc<RefCell<Vec<ProviderConfig>>> = Rc::new(RefCell::new(Vec::new()));
    let default_provider_work: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    // 跨会话投递总开关工作副本（#21）
    let cross_delivery_work: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
    settings.set_providers(slint::ModelRc::from(providers_model.clone()));

    /// 供 bot Tab 的供应商下拉：第一项 ""（=跟随全局默认），后接各供应商 name。
    fn build_provider_names(providers: &[ProviderConfig]) -> Vec<slint::SharedString> {
        let mut v = vec![slint::SharedString::from("")];
        for p in providers {
            if !p.name.is_empty() {
                v.push(p.name.clone().into());
            }
        }
        v
    }

    // 打开设置窗时装载 config → 各工作副本 + model
    // 设置窗装载涉及 Slint model + 四份工作副本，参数多；聚集成本高于收益，允许 lint。
    #[allow(clippy::too_many_arguments)]
    fn load_into(
        w: &SettingsWindow,
        c: &Config,
        work: &RefCell<Vec<BotConfig>>,
        model: &slint::VecModel<BotRow>,
        providers_work: &RefCell<Vec<ProviderConfig>>,
        providers_model: &slint::VecModel<ProviderRow>,
        default_provider_work: &RefCell<String>,
        cross_delivery_work: &RefCell<bool>,
    ) {
        *work.borrow_mut() = c.bots.clone();
        sync_model(model, &c.bots);
        *providers_work.borrow_mut() = c.providers.clone();
        *default_provider_work.borrow_mut() = c.default_provider.clone();
        *cross_delivery_work.borrow_mut() = c.cross_delivery_enabled;
        w.set_cross_delivery_enabled(c.cross_delivery_enabled);
        sync_providers_model(providers_model, &c.providers, &c.default_provider);
        w.set_provider_names(slint::ModelRc::from(Rc::new(slint::VecModel::from(
            build_provider_names(&c.providers),
        ))));
        // 后端、Owner、供应商 都是 per-bot（bots[i].backend / .owner_open_id / .provider）
        w.set_selected(if c.bots.is_empty() { -1 } else { 0 });
        w.set_provider_selected(if c.providers.is_empty() { -1 } else { 0 });
        refresh_owner_code_info(w, work);
        refresh_exclusive_checks(w, work);
        w.set_status_line("".into());
        // 依赖检测：claude/codex/node/python3/lark-cli 是否在本机可执行路径上。
        push_deps_to_window(w);
        // 系统权限检测（macOS）：完全磁盘/辅助功能/屏幕录制/自动化。
        push_perms_to_window(w);
    }

    /// 装载设置窗：发现比正式配置新的草稿 → 静默恢复（返回 true，标记 dirty 并给一行提示）；
    /// 否则按正式配置装载（返回 false）。「静默恢复」= 不弹选择框，直接把草稿当工作底稿。
    #[allow(clippy::too_many_arguments)]
    fn load_with_draft(
        w: &SettingsWindow,
        dirty: &Cell<bool>,
        work: &RefCell<Vec<BotConfig>>,
        model: &slint::VecModel<BotRow>,
        providers_work: &RefCell<Vec<ProviderConfig>>,
        providers_model: &slint::VecModel<ProviderRow>,
        default_provider_work: &RefCell<String>,
        cross_delivery_work: &RefCell<bool>,
    ) -> bool {
        if Config::draft_is_newer() {
            let draft = Config::load_draft().unwrap_or_default();
            load_into(w, &draft, work, model, providers_work, providers_model, default_provider_work, cross_delivery_work);
            dirty.set(true);
            w.set_status_is_error(false);
            w.set_status_line("已恢复上次未保存的草稿（编辑后点「保存」写入配置）".into());
            true
        } else {
            let cfg = Config::load().unwrap_or_default();
            load_into(w, &cfg, work, model, providers_work, providers_model, default_provider_work, cross_delivery_work);
            dirty.set(false);
            false
        }
    }

    // ── 后台 tokio 线程：处理慢操作（HTTP/起停），结果回主线程 ──
    let tray_weak_bg = tray.as_weak();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(async move {
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    UiCmd::Start => {
                        let _ = install::svc_start();
                        refresh(&tray_weak_bg);
                    }
                    UiCmd::Stop => {
                        install::svc_stop();
                        refresh(&tray_weak_bg);
                    }
                    UiCmd::Restart => {
                        install::svc_restart();
                        refresh(&tray_weak_bg);
                    }
                    UiCmd::OpenLogs => platform::open_path(&crate::bridge_dir().join("logs")),
                    UiCmd::OpenFolder => platform::open_path(&crate::bridge_dir()),
                    UiCmd::Save(cfg) => {
                        let res = cfg.save();
                        if res.is_ok() {
                            Config::remove_draft(); // 正式配置已落盘，草稿作废
                        }
                        if res.is_ok() && install::status().running {
                            install::svc_restart();
                        }
                        // 接入飞书 bot → 后台自动装 lark-cli + lark 技能（幂等/best-effort）。
                        // GUI 路径免等 service 重启；装不上只 log 警告。只对飞书 bot 触发。
                        if res.is_ok() && cfg.bots.iter().any(|b| b.enabled && b.kind == "feishu") {
                            tokio::spawn(async { crate::larkskills::ensure_lark_setup().await });
                        }
                        let tw = tray_weak_bg.clone();
                        let ok = res.is_ok();
                        let st = install::status();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(t) = tw.upgrade() {
                                push_status(&t, &st);
                            }
                        });
                        if !ok {
                            crate::log!("[gui] 配置保存失败");
                        }
                    }
                    UiCmd::FetchBotInfo {
                        idx,
                        app_id,
                        app_secret,
                    } => {
                        let fs = FeishuClient::new(&app_id, &app_secret);
                        let r = fs.bot_info().await.map_err(|e| format!("{e:#}"));
                        let _ = bot_tx.send((idx, r));
                    }
                    UiCmd::WxLogin { idx, bot_key } => {
                        run_wx_login(idx, &bot_key, wx_tx.clone()).await;
                    }
                    UiCmd::InstallDep(dep_id) => {
                        let r = crate::deps::run_install(&dep_id).await;
                        let _ = dep_tx.send((dep_id, r));
                    }
                    UiCmd::TestProvider { idx, snapshot } => {
                        let r = test_provider(&snapshot).await;
                        let _ = prov_tx.send((idx, r));
                    }
                }
            }
        });
    });

    fn refresh(tw: &slint::Weak<Tray>) {
        let tw = tw.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(t) = tw.upgrade() {
                let st = install::status();
                push_status(&t, &st);
            }
        });
    }

    // ── 托盘回调 ──
    {
        let txc = || tx.clone();
        tray.on_start_service({
            let tx = txc();
            move || {
                let _ = tx.send(UiCmd::Start);
            }
        });
        tray.on_stop_service({
            let tx = txc();
            move || {
                let _ = tx.send(UiCmd::Stop);
            }
        });
        tray.on_restart_service({
            let tx = txc();
            move || {
                let _ = tx.send(UiCmd::Restart);
            }
        });
        tray.on_open_logs({
            let tx = txc();
            move || {
                let _ = tx.send(UiCmd::OpenLogs);
            }
        });
        tray.on_open_folder({
            let tx = txc();
            move || {
                let _ = tx.send(UiCmd::OpenFolder);
            }
        });
        tray.on_toggle_autostart(move |on| {
            let _ = platform::set_autostart(on);
        });
    }
    // #8 M0 自动引导：claude/codex/pi 未安装 → 启动即弹出设置窗。复用托盘打开同一条路径
    // （load_into → 依赖横幅 + 状态行），保证窗口内容完整（不只是空窗）。已装好 agent 的
    // 开发者/朋友零打扰（条件不成立）；对新装用户这是「打开就能被引导」的关键一步。
    {
        let deps = crate::deps::detect_all();
        let missing = |id: &str| {
            deps.iter()
                .find(|d| d.id == id)
                .map(|d| !d.found)
                .unwrap_or(true)
        };
        if missing("claude") || missing("codex") || missing("pi") {
            let work = work.clone();
            let model = bots_model.clone();
            let pwork = providers_work.clone();
            let pmodel = providers_model.clone();
            let dwork = default_provider_work.clone();
            load_with_draft(
                &settings,
                &dirty,
                &work,
                &model,
                &pwork,
                &pmodel,
                &dwork,
                &cross_delivery_work,
            );
            push_settings_status(&settings, &install::status());
            settings.set_status_line(
                "⚠️ 未检测到 Claude Code / Codex CLI：请到「环境配置」页安装依赖，否则机器人无法处理消息。"
                    .into(),
            );
            settings.set_status_is_error(true);
            startup_shown.store(true, Ordering::Relaxed);
            show_window_and_focus(&settings);
        }
    }
    {
        let sw = settings.as_weak();
        let work = work.clone();
        let model = bots_model.clone();
        let pwork = providers_work.clone();
        let pmodel = providers_model.clone();
        let dwork = default_provider_work.clone();
        let cdwork = cross_delivery_work.clone();
        let dirty_open = dirty.clone();
        tray.on_open_settings(move || {
            if let Some(w) = sw.upgrade() {
                // 草稿比正式配置新（上次编辑没保存就退出/崩溃）→ 静默恢复为工作底稿
                load_with_draft(
                    &w,
                    &dirty_open,
                    &work,
                    &model,
                    &pwork,
                    &pmodel,
                    &dwork,
                    &cdwork,
                );
                push_settings_status(&w, &install::status());
                show_window_and_focus(&w); // 先 show 再激活再重绘（见该函数注释：避免内容区透明）
            }
        });
    }
    tray.on_quit_app(|| {
        install::svc_stop();
        let _ = slint::quit_event_loop();
    });

    // ── 设置窗首页 hero：启动/停止/重启服务 + 打开日志，复用托盘同一套 UiCmd（后台线程串行执行）──
    {
        let txc = || tx.clone();
        settings.on_start_service({
            let tx = txc();
            move || {
                let _ = tx.send(UiCmd::Start);
            }
        });
        settings.on_stop_service({
            let tx = txc();
            move || {
                let _ = tx.send(UiCmd::Stop);
            }
        });
        settings.on_restart_service({
            let tx = txc();
            move || {
                let _ = tx.send(UiCmd::Restart);
            }
        });
        settings.on_open_logs({
            let tx = txc();
            move || {
                let _ = tx.send(UiCmd::OpenLogs);
            }
        });
    }

    // ── 设置窗回调 ──
    {
        let work = work.clone();
        let model = bots_model.clone();
        let sw = settings.as_weak();
        let dirty = dirty.clone();
        settings.on_add_bot(move || {
            dirty.set(true);
            // 新 bot 默认类型跟随当前选中的 bot（没有选中则用 feishu 默认），
            // 避免旧默认 wechat 让用户以为新加的 bot 是微信（参数区显示微信登录框）。
            let sel = sw.upgrade().map(|w| w.get_selected()).unwrap_or(-1);
            let mut b = work.borrow_mut();
            let n = b.len() + 1;
            let kind = if sel >= 0 && (sel as usize) < b.len() {
                b[sel as usize].kind.clone()
            } else {
                crate::config::default_kind()
            };
            b.push(BotConfig {
                name: format!("bot{n}"), // 占位名；微信扫码登录成功后自动改成 wxN
                kind,
                ..Default::default()
            });
            let idx = b.len() as i32 - 1;
            sync_model(&model, &b);
            drop(b);
            if let Some(w) = sw.upgrade() {
                w.set_selected(idx);
            }
        });
    }
    {
        let work = work.clone();
        let model = bots_model.clone();
        let sw = settings.as_weak();
        let dirty = dirty.clone();
        settings.on_remove_bot(move |idx| {
            dirty.set(true);
            let mut b = work.borrow_mut();
            let i = idx as usize;
            if i < b.len() {
                b.remove(i);
            }
            let new_sel = if b.is_empty() {
                -1
            } else {
                (b.len() as i32 - 1).min(idx)
            };
            sync_model(&model, &b);
            drop(b);
            if let Some(w) = sw.upgrade() {
                w.set_selected(new_sel);
            }
        });
    }
    {
        let work = work.clone();
        let model = bots_model.clone();
        // 按字段回写（slint 侧只传被改的那一个字段）：杜绝「未改字段从过期 model 读回」
        // 导致的连改两字段互相回滚（旧 bot-edited 的 CRITICAL bug）。
        let dirty = dirty.clone();
        settings.on_bot_field_edited(move |idx, field, value| {
            dirty.set(true);
            let mut refresh = false;
            {
                let mut b = work.borrow_mut();
                if let Some(bot) = b.get_mut(idx as usize) {
                    match field.as_str() {
                        "name" => bot.name = value.to_string(),
                        "kind" => {
                            bot.kind = value.to_string();
                            // kind 决定编辑区显示哪套字段（slint 的 if 条件绑 model）；
                            // 不回写 model 的话，改类型后右侧仍显示旧类型的表单（如改「钉钉」还显示微信登录框）
                            refresh = true;
                        }
                        "backend" => {
                            bot.backend = value.to_string();
                            // 后端三选一 CheckBox：select-backend 已显式 set 勾选态，这里只回写
                            // work；切 bot 时由 refresh_exclusive_checks 经 backend-ui property
                            // 重新 set 勾选态（绕开用户交互会移除 checked 绑定的 slint 坑）。
                            refresh = true;
                        }
                        "provider" => bot.provider = value.to_string(),
                        "owner" => bot.owner_open_id = value.trim().to_string(),
                        "open_access" => {
                            bot.open_access = value.contains("任何人都可以对话");
                            refresh = true; // 对话权限互斥 CheckBox 同样依赖 model 重求值
                        }
                        "app_id" => bot.app_id = value.trim().to_string(),
                        "app_secret" => bot.app_secret = value.trim().to_string(),
                        "ding_user_id" => bot.ding_user_id = value.trim().to_string(),
                        "ding_owner_ids" => bot.ding_owner_ids = value.trim().to_string(),
                        "ding_open_access" => {
                            bot.ding_open_access = value.contains("任何人都可以对话");
                            refresh = true; // 同 open_access：互斥显示靠 model 重建
                        }
                        "ding_robot_code" => bot.ding_robot_code = value.trim().to_string(),
                        _ => {}
                    }
                }
            }
            if refresh {
                let b = work.borrow();
                sync_model(&model, &b);
            }
        });
    }
    {
        let work = work.clone();
        let model = bots_model.clone();
        let dirty = dirty.clone();
        settings.on_set_bot_enabled(move |idx, enabled| {
            dirty.set(true);
            {
                let mut b = work.borrow_mut();
                if let Some(bot) = b.get_mut(idx as usize) {
                    bot.enabled = enabled;
                }
            }
            // 同步回写 model：列表的 ⚪/停用 前缀与勾选框状态都绑 model，不刷新会显示旧值
            let b = work.borrow();
            sync_model(&model, &b);
        });
    }
    {
        let cdwork = cross_delivery_work.clone();
        let dirty = dirty.clone();
        settings.on_set_cross_delivery(move |enabled| {
            dirty.set(true);
            *cdwork.borrow_mut() = enabled;
        });
    }
    {
        // 切中别的 bot：重建 model 强制编辑区刷新。Slint ComboBox 的 current-value 是命令式
        // 快照，不随 model 数据变（LineEdit 会刷新但 ComboBox 不会），不重建则下拉还显示上一个
        // bot 的后端，看起来像「改了一个另一个也跟着变」。set_vec 触发 model 变更通知，ComboBox 重求值。
        let work = work.clone();
        let model = bots_model.clone();
        let sw = settings.as_weak();
        settings.on_selection_changed(move |_idx| {
            let b = work.borrow();
            sync_model(&model, &b);
            if let Some(w) = sw.upgrade() {
                refresh_owner_code_info(&w, &work);
                refresh_exclusive_checks(&w, &work);
            }
        });
    }
    {
        let tx = tx.clone();
        let work = work.clone();
        settings.on_fetch_bot_info(move |idx| {
            let b = work.borrow();
            if let Some(bot) = b.get(idx as usize) {
                let _ = tx.send(UiCmd::FetchBotInfo {
                    idx,
                    app_id: bot.app_id.clone(),
                    app_secret: bot.app_secret.clone(),
                });
            }
        });
    }
    {
        let tx = tx.clone();
        let work = work.clone();
        settings.on_wx_login(move |idx| {
            let b = work.borrow();
            if let Some(bot) = b.get(idx as usize) {
                let _ = tx.send(UiCmd::WxLogin {
                    idx,
                    bot_key: bot.key(),
                });
            }
        });
    }
    {
        let tx = tx.clone();
        let work = work.clone();
        let pwork = providers_work.clone();
        let dwork = default_provider_work.clone();
        let cdwork = cross_delivery_work.clone();
        let sw = settings.as_weak();
        let dirty = dirty.clone();
        settings.on_save_clicked(move || {
            dirty.set(false);
            if let Some(w) = sw.upgrade() {
                let (c, dropped) = snapshot_config(&work, &pwork, &dwork, &cdwork);
                let _ = tx.send(UiCmd::Save(c));
                // 保存后窗口保持打开（用户要求）：给个绿色确认，方便继续编辑或手动关闭。
                w.set_status_is_error(false);
                let mut msg = "✅ 已保存。窗口可继续编辑，不用了点「关闭」或红点关闭。".to_string();
                if dropped > 0 {
                    msg.push_str(&format!("（丢弃 {dropped} 个未命名供应商）"));
                }
                w.set_status_line(msg.into());
            }
        });
    }
    {
        let sw = settings.as_weak();
        let dlg = unsaved_dialog.as_weak();
        let dirty = dirty.clone();
        settings.on_cancel_clicked(move || {
            // 有未保存修改：先弹确认，别静默丢编辑（红点关闭走 winit 拦截，这里管「关闭」按钮）
            if dirty.get() {
                if let Some(d) = dlg.upgrade() {
                    show_window_and_focus(&d);
                }
                return;
            }
            if let Some(w) = sw.upgrade() {
                let _ = w.hide();
                platform::hide_dock();
            }
        });
    }
    // ── 通用确认弹窗（两种用途：① 未保存修改 → 保存/不保存；② 发现草稿 → 恢复/丢弃）──
    {
        let sw = settings.as_weak();
        let dw = unsaved_dialog.as_weak();
        let dirty = dirty.clone();
        unsaved_dialog.on_save_close(move || {
            // 未保存修改：复用「保存」同一路径（汇总工作副本写盘 + 服务在跑则重启），随后关设置窗
            if let Some(w) = sw.upgrade() {
                w.invoke_save_clicked();
                let _ = w.hide();
                platform::hide_dock();
            }
            dirty.set(false);
            if let Some(d) = dw.upgrade() {
                let _ = d.hide();
            }
        });
    }
    {
        let sw = settings.as_weak();
        let dw = unsaved_dialog.as_weak();
        let dirty = dirty.clone();
        unsaved_dialog.on_discard_close(move || {
            // 不保存：丢弃本次编辑（同时删掉自动草稿，避免下次再提示）
            dirty.set(false);
            Config::remove_draft();
            if let Some(w) = sw.upgrade() {
                let _ = w.hide();
                platform::hide_dock();
            }
            if let Some(d) = dw.upgrade() {
                let _ = d.hide();
            }
        });
    }
    {
        let dw = unsaved_dialog.as_weak();
        unsaved_dialog.on_stay(move || {
            if let Some(d) = dw.upgrade() {
                let _ = d.hide();
            }
        });
    }
    // 打开官方安装文档（缺失依赖时，点「安装文档」按钮 → 默认浏览器打开 url）
    settings.on_open_install_docs(|url| {
        platform::open_url(url.as_str());
    });
    // 「去授权」：跳到对应系统权限的设置面板（仅 macOS 有；面板 URL 与 deps.rs detect_permissions 一致）
    settings.on_open_perm_settings(|id| {
        let url = match id.as_str() {
            "full-disk" => "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles",
            "accessibility" => "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
            "screen" => "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
            "automation" => "x-apple.systempreferences:com.apple.preference.security?Privacy_Automation",
            "camera" => "x-apple.systempreferences:com.apple.preference.security?Privacy_Camera",
            "microphone" => "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone",
            _ => return,
        };
        platform::open_url(url);
    });
    // 「请求权限」：拉起子进程逐项触发屏幕录制/摄像头/麦克风授权弹框（不阻塞托盘主线程）。
    // 完成后把日志尾部带进状态区，并刷新权限状态。
    {
        let sw = settings.as_weak();
        settings.on_request_perms(move || {
            if let Some(w) = sw.upgrade() {
                if w.get_perm_busy() {
                    return; // 防连点
                }
                w.set_perm_busy(true);
                w.set_status_is_error(false);
                w.set_status_line("⏳ 逐项弹系统授权框（屏幕录制→摄像头→麦克风），请点「允许」…".into());
            }
            let sw2 = sw.clone();
            std::thread::spawn(move || {
                let exe = platform::current_exe().unwrap_or_default();
                let out = std::process::Command::new(exe)
                    .arg("--request-permissions")
                    .output();
                let tail = match out {
                    Ok(o) => {
                        let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
                        let e = String::from_utf8_lossy(&o.stderr).into_owned();
                        if !e.is_empty() {
                            if !s.is_empty() {
                                s.push('\n');
                            }
                            s.push_str(&e);
                        }
                        // 只留最后 3 行做状态摘要
                        let lines: Vec<&str> = s.lines().collect();
                        let n = lines.len();
                        lines[n.saturating_sub(3)..].join(" · ")
                    }
                    Err(e) => format!("请求进程启动失败：{e}"),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = sw2.upgrade() {
                        w.set_perm_busy(false);
                        push_perms_to_window(&w);
                        w.set_status_is_error(false);
                        w.set_status_line(format!("权限请求完成。{tail}").into());
                    }
                });
            });
        });
    }
    // 环境 Tab「重启服务生效」：授权/改权限后一键重启（权限对运行中进程不实时刷新）。
    {
        let tx = tx.clone();
        let sw = settings.as_weak();
        settings.on_restart_service_request(move || {
            let _ = tx.send(UiCmd::Restart);
            if let Some(w) = sw.upgrade() {
                w.set_status_is_error(false);
                w.set_status_line("🔁 正在重启服务，几秒后新权限生效…".into());
            }
        });
    }
    // Windows「以管理员重启」：弹 UAC 提权后重启自己（成功则当前进程退出）。
    {
        let sw = settings.as_weak();
        settings.on_relaunch_as_admin(move || {
            #[cfg(target_os = "windows")]
            {
                match crate::deps::relaunch_as_admin() {
                    Ok(()) => {
                        // 提权成功 → 当前进程立即退出，新实例以管理员身份启动
                        std::process::exit(0);
                    }
                    Err(e) => {
                        if let Some(w) = sw.upgrade() {
                            w.set_status_is_error(true);
                            w.set_status_line(format!("提权失败：{e}").into());
                        }
                    }
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                if let Some(w) = sw.upgrade() {
                    w.set_status_is_error(true);
                    w.set_status_line("此功能仅 Windows 需要".into());
                }
            }
        });
    }
    // 「重新检测」按钮：重跑依赖检查并回填状态（用户装完 CLI 不用重开设置窗）
    {
        let sw = settings.as_weak();
        settings.on_recheck_deps(move || {
            if let Some(w) = sw.upgrade() {
                push_deps_to_window(&w);
                push_perms_to_window(&w);
                w.set_status_is_error(false);
                let all = crate::deps::detect_all();
                let missing: Vec<&str> = all.iter().filter(|d| !d.found).map(|d| d.label).collect();
                let msg = if missing.is_empty() {
                    "✅ 全部依赖均已安装".to_string()
                } else {
                    format!("⚠️ 缺失：{}", missing.join("、"))
                };
                w.set_status_line(msg.into());
            }
        });
    }
    // 「安装」按钮：装某个依赖（后台跑，dep-busy 防重复；结果经 dep_rx 回主线程）
    {
        let tx = tx.clone();
        let sw = settings.as_weak();
        settings.on_install_dep(move |dep_id| {
            if let Some(w) = sw.upgrade() {
                if !w.get_dep_busy().is_empty() {
                    return; // 已有安装在进行
                }
                w.set_dep_busy(dep_id.clone());
                w.set_status_is_error(false);
                w.set_status_line(format!("⏳ 开始安装 {dep_id} …").into());
            }
            let _ = tx.send(UiCmd::InstallDep(dep_id.to_string()));
        });
    }

    // 「生成授权码」：作废旧码、生成新码落盘，回填展示 + 状态行。role=owner 生成管理员码。
    // 同步操作（config 本地读写）。
    {
        let work = work.clone();
        let sw = settings.as_weak();
        settings.on_generate_owner_code(move |idx, role| {
            let Some(w) = sw.upgrade() else { return };
            let Some(bot) = work.borrow().get(idx as usize).cloned() else { return };
            let open = if bot.is_dingtalk() {
                bot.ding_open_access
            } else {
                bot.open_access
            };
            if open {
                // 公开模式无需授权（按钮已禁用，双保险防参数矛盾）
                w.set_status_is_error(true);
                w.set_status_line("❌ 公开模式下任何人都可对话，无需生成授权码（请先切回「仅授权用户」）".into());
                return;
            }
            match crate::config::Config::generate_owner_code(&bot.key(), role.as_str()) {
                Some((code, _expires)) => {
                    let label = if role == "owner" { "管理员授权码" } else { "授权码" };
                    w.set_status_is_error(false);
                    w.set_status_line(
                        format!("✅ 已生成{label} {code}：发给对方，由 ta 私聊发给本 bot 完成授权（同类型旧码已作废）").into(),
                    );
                    refresh_owner_code_info(&w, &work);
                }
                None => {
                    w.set_status_is_error(true);
                    w.set_status_line("❌ 生成授权码失败（config 写入失败）".into());
                }
            }
        });
    }

    // 「复制授权码」：把指定 role 的未过期码写入系统剪贴板（pbcopy/clip/xclip）。
    {
        let work = work.clone();
        let sw = settings.as_weak();
        settings.on_copy_owner_code(move |role| {
            let Some(w) = sw.upgrade() else { return };
            let Some(bot) = work.borrow().get(w.get_selected() as usize).cloned() else {
                return;
            };
            let code = crate::config::Config::pending_owner_codes(&bot.key())
                .into_iter()
                .find(|(r, _, _)| r == role.as_str())
                .map(|(_, code, _)| code);
            match code {
                Some(code) if crate::platform::copy_to_clipboard(&code) => {
                    w.set_status_is_error(false);
                    w.set_status_line(format!("✅ 授权码 {code} 已复制到剪贴板").into());
                }
                Some(_) => {
                    w.set_status_is_error(true);
                    w.set_status_line("❌ 复制失败（系统剪贴板不可用）".into());
                }
                None => {
                    w.set_status_is_error(true);
                    w.set_status_line("❌ 没有有效授权码，请先生成".into());
                }
            }
        });
    }

    // 后端 / 对话权限互斥选项的勾选回调：写 work + 重算 option model（整体替换 → CheckBox 重建）。
    {
        let work = work.clone();
        let sw = settings.as_weak();
        settings.on_backend_option_toggled(move |i| {
            let Some(w) = sw.upgrade() else { return };
            let val = ["claude", "codex", "pi"][i as usize];
            if let Some(bot) = work.borrow_mut().get_mut(w.get_selected() as usize) {
                bot.backend = val.to_string();
            }
            refresh_exclusive_checks(&w, &work);
        });
    }
    {
        let work = work.clone();
        let sw = settings.as_weak();
        settings.on_access_option_toggled(move |i| {
            let Some(w) = sw.upgrade() else { return };
            if let Some(bot) = work.borrow_mut().get_mut(w.get_selected() as usize) {
                bot.open_access = i == 1;
            }
            refresh_exclusive_checks(&w, &work);
        });
    }
    {
        let work = work.clone();
        let sw = settings.as_weak();
        settings.on_ding_access_option_toggled(move |i| {
            let Some(w) = sw.upgrade() else { return };
            if let Some(bot) = work.borrow_mut().get_mut(w.get_selected() as usize) {
                bot.ding_open_access = i == 1;
            }
            refresh_exclusive_checks(&w, &work);
        });
    }

    // 「取消授权」：从该 bot 授权者列表移除某用户（config 落盘 + 刷新列表）。
    {        let work = work.clone();
        let model = bots_model.clone();
        let sw = settings.as_weak();
        settings.on_remove_granted(move |bot_idx, granted_idx| {
            let Some(w) = sw.upgrade() else { return };
            let Some(bot) = work.borrow().get(bot_idx as usize).cloned() else {
                return;
            };
            // 授权者展示名列表按 bot kind 取（飞书 granted_infos / 钉钉 ding_granted_infos）
            let open_id = if bot.is_dingtalk() {
                bot.ding_granted_infos
                    .get(granted_idx as usize)
                    .map(|i| i.open_id.clone())
            } else {
                bot.granted_infos.get(granted_idx as usize).map(|i| i.open_id.clone())
            }
            .unwrap_or_default();
            if open_id.is_empty() {
                return;
            }
            if crate::config::Config::remove_granted(&bot.key(), &open_id) {
                w.set_status_is_error(false);
                w.set_status_line(format!("✅ 已取消授权 {open_id}").into());
                // 同步更新工作副本 + model（列表即时消失）；config watch 也会兜底刷新
                if let Some(b) = work.borrow_mut().get_mut(bot_idx as usize) {
                    crate::config::remove_granted_from_bot(b, &open_id);
                }
                let bots = work.borrow().clone();
                sync_model(&model, &bots);
                w.set_selected(bot_idx);
            } else {
                w.set_status_is_error(true);
                w.set_status_line("❌ 取消授权失败（config 写入失败）".into());
            }
        });
    }

    // ── 供应商编辑回调（per-field 纪律与 bot 一致）──
    {
        let pwork = providers_work.clone();
        let dirty = dirty.clone();
        settings.on_provider_field_edited(move |idx, field, value| {
            dirty.set(true);
            let mut pv = pwork.borrow_mut();
            if let Some(p) = pv.get_mut(idx as usize) {
                match field.as_str() {
                    "name" => p.name = value.trim().to_string(),
                    "kind" => p.kind = value.to_string(),
                    "base_url" => p.base_url = value.trim().to_string(),
                    "api_key" => p.api_key = value.to_string(), // 密钥不 trim（可能含有意空白）
                    "model" => p.model = value.trim().to_string(),
                    _ => {}
                }
            }
        });
    }
    {
        // 切中别的供应商：重建 model 强制编辑区刷新（同 bot 的 ComboBox current-value 陈旧坑）
        let pwork = providers_work.clone();
        let pmodel = providers_model.clone();
        let dwork = default_provider_work.clone();
        settings.on_provider_selection_changed(move |_idx| {
            let pv = pwork.borrow();
            let d = dwork.borrow();
            sync_providers_model(&pmodel, &pv, &d);
        });
    }
    {
        let pwork = providers_work.clone();
        let pmodel = providers_model.clone();
        let dwork = default_provider_work.clone();
        let sw = settings.as_weak();
        let dirty = dirty.clone();
        settings.on_add_provider(move || {
            dirty.set(true);
            let mut pv = pwork.borrow_mut();
            let n = pv.len() + 1;
            pv.push(ProviderConfig {
                name: format!("provider{n}"),
                kind: "anthropic".into(),
                ..Default::default()
            });
            let idx = pv.len() as i32 - 1;
            let d = dwork.borrow().clone();
            sync_providers_model(&pmodel, &pv, &d);
            drop(pv);
            if let Some(w) = sw.upgrade() {
                w.set_provider_selected(idx);
            }
        });
    }
    {
        let pwork = providers_work.clone();
        let pmodel = providers_model.clone();
        let dwork = default_provider_work.clone();
        let sw = settings.as_weak();
        let dirty = dirty.clone();
        settings.on_remove_provider(move |idx| {
            dirty.set(true);
            let mut pv = pwork.borrow_mut();
            let i = idx as usize;
            if i < pv.len() {
                pv.remove(i);
            }
            let new_sel = if pv.is_empty() {
                -1
            } else {
                (pv.len() as i32 - 1).min(idx)
            };
            let d = dwork.borrow().clone();
            sync_providers_model(&pmodel, &pv, &d);
            let names = build_provider_names(&pv);
            drop(pv);
            if let Some(w) = sw.upgrade() {
                w.set_provider_selected(new_sel);
                w.set_provider_names(slint::ModelRc::from(Rc::new(slint::VecModel::from(names))));
            }
        });
    }
    {
        let pwork = providers_work.clone();
        let pmodel = providers_model.clone();
        let dwork = default_provider_work.clone();
        let dirty = dirty.clone();
        settings.on_set_default_provider(move |idx| {
            dirty.set(true);
            let pv = pwork.borrow_mut();
            if let Some(p) = pv.get(idx as usize) {
                if !p.name.is_empty() {
                    *dwork.borrow_mut() = p.name.clone();
                }
            }
            let d = dwork.borrow().clone();
            sync_providers_model(&pmodel, &pv, &d);
        });
    }
    {
        let tx = tx.clone();
        let pwork = providers_work.clone();
        let sw = settings.as_weak();
        settings.on_test_provider(move |idx| {
            let pv = pwork.borrow();
            if let Some(p) = pv.get(idx as usize) {
                let snapshot = p.clone();
                drop(pv);
                if let Some(w) = sw.upgrade() {
                    w.set_status_is_error(false);
                    w.set_status_line(format!("⏳ 测试 {} 连通性…", snapshot.name).into());
                }
                let _ = tx.send(UiCmd::TestProvider { idx, snapshot });
            }
        });
    }

    push_status(&tray, &install::status());
    tray.show()?;

    // GUI 是 service 的看门：确保 service 在跑（已配置才拉）
    if Config::load().map(|c| c.is_configured()).unwrap_or(false) {
        let _ = tx.send(UiCmd::Start);
    } else {
        // 关键：显示前必须 load_into——否则窗口显示空列表，此时点保存会用空 work 副本
        // 覆盖 config，把磁盘上已有的 bots 全部抹掉（半配置状态：配了但 is_configured 为假）。
        // 此处仍在事件循环启动前的主线程设置阶段，直接调用即可（Rc 非 Send，
        // 不能塞进 invoke_from_event_loop）。
        let restored = load_with_draft(
            &settings,
            &dirty,
            &work,
            &bots_model,
            &providers_work,
            &providers_model,
            &default_provider_work,
            &cross_delivery_work,
        );
        // 已静默恢复草稿时保留恢复提示，别被「请先添加」覆盖
        if !restored {
            settings.set_status_is_error(false);
            settings.set_status_line("请先添加一个飞书/微信机器人".into());
        }
        startup_shown.store(true, Ordering::Relaxed);
        show_window_and_focus(&settings);
    }

    // ── 主线程定时器：① 看门 ② 刷新托盘 ③ 抽 bot_rx/wx_rx 回填 ──
    let timer = slint::Timer::default();
    {
        let settings_weak = settings.as_weak();
        let qr_weak = qr_dialog.as_weak();
        let work = work.clone();
        let model = bots_model.clone();
        let tray_hold = tray;
        let tray_weak = tray_hold.as_weak();
        let dirty = dirty.clone();
        let providers_work_t = providers_work.clone();
        let default_provider_work_t = default_provider_work.clone();
        let cross_delivery_work_t = cross_delivery_work.clone();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(2),
            move || {
                let _keep = &tray_hold;
                // 每 tick 只查一次进程状态（install::status 内部 fork ps，别重复查）
                let st = install::status();
                // 看门：意图=运行但进程不在 → 崩溃，重拉（用户手动停止会清 desired，不覆盖）
                if install::is_desired() && !st.running {
                    crate::log!("[watchdog] service 意外退出，自动重拉");
                    let _ = install::svc_start();
                }
                if let Some(t) = tray_weak.upgrade() {
                    push_status(&t, &st);
                }
                // 同步设置窗的动态标题 + 顶部运行概览（隐藏也更新，下次打开即最新）
                if let Some(w) = settings_weak.upgrade() {
                    push_settings_status(&w, &st);
                }
                // 草稿自动保存：有未保存修改就每 tick 落盘一次（小文件 + 原子写，成本可忽略）
                if dirty.get() {
                    let (draft, _dropped) = snapshot_config(
                        &work,
                        &providers_work_t,
                        &default_provider_work_t,
                        &cross_delivery_work_t,
                    );
                    if let Err(e) = draft.save_draft() {
                        crate::log!("[gui] 草稿自动保存失败: {e:#}");
                    }
                }
                // 依赖安装结果：清 dep-busy，刷新检测状态，报结果
                while let Ok((dep_id, r)) = dep_rx.try_recv() {
                    if let Some(w) = settings_weak.upgrade() {
                        w.set_dep_busy("".into());
                        push_deps_to_window(&w);
                        match r {
                            Ok(_) => {
                                w.set_status_is_error(false);
                                w.set_status_line(format!("✅ {dep_id} 安装完成").into());
                            }
                            Err(e) => {
                                w.set_status_is_error(true);
                                w.set_status_line(format!("⚠️ {e}").into());
                            }
                        }
                    }
                }
                // 供应商测试结果
                while let Ok((_idx, r)) = prov_rx.try_recv() {
                    if let Some(w) = settings_weak.upgrade() {
                        match r {
                            Ok(msg) => {
                                w.set_status_is_error(false);
                                w.set_status_line(msg.into());
                            }
                            Err(e) => {
                                w.set_status_is_error(true);
                                w.set_status_line(e.into());
                            }
                        }
                    }
                }
                // 回填「自动获取」的 bot 名/open_id 到工作副本 + model
                while let Ok((idx, r)) = bot_rx.try_recv() {
                    if let Some(w) = settings_weak.upgrade() {
                        match r {
                            Ok((name, oid)) => {
                                dirty.set(true); // 自动获取回填了 bot 名/open_id，属于编辑
                                let mut b = work.borrow_mut();
                                if let Some(bot) = b.get_mut(idx as usize) {
                                    bot.bot_name = name.clone();
                                    bot.bot_open_id = oid.clone();
                                    if bot.name.is_empty() {
                                        bot.name = name.clone();
                                    }
                                }
                                sync_model(&model, &b);
                                w.set_status_line("".into());
                            }
                            Err(e) => w.set_status_line(format!("自动获取失败：{e}").into()),
                        }
                    }
                }
                // 微信扫码登录事件：二维码就绪/成功/失败
                while let Ok(evt) = wx_rx.try_recv() {
                    match evt {
                        WxEvt::QrReady(path) => {
                            if let Some(d) = qr_weak.upgrade() {
                                match slint::Image::load_from_path(&path) {
                                    Ok(img) => {
                                        d.set_qr_image(img);
                                        d.set_tip("请用微信扫描下方二维码".into());
                                        show_window_and_focus(&d);
                                    }
                                    Err(_) => {
                                        if let Some(w) = settings_weak.upgrade() {
                                            w.set_status_line("二维码加载失败，请重试".into());
                                        }
                                    }
                                }
                            }
                        }
                        WxEvt::Confirmed(idx, login) => {
                            // 扫码完成：关掉二维码弹窗即可（设置窗保持打开、置前聚焦，
                            // 让用户看到「登录成功」并点保存——用户要求扫码后窗口不关）。
                            dirty.set(true); // 扫码登录写回了 token/name，属于编辑
                            if let Some(d) = qr_weak.upgrade() {
                                let _ = d.hide();
                            }
                            if let Some(w) = settings_weak.upgrade() {
                                let mut b = work.borrow_mut();
                                if let Some(bot) = b.get_mut(idx as usize) {
                                    bot.wx_token = login.token.clone();
                                    bot.wx_base_url = login.base_url.clone();
                                    bot.wx_user_id = login.user_id.clone();
                                    if bot.bot_name.is_empty() {
                                        bot.bot_name = format!("微信 {}", login.user_id);
                                    }
                                    // 占位名（添加时的 "botN"）登录成功后才覆盖成 wxN
                                    if bot.name.is_empty() || bot.name.starts_with("bot") {
                                        bot.name = format!(
                                            "wx{}",
                                            crate::agent::truncate(&login.user_id, 6)
                                        );
                                    }
                                }
                                sync_model(&model, &b);
                                drop(b);
                                w.set_status_is_error(false);
                                w.set_status_line(
                                    "✅ 微信登录成功！点「保存」写入配置并重启服务。".into(),
                                );
                                show_window_and_focus(&w);
                            }
                        }
                        WxEvt::Failed(e) => {
                            if let Some(d) = qr_weak.upgrade() {
                                let _ = d.hide();
                                platform::hide_dock();
                            }
                            if let Some(w) = settings_weak.upgrade() {
                                w.set_status_line(format!("微信登录失败：{e}").into());
                            }
                        }
                    }
                }
            },
        );
    }

    slint::run_event_loop()?;
    Ok(())
}

/// 测试供应商连通性（best-effort，10s 超时）。返回用户可读结果，绝不含 api_key。
/// openai-chat/responses：GET {base}/models 带 Bearer；anthropic：POST {base}/v1/messages 最小体。
async fn test_provider(p: &ProviderConfig) -> std::result::Result<String, String> {
    if p.base_url.is_empty() {
        return Err("Base URL 为空".into());
    }
    if p.api_key.is_empty() {
        return Err("API Key 为空（测试前请先在输入框填密钥）".into());
    }
    let base = p.base_url.trim_end_matches('/');
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let label = format!("{}（{}）", p.name, p.kind);
    match p.kind.as_str() {
        "openai-chat" | "openai-responses" => {
            let url = format!("{base}/models");
            let resp = client
                .get(&url)
                .bearer_auth(&p.api_key)
                .send()
                .await
                .map_err(|e| format!("连接失败：{e}"))?;
            let code = resp.status().as_u16();
            if code == 200 {
                Ok(format!("✅ {label} 连通正常"))
            } else if code == 401 || code == 403 {
                Err(format!("{label} 认证失败（HTTP {code}），检查 API Key"))
            } else {
                Err(format!("{label} 返回 HTTP {code}（能连上但响应异常，检查 Base URL/模型）"))
            }
        }
        "anthropic" => {
            let url = format!("{base}/v1/messages");
            let model = if p.model.is_empty() {
                "claude-haiku-4-5"
            } else {
                p.model.as_str()
            };
            let body = serde_json::json!({
                "model": model,
                "max_tokens": 1,
                "messages": [{"role": "user", "content": "hi"}],
            });
            let resp = client
                .post(&url)
                .header("x-api-key", &p.api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("连接失败：{e}"))?;
            let code = resp.status().as_u16();
            if code == 200 {
                Ok(format!("✅ {label} 连通正常"))
            } else if code == 401 || code == 403 {
                Err(format!("{label} 认证失败（HTTP {code}），检查 API Key"))
            } else if code == 400 || code == 404 {
                // 400/404：认证过了但请求体/模型名问题——连接本身是通的
                Ok(format!("✅ {label} 可达（HTTP {code}：认证通过，检查模型名「{model}」）"))
            } else {
                Err(format!("{label} 返回 HTTP {code}"))
            }
        }
        other => Err(format!("未知供应商类型：{other}")),
    }
}

/// 微信扫码登录后台流程：拉二维码 → 渲染 PNG 落盘 → 路径回主线程在弹窗显示 → 长轮询。
/// 二维码直接画在 GUI 弹窗里（Image::load_from_path），不依赖系统看图。
async fn run_wx_login(idx: i32, bot_key: &str, tx: std_mpsc::Sender<WxEvt>) {
    // 整个登录流程；出错成 String 由调用处统一发 Failed。
    async fn attempt(idx: i32, bot_key: &str, tx: &std_mpsc::Sender<WxEvt>) -> Result<(), String> {
        // 1) 拿二维码 → 渲染 PNG 落盘 → 通知主线程显示
        let (qrcode, img) = crate::wechat::fetch_qrcode()
            .await
            .map_err(|e| format!("拉二维码失败: {e:#}"))?;
        let path = crate::wechat::save_qrcode_image(bot_key, &img)
            .map_err(|e| format!("二维码渲染失败: {e:#}"))?;
        let _ = tx.send(WxEvt::QrReady(path));
        // 2) 长轮询扫码状态（最多 ~5 分钟）
        for _ in 0..150 {
            match crate::wechat::poll_qr_status(&qrcode).await {
                Ok(Some(login)) => {
                    let _ = tx.send(WxEvt::Confirmed(idx, login));
                    return Ok(());
                }
                Ok(None) => tokio::time::sleep(Duration::from_secs(2)).await,
                Err(e) => return Err(format!("{e:#}")),
            }
        }
        Err("扫码超时（5 分钟未确认）".into())
    }
    if let Err(e) = attempt(idx, bot_key, &tx).await {
        let _ = tx.send(WxEvt::Failed(e));
    }
}
