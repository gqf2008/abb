//! GUI —— Slint 托盘控制器 + 多 bot 设置窗（兼作 service 的看门）。
//!
//! 设置窗为多 bot 主从结构：左列表（VecModel<BotRow>）+ 右编辑选中项。
//! 编辑在一份「工作副本」（Rc<RefCell<Vec<BotConfig>>>）上进行，保存时才写回 config.json。
//!
//! 看门：GUI 启动拉起 service 子进程；托盘 Timer 周期探测，崩溃自动重拉（见 install.rs）。
//! 打开日志/目录走 platform::open_path（跨平台）。

use crate::config::{first_owner_id, BotConfig, Config, ProviderConfig};
use crate::dingtalk::DingTalkClient;
use crate::feishu::FeishuClient;
use crate::install;
use crate::platform;
use crate::virtualbot::{
    builtin_templates, format_created, RoleTemplate, VirtualBot, VirtualBotStore, ROLE_NAME_MAX,
    ROLE_PROMPT_MAX,
};
use anyhow::Result;
// Model trait 导入：ModelRc 的方法（row_count/row_data/set_row_data）在 trait 上
use slint::Model as _;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;
use tokio::sync::mpsc;

slint::include_modules!();

// 像素自绘窗口接线：PixelTitleBar 的拖拽/最小化/最大化/关闭 → 系统窗口控制。
// 关闭 = 隐藏窗口不退出（托盘应用惯例，install_title_bar_controls_no_quit）。
slint_pixel::impl_title_bar_ui!(SettingsWindow);

// 系统级对话窗也用像素自绘标题栏（native-move + 关闭→隐藏）。
slint_pixel::impl_title_bar_ui!(SystemChatWindow);

enum UiCmd {
    Start,
    Stop,
    Restart,
    Save(Box<Config>),
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
    /// 安装某个依赖（node/lark-cli/dingtalk-cli/git）。结果经 dep_rx 回主线程。
    InstallDep(String),
    /// #60 一键安装全部缺失组件（node 前置自动装）。逐项进度/汇总经 dep_rx 回主线程。
    InstallAllMissing,
    /// 测试某个供应商连通性（快照里 api_key 已就绪）。结果经 prov_rx 回主线程。
    TestProvider {
        idx: i32,
        snapshot: ProviderConfig,
    },
    OpenLogs,
    OpenFolder,
    /// 版本升级：检查最新 release（silent=启动/周期自动检查，失败静默不打扰）。
    CheckUpdate {
        silent: bool,
    },
    /// 下载并安装已发现的新版本，成功安装后退出本进程（新版由分离脚本/安装器拉起）。
    InstallUpdate,
    /// 虚拟 Bot 批量创建（#75）：逐群调平台 API（现造 FeishuClient/DingTalkClient，
    /// 与 FetchBotInfo 同路径——后台线程不依赖 service 进程）→ 成功后写登记表。
    /// items = (角色名, 提示词)。
    VirtualBotCreate {
        bot_key: String,
        kind: String,
        app_id: String,
        app_secret: String,
        /// 建群群主（飞书必填；点击时从工作副本 bot 解析，None = 未配置 owner）。
        owner: Option<String>,
        items: Vec<(String, String)>,
    },
    /// 编辑保存：PATCH 平台群资料（改名/改介绍）+ 登记表角色名同步。
    VirtualBotUpdate {
        bot_key: String,
        kind: String,
        app_id: String,
        app_secret: String,
        chat_id: String,
        name: String,
        prompt: String,
        /// owner open_id（权限不足时发授权指引；None = 未配置则不提示）。
        owner: Option<String>,
    },
    /// 手动登记（#75 降级路径）：只写登记表，不调平台 API（平台手动建群后登记）。
    VirtualBotRegister {
        bot_key: String,
        name: String,
        chat_id: String,
    },
    /// 取消登记（平台群保留，只删 ABB 登记）。
    VirtualBotDeregister {
        bot_key: String,
        chat_id: String,
    },
    /// 解散群（红色强确认后）：调平台删除接口 + 移除登记。仅飞书有解散 API。
    VirtualBotDisband {
        bot_key: String,
        kind: String,
        app_id: String,
        app_secret: String,
        chat_id: String,
        /// owner open_id（权限不足时发授权指引；None = 未配置则不提示）。
        owner: Option<String>,
    },
    /// 编辑预填：拉平台群资料（(群名, 群介绍)）→ 回填弹窗。
    VirtualBotFetchInfo {
        kind: String,
        app_id: String,
        app_secret: String,
        chat_id: String,
    },
    /// 手动刷新（虚拟 Bot section「⟳ 刷新」）：逐个登记群调 get_chat_info 验证存在性——
    /// 平台解散群的兜底（im.chat.deleted 事件可能因未订阅/丢失不达）；确认已解散的群
    /// 移除登记 + 归档会话历史。get_chat_info 失败但错误不像"群不存在"（网络/权限等）
    /// 保留登记并提示，避免误删。
    VirtualBotVerify {
        bot_key: String,
        kind: String,
        app_id: String,
        app_secret: String,
    },
    /// 「✨ 生成」提示词（8-20 需求）：根据角色名让 LLM 写系统提示词（≤100 字符），
    /// 走该 bot 生效后端的一次性轻量 CLI 调用，结果回填弹窗。
    GeneratePrompt {
        bot_key: String,
        name: String,
    },
    /// #141 一键创建团队（真实数据流）：LLM 生成团队方案（teambuilder），结果经 team_rx 回主线程。
    TeamGenerate {
        idx: i32,
        bot_key: String,
        target: String,
    },
    /// #141 确认建群：teamflow::create_team_groups（幂等）逐角色建群 + 登记，结果经 team_rx 回主线程。
    TeamCreate {
        idx: i32,
        bot_key: String,
        plan: String,
    },
    /// #147 任命成员：写团队登记表（member="" = 恢复「待任命」）。结果经 team_rx 回主线程。
    TeamAppoint {
        bot_key: String,
        team_name: String,
        role_name: String,
        member: String,
    },
    /// #147 解散团队（红色强确认后）：移除全部角色虚拟 Bot 登记 + 归档聊天历史 + 删团队条目。
    TeamDissolve {
        bot_key: String,
        team_name: String,
    },
    /// 系统级对话：跑一轮与 abb-agent 的直接对话（syschat::turn），
    /// 结果经 syschat_rx 回主线程（气泡/对话框共用）。
    /// `atts` = 本批已落盘的待发附件（路径进 prompt，见 `syschat::prompt_with_attachments`）。
    /// `cancel` = ⏹ 停止用的外部取消令牌（用户点停止时由 UI 线程直接 `.cancel()`）。
    /// `inject_rx` / `cap_tx` = 卡 `abb-syschat-d10-insert-interrupt` 的生成中插入接线：
    /// 后端 steer 能力经 `cap_tx` 上报，用户在生成中发来的消息经 `inject_rx` 插入在途回合。
    SysChatSend {
        text: String,
        atts: Vec<crate::syschat::Attachment>,
        cancel: tokio_util::sync::CancellationToken,
        inject_rx: tokio::sync::mpsc::UnboundedReceiver<crate::runtime::queue::InboundMsg>,
        cap_tx: tokio::sync::watch::Sender<crate::runtime::harness::SteerSupport>,
    },
    /// 后台下载一个 http(s) 媒体到本地缓存（图片内联 / 音视频可打开）。
    FetchMedia {
        url: String,
    },
}

/// #141 一键创建团队结果（后台 → 主线程）。
enum TeamEvt {
    /// 生成完成：Ok(TeamPlan JSON) / Err(用户可操作错误文案)。
    Generate {
        result: std::result::Result<String, String>,
    },
    /// 确认建群完成：Ok(逐角色创建结果) / Err(整体错误)。
    Create {
        result: std::result::Result<Vec<(String, String, bool, String)>, String>,
    },
    /// #147 团队管理操作完成（任命/解散）：Ok(摘要) / Err(错误文案)。
    Manage {
        result: std::result::Result<String, String>,
    },
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

/// 系统级对话事件（后台 → 主线程）：气泡与对话框共用一条通道。
/// 回合跑在后台 tokio 线程（GUI 主线程不能阻塞），结果经 std mpsc 回主线程，
/// 由专用的 50ms 流式定时器轮询上屏（E1；与 dep_rx/prov_rx/vb_rx 的 2s 轮询同模式，
/// 只是节拍更快以保证打字机手感）。
enum SysChatEvt {
    /// 流式分片（E1 正文 / E2 思考）：按 kind 分段累积，分别喂两路打字机。
    Delta {
        kind: crate::runtime::acp::ChunkKind,
        text: String,
    },
    /// 回合成功：助手回复文本（以它为准收尾流式气泡，见去重口径）。
    Reply(String),
    /// 用户停止：保留已生成文本，提示「已停止」。
    Stopped,
    /// 回合失败：用户可读原因。
    Error(String),
    /// 后台已把一个 http(s) 媒体下到缓存：重载当前会话让图片内联显示。
    MediaReady,
}

/// 流式回合的**结局态**（GUI 侧）：Reply 要等打字机把已收到文本推完再收尾
///（否则大段 chunk 会一次性蹦出），Stopped 已经 `flush` 过、当拍即可收尾。
#[derive(Clone, PartialEq, Eq)]
enum StreamEnd {
    Reply(String),
    Stopped,
}

/// 虚拟 Bot 操作结果（#75，后台 → 主线程）：创建/编辑/登记/解散统一形状。
/// 走 std mpsc（与 dep_rx/prov_rx 同模式：主线程 2s 定时器轮询，不阻塞事件循环）。
enum VirtualBotEvt {
    /// 批量创建进度：done=已完成数（0-based），total=总数。弹窗显示「创建中 N/total」。
    Progress { done: usize, total: usize },
    /// 全部结束：逐项结果（角色名 → Ok(摘要) / Err(错误文案)）。
    Done {
        results: Vec<(String, std::result::Result<String, String>)>,
    },
    /// 编辑预填：平台群资料已拉回（name/desc 回填弹窗；error=读取失败文案）。
    Fetched {
        chat_id: String,
        name: String,
        desc: String,
        error: Option<String>,
    },
    /// 「✨ 生成」结果回填：text=生成成功的提示词；error=生成失败文案。
    PromptGenerated {
        text: Option<String>,
        error: Option<String>,
    },
}

/// 确认弹窗待执行的虚拟 Bot 操作（#75）：取消登记（轻确认）/ 解散群（红色强确认）。
/// 主线程持有；弹确认窗前写入，确认回调里 clone 并发送对应 UiCmd（失败态「重试」
/// 保留原 action，成功才 take 清空），取消/红点关闭时清空。
#[derive(Clone)]
enum VbAction {
    /// 取消登记：平台群保留，只删 ABB 登记。
    Deregister { bot_key: String, chat_id: String },
    /// 解散群：调平台删除接口（仅飞书有）+ 移除登记，不可恢复。
    Disband {
        bot_key: String,
        kind: String,
        app_id: String,
        app_secret: String,
        chat_id: String,
        /// owner open_id（权限不足时发授权指引给 owner；None = 未配置则不提示）。
        owner: Option<String>,
    },
}

/// #147 解散团队确认弹窗待执行操作（与 VbAction 同款：确认回调 clone 发 UiCmd，
/// 成功才 take 清空，失败保留可重试）。与 VbAction 共用 vb_confirm 弹窗实例，
/// 互斥存在（同一时刻只会有一个待操作）。
#[derive(Clone)]
enum TeamAction {
    /// 解散团队：移除全部角色登记 + 归档聊天历史 + 删团队条目（平台群物理保留）。
    Dissolve { bot_key: String, team_name: String },
}

/// 依赖安装的结果事件（后台 → 主线程）。
enum DepEvt {
    /// 单项安装完成（dep_id + 结果）。
    Done {
        dep_id: String,
        result: std::result::Result<String, String>,
    },
    /// #60 一键装：每开始一项发一次（label 为展示名，idx 1-based）。
    AllProgress {
        label: String,
        idx: usize,
        total: usize,
    },
    /// #60 一键装：全部结束的如实汇总。
    AllDone(crate::deps::AllInstallOutcome),
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

/// #141 团队方案 → 预览行（role_name, member, duty），供 TeamDialog 角色列表消费。
/// #162：role_name 为完整三段（项目-角色-姓名，含成员），member 列置空——避免
/// 「斯蒂芬（斯蒂芬）」重复；既有 GUI 列表（登记表/团队条目）显示三段自动生效。
fn team_plan_rows(plan: &crate::teambuilder::TeamPlan) -> Vec<(String, String, String)> {
    plan.roles
        .iter()
        .map(|r| {
            (
                crate::teamflow::full_role_name(
                    &plan.team_name,
                    &r.role_name,
                    r.member_name.as_deref(),
                ),
                String::new(), // #162：三段名已含成员，member 列不再显示
                r.system_prompt.clone(),
            )
        })
        .collect()
}

/// #141 单角色创建结果行文案（成功/失败清单展示）：`角色 → 详情`。
/// #162：role_name 已是完整三段（含成员），不再拼（成员）括号。
fn team_create_line(role_name: &str, detail: &str) -> String {
    format!("{role_name} → {detail}")
}

/// 把已查好的服务状态写进 Tray 属性。主线程调用（status 由调用方查，避免重复 fork ps）。
/// 托盘菜单里 bot 的显示名：空名回落通道类型；过长（如微信「微信 {完整 ilink user_id}」）
/// 截断加省略号，否则会把原生 NSMenu 撑得过宽。完整名仍在设置窗/配置里。
fn display_name(name: &str, kind: &str) -> String {
    const MAX: usize = 16;
    let base = if name.is_empty() {
        kind_label(kind)
    } else {
        name.to_string()
    };
    let chars: Vec<char> = base.chars().collect();
    if chars.len() <= MAX {
        base
    } else {
        chars.iter().take(MAX - 1).collect::<String>() + "…"
    }
}

/// 服务动作（启动/停止/重启）失败时的 toast 文案。
///
/// 单行、给**动作 + 原因**：toast 是用户唯一能看到的反馈（细节留在 logs/bridge.out）。
/// Windows 上追加可操作提示 —— 「权限不足」是高完整性形态最常见的失败原因，而这一态下
/// 普通权限怎么点都不会生效，必须告诉用户往哪走（2026-09-29 owner 实报「停不了也没提示」）。
fn svc_action_toast(action: &str, err: &anyhow::Error) -> String {
    let hint = if cfg!(target_os = "windows") {
        "（若为权限不足：用设置页「以管理员身份重启」，或检查任务计划程序里的 ABB-Bridge）"
    } else {
        ""
    };
    crate::agent::truncate(&format!("{action}：{err}{hint}"), 200)
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
                _ => tray.get_icon_offline(),         // 会话过期 / 离线 / 其它
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
    // 全局气泡开关的回显（真值在 config.json；托盘菜单是结构性 if 重建，必须每 tick 回写）。
    tray.set_bubble_enabled(Config::load().map(|c| c.syschat_bubble).unwrap_or(true));

    // 托盘图标整体状态：所有启用 bot 在线=绿；有在连/重连=黄；有离线/会话过期=红；
    // 未跑服务、无 bot 或全部停用=原灰（不显红吓人）。
    let active: Vec<&crate::botstatus::BotStatus> =
        bots.iter().filter(|b| b.conn != "已停用").collect();
    let status = if !st.running || active.is_empty() {
        "none"
    } else if active
        .iter()
        .any(|b| b.conn == "连接中" || b.conn == "重连中")
    {
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
        format!("ABB 设置 v{}", env!("CARGO_PKG_VERSION"))
    } else {
        format!(
            "ABB 设置 v{} — {} 个 bot · {} 在线",
            env!("CARGO_PKG_VERSION"),
            bots.len(),
            online
        )
    };
    settings.set_window_title(title.into());
    // hero 大按钮 / 状态卡用的结构化字段（running-line 保留给底部状态行）
    settings.set_service_running(st.running);
    settings.set_autostart(platform::autostart_enabled());
    settings.set_bot_count(bots.len() as i32);
    settings.set_online_count(online as i32);
    // 硬闸配套引导：全局默认供应商缺失/悬空 → 首页待处理卡提示（点击跳供应商页）。
    // load 失败按 true（保守提示）；bot 级 provider 覆盖属进阶语义，卡片按全局判定。
    let provider_unconfigured = crate::config::Config::load()
        .map(|c| {
            c.default_provider.is_empty()
                || !c.providers.iter().any(|p| p.name == c.default_provider)
        })
        .unwrap_or(true);
    settings.set_provider_unconfigured(provider_unconfigured);
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

/// 打开窗口前调用：**激活本进程**，让刚 `show` 的窗口成为 key window（可输入）。
///
/// D5（owner 2026-10-14）：**不再**提升为 Regular —— 那会给整机凭空加一个 Dock 图标。
/// ABB 永远是 accessory（`Info.plist` 的 `LSUIElement=true`），见 [`platform::activate_app`]。
fn bring_app_to_front() {
    platform::activate_app();
}

/// 是否启用**自绘**无边框窗口缩放热区（`PixelWindowResize`）。
///
/// 口径：只有 Windows/Linux 需要自绘（它们没有系统缩放边框）；**macOS 一律关掉**。
///
/// 为什么 macOS 必须关（D6，2026-10-14）：`slint_pixel::install_window_resize` 把热区回调接到
/// `winit::Window::drag_resize_window`，而 winit 0.30.13 的 macOS 实现**直接返回
/// `Err(NotSupported)`**（`src/platform_impl/macos/window_delegate.rs:1179`）——热区于是变成
/// 「按下就被 grab 吃掉、然后什么都不做」的死区。它不是无害的：热区是整窗 overlay（north 条
/// 绝对 y 0..6、左右条 6px 整高、四角 14×14），压在标题栏（y 2..36）与窗口按钮的角上，
/// 「抓住标题栏顶部拖窗口」这类操作会静默失效。`ui/app.slint` 里设置窗那条注释
/// （“macOS 刻意不加：系统对 no-frame 窗口自带边角缩放，自绘热区会抢掉它”）说的就是这个口径，
/// 对话窗（D 批「裸平台也开」）当时背离了它。
///
/// 守卫：`ui::tests::resize_hit_areas_follow_platform_rule`（结构：两个窗口都不许写死 `true`）+ 无头几何取证
/// `ui_headless_render_test::resize_hot_zone_eats_titlebar_top_band`。
fn resize_hit_areas_enabled() -> bool {
    !cfg!(target_os = "macos")
}

/// 从回调里首次显示一个「创建后一直隐藏」的窗口。顺序很关键（macOS 实测坑）：
/// ① **必须 `request_redraw`**：Slint winit 后端在 `set_visible` 里对「首次 show」会预渲染首帧
///    （`winitwindowadapter.rs` ~1132 行的 `self.draw()`），但 macOS Metal 下该预渲染发生在
///    窗口 **map 之前**、surface 尚未就绪 → 首帧画空。而 macOS 又不像 iOS（同文件 ~1154 行的
///    guard）那样对「晚 show 的窗口」自动补一次 redraw，于是**内容区永久透明、只剩系统标题栏**。
///    显式 `request_redraw` 等于把 iOS 那条 guard 移植到 macOS，让窗口 map 后补画一帧。
/// ② **激活在 show 之前**：与 winit 自己 `focus_window()` 的两步同序
///    （`activateIgnoringOtherApps` → `makeKeyAndOrderFront`，而 winit 的 `set_visible(true)`
///    就是 `makeKeyAndOrderFront`）。D5 之前这里还有一条「先激活」的理由（策略切 Regular 会
///    重排窗口、把已可见窗口刚画好的内容闪一下）—— 不再切策略后那条已不存在，顺序照旧。
/// 综上顺序：`bring_app_to_front` → `show` → `request_redraw`。`QrDialog`/`SettingsWindow` 都走这个。
fn show_window_and_focus<W: slint::ComponentHandle + 'static>(w: &W) {
    // 窗口位置完全交给系统默认（macOS 级联位），代码不做任何移动：
    // 曾做过"show 后 100ms 强行居中"（肉眼可见跳动）和启动预热（钉死 (0,0)），都已拆除。
    // 最小化恢复：窗口被最小化（自绘标题栏黄点/macOS cmd+M）后 show() 不还原，
    // 托盘点击会"没反应"（8-20 实测：必须点 Dock 才恢复）——显式还原最小化状态。
    if w.window().is_minimized() {
        w.window().set_minimized(false);
    }
    bring_app_to_front();
    let _ = w.show();
    w.window().request_redraw();
}

// D 批：气泡窗口 = 36px 可视气泡 + 四周各 4px 透明边距（给悬停放大 1.06 / 按下描边留空间，
// 见 `ui/app.slint` 的 AnchorBubble 注释）。位置记忆存的是**窗口**左上角，几何换算时要内缩。
const BUBBLE_VISUAL_SIZE: i32 = 36;
const BUBBLE_INSET: i32 = 4;

/// 气泡显隐的**唯一收口**。
///
/// 可见 ⟺ `config.syschat_bubble == true`（**只由托盘开关决定**）。
///
/// v10（稿 v8 第 1 条「气泡常驻置顶」）：**气泡不再随对话框显隐而隐藏**——点气泡开窗后气泡仍在，
/// 永久置顶、窗口始终锚在它旁边；老需求 #13（「对话框可见就藏气泡」）**作废**。
/// 旧实现把「对话框当前是否可见」当第二个判据，于是「开窗 ⇒ 藏气泡 ⇒ 拖窗口的锚点没了」，
/// 且关窗必须再把气泡弄回来；现在只剩托盘开关这一路输入。
///
/// 托盘 `on_toggle_bubble`、气泡点击、两种关窗路径（✕ 按钮 / 红点 `CloseRequested`）都只调它，
/// 禁止各写一份——否则显隐口径迟早分叉。
fn refresh_bubble_visibility(bubble: &AnchorBubble) {
    let enabled = Config::load().map(|c| c.syschat_bubble).unwrap_or(true);
    if bubble_visible(enabled) {
        let _ = bubble.show();
    } else {
        let _ = bubble.hide();
    }
}

/// 隐藏系统对话窗的**唯一收口**（✕ 按钮 / 红点 `CloseRequested` / D5 失焦隐藏 三条路径共用）。
///
/// - 气泡不动（v10「常驻置顶」口径）：只调一次 [`refresh_bubble_visibility`] 把显隐拉回 config 真值；
/// - `hide_dock`：D5 之后进程永不为 Regular（见 [`bring_app_to_front`]），这句只是把
///   「ABB 恒 accessory」这条不变式在「窗口都关了」的收尾点重新钉一遍（非 macOS 是空实现）。
fn hide_syschat(
    win: &SystemChatWindow,
    bubble: &slint::Weak<AnchorBubble>,
    settings: &slint::Weak<SettingsWindow>,
) {
    let _ = win.hide();
    if let Some(b) = bubble.upgrade() {
        refresh_bubble_visibility(&b);
    }
    let settings_open = settings
        .upgrade()
        .map(|s| s.window().is_visible())
        .unwrap_or(false);
    if !settings_open {
        platform::hide_dock();
    }
}

/// D5 失焦隐藏的**判据**（纯函数，口径钉在单测里）。
///
/// `visible` = 对话窗当前是否可见；`app_active` = **进程**当前是否仍处于激活态
/// （[`platform::app_is_active`]，macOS 上是 `[NSApp isActive]`）。
///
/// 只有「窗口可见 **且** 焦点离开了**整个进程**」才隐藏：
/// - 同一进程里气泡 / 设置窗 / 原生文件面板抢走 key 时 `app_active` 仍为 true ⇒ **不隐藏** ——
///   否则「点/拖气泡时对话框跟着动」（D 批 #9）会在鼠标按下那一刻先把窗口藏掉，
///   「📎 选文件」也会把窗口藏在面板背后（选完的文件就落进一个看不见的窗口）；
/// - 窗口本来就藏着（刚被 ✕ 关掉，又收到一次 `Focused(false)`）⇒ 不重复处理。
///
/// 判别力（负向）：把判据改成 `visible`（只看窗口焦点）⇒ `blur_hide_ignores_focus_that_stays_in_process`
/// 必红；改成 `!app_active`（不看可见性）⇒ `blur_hide_skips_hidden_window` 必红。
fn should_hide_on_blur(visible: bool, app_active: bool) -> bool {
    visible && !app_active
}

/// 失焦隐藏的**判定延迟**（ms）。
///
/// 不是节流/动画，而是**事件顺序**：winit 的 `WindowEvent::Focused(false)` 来自
/// `windowDidResignKey:`，而 `[NSApp isActive]` 要到 `applicationDidResignActive:` 才转 false
/// —— 在 blur 回调里**同步**查会永远为真（隐藏永远不触发）。延后一拍后跑在后续的 runloop
/// 迭代上，那两个 AppKit 回调都已发完。
/// 120ms：远低于「点别处 ⇒ 窗口消失」的感知阈值，又远大于一次 runloop 周转。
const BLUR_HIDE_DELAY_MS: u64 = 120;

/// D5：处理对话窗的 `Focused` 事件（失焦 ⇒ 隐藏）。返回 `true` 表示该事件已由本函数消费。
///
/// 为什么单独成函数（而不是就地写在事件钩子里）：
/// 1. 它必须挂在**已有的那个** winit 事件钩子上（`Window::on_winit_window_event` 是单槽 `set`，
///    见 `ui::tests::syschat_has_exactly_one_winit_event_hook`）——钩子体里只留一行转发；
/// 2. 「事件 → 延时 → 判定 → 隐藏」整条链要能被无头测试直接驱动
///    （`ui_headless_render_test::blur_hides_dialog_window_end_to_end`），不必跑 `run_gui`
///    （跑 `run_gui` 会触发自启自愈、改用户真实的 `~/Library/LaunchAgents/`）。
///
/// `timer` 由调用方持有并保活（drop 即取消）；单槽：`start` 会覆盖上一次待执行的隐藏。
fn handle_syschat_focus_event(
    ev: &slint::winit_030::winit::event::WindowEvent,
    timer: &slint::Timer,
    win: &slint::Weak<SystemChatWindow>,
    bubble: &slint::Weak<AnchorBubble>,
    settings: &slint::Weak<SettingsWindow>,
) -> bool {
    let slint::winit_030::winit::event::WindowEvent::Focused(focused) = ev else {
        return false;
    };
    if *focused {
        // 焦点又回来了（例如点了窗口内部 / 刚 show 完）：取消待执行的隐藏。
        timer.stop();
        return true;
    }
    let Some(w) = win.upgrade() else { return true };
    if !w.window().is_visible() {
        return true; // 本来就藏着（✕ 关掉之后又收到一次失焦）：不用排隐藏
    }
    let (cwp, bwp, swp) = (win.clone(), bubble.clone(), settings.clone());
    timer.start(
        slint::TimerMode::SingleShot,
        Duration::from_millis(BLUR_HIDE_DELAY_MS),
        move || {
            let Some(w) = cwp.upgrade() else { return };
            if should_hide_on_blur(w.window().is_visible(), platform::app_is_active()) {
                hide_syschat(&w, &bwp, &swp);
            }
        },
    );
    true
}

/// 气泡显隐的**唯一判据**（纯函数，口径钉在单测里）。
///
/// 签名里**故意没有**「对话框可见性」这个入参：v10（稿 v8 第 1 条）把「对话框可见即不显示气泡」
/// 作废了——气泡常驻置顶。把 dialog 参数加回来就等于把作废的口径请回来，
/// 守卫见 `ui::tests::bubble_visibility_has_no_dialog_input`。
fn bubble_visible(enabled: bool) -> bool {
    enabled
}

/// 系统对话窗的**当前逻辑宽度**（标题栏计划要用）；窗口还没 realize（`size()` 为 0）时返回 0，
/// 调用方回落到 `syschat::SYSCHAT_PREFERRED_WIDTH`。
///
/// `Window::size()` 是物理像素，必须除 scale factor——Retina 2x 下不换算会让计划按两倍窗宽算，
/// 控件条直接摆到窗外（同 [`anchored_dialog_pos`] 里踩过的那个坑）。
fn syschat_logical_width(win: &SystemChatWindow) -> f32 {
    let sf = win.window().scale_factor();
    if sf <= 0.0 {
        return 0.0;
    }
    win.window().size().width as f32 / sf
}

/// D2（稿 ①②③）：把标题栏控件条的**落位计划**与消息区正文宽一起写进窗口（两者都是
/// 「按窗宽算好、由 Rust 写成普通值」的东西，合并在一个入口避免两处分别跟窗宽变化）。
///
/// 算术全在 [`crate::syschat::titlebar_plan`]（纯函数 + 单测），这里只做两件接线活：
/// 1. 把计划里的长度写进 `titlebar-*-w` / `titlebar-strip-x`（0 / 负值 ⇒ `.slint` 不画该项）；
/// 2. 把 chip 文案按各自宽度截断（`PixelMenu` 内部 Text 不会 elide，长标题会画出框外）；
/// 3. 把消息区正文可用宽写进 `convo-text-width`（见 `.slint` 里那条属性的注释：
///    文本必须带 px 宽绑定，但又不能在 `.slint` 里绑 `root.width`——那是 Slint 的 binding loop）。
///
/// `window_w <= 0`（窗口未 realize）时回落到 `preferred-width`：否则首帧会按 0 宽算出「整条不画」，
/// 窗口真宽到位后即使能重算，也会白闪一下。
fn apply_syschat_layout(win: &SystemChatWindow, window_w: f32) {
    use slint::Model as _;
    let w = if window_w > 0.0 {
        window_w
    } else {
        crate::syschat::SYSCHAT_PREFERRED_WIDTH
    };
    // 消息区正文宽：窗宽 − 根 padding 4 − col padding 24 − 块内 padding 16（
    // `crate::syschat::CONVO_BLOCK_TEXT_INSET` 一处说了算，改布局时只动那一个常量）。
    win.set_convo_text_width(
        (w - crate::syschat::CONVO_BLOCK_TEXT_INSET).max(crate::syschat::CONVO_TEXT_WIDTH_MIN),
    );
    // 没配「设置 → 供应商」时模型下拉无可选项 ⇒ 该项不占位（D2 前是 `if model-labels.length > 0`）。
    let has_model = win.get_model_labels().row_count() > 0;
    let plan = crate::syschat::titlebar_plan(w, has_model);
    win.set_titlebar_strip_x(plan.x);
    win.set_titlebar_session_w(plan.session_w);
    win.set_titlebar_model_w(plan.model_w);
    win.set_titlebar_settings_w(plan.settings_w);
    let chip_text = |labels: slint::ModelRc<slint::SharedString>, idx: i32, chip_w: f32| {
        if chip_w <= 0.0 {
            return slint::SharedString::default();
        }
        labels
            .row_data(idx.max(0) as usize)
            .map(|s| crate::syschat::fit_chip_label(&s, chip_w).into())
            .unwrap_or_default()
    };
    // D2 回退位：标题栏放不下的项会落到状态行（`.slint` 里的 `titlebar-*-w <= 0` 分支），
    // 那里 chip 用的是**固定宽**（会话 84、模型 112）——所以文案也得按那个宽度截，
    // 不能因为「标题栏没要」就把文案置空（否则回落位里是个空菜单壳）。
    let row_session_w = crate::syschat::TB_SESSION_FALLBACK_W;
    let row_model_w = if plan.model_w > 0.0 {
        plan.model_w
    } else {
        crate::syschat::TB_MODEL_W
    };
    win.set_titlebar_title(
        if crate::syschat::titlebar_title_visible(&plan) {
            "ABB · 系统对话"
        } else {
            ""
        }
        .into(),
    );
    win.set_titlebar_session_text(chip_text(
        win.get_session_labels(),
        win.get_session_index(),
        row_session_w,
    ));
    win.set_titlebar_model_text(chip_text(
        win.get_model_labels(),
        win.get_model_index(),
        row_model_w,
    ));
    // v11（卡 `abb-v11-panels-align`）：两个面板的锚点（右缘跟钮右缘对齐、顶边在钮下缘 6px、
    // 靠窗边整体内移）。算术在 `syschat::menu_anchor`（纯函数带单测）；这里只把结果写进窗口。
    // **未画的项**（换档换掉了）不写——面板也只能从 banner 里点开，本就不会用到。
    let btn_bottom = (crate::syschat::TB_BTN_Y + crate::syschat::TB_BTN_H) as i32;
    let panel_w = crate::syschat::MENU_PANEL_W as i32;
    if let Some((bx, bw)) = crate::syschat::titlebar_session_btn(&plan) {
        let (mx, my) =
            crate::syschat::menu_anchor(bx as i32, bw as i32, btn_bottom, panel_w, w as i32);
        win.set_session_menu_x(mx as f32);
        win.set_session_menu_y(my as f32);
    }
    if let Some((bx, bw)) = crate::syschat::titlebar_model_btn(&plan) {
        let (mx, my) =
            crate::syschat::menu_anchor(bx as i32, bw as i32, btn_bottom, panel_w, w as i32);
        win.set_model_menu_x(mx as f32);
        win.set_model_menu_y(my as f32);
    }
}

/// v11（卡 `abb-v11-panels-align`）：状态栏右侧的上下文用量。
///
/// 数据源接缝在 [`crate::syschat::context_usage`]（**E3 卡**）：当前如实为空 ⇒
/// 文案 `上下文 —/— · —`、进度条 0 宽；百分比/档位/文案格式化全在纯函数里（带单测），
/// E3 落地时只改那一个函数（本函数是**唯一**消费点）。
fn refresh_syschat_usage(win: &SystemChatWindow) {
    let (used, limit) = crate::syschat::context_usage();
    win.set_usage_text(crate::syschat::context_usage_text(used, limit).into());
    win.set_usage_tip(
        "本会话上下文占用（数据来源待 E3 卡：上游不上报 usage，不显示估算值）".into(),
    );
    let (fill, level) = match (used, limit) {
        (Some(u), Some(l)) if l > 0 => {
            let pct = crate::syschat::usage_percent(u, l);
            (
                crate::syschat::USAGE_METER_INNER_W * pct / 100.0,
                crate::syschat::usage_level(pct),
            )
        }
        _ => (0.0, 0),
    };
    win.set_usage_meter_fill(fill);
    win.set_usage_level(level);
}

/// D4：读某窗口的**逻辑**外框 `(x, y, w, h)`。
///
/// `outer_position()` / `inner_size()` 都是**物理**像素，锚定与吸附的数学全用逻辑像素
/// （Retina 2x 下不换算差一倍 —— 同 [`anchored_dialog_pos`] 里踩过的坑）。拿不到 winit 窗口
/// （未 realize / 无头平台）返回 None，调用方据此跳过本次跟随。
fn logical_window_rect(win: &slint::Window) -> Option<(i32, i32, i32, i32)> {
    use slint::winit_030::WinitWindowAccessor;
    let sf = win.scale_factor();
    let sf = if sf > 0.0 { sf } else { 1.0 };
    win.with_winit_window(|w| {
        let pos = w.outer_position().ok()?;
        let size = w.inner_size();
        Some((
            (pos.x as f32 / sf).round() as i32,
            (pos.y as f32 / sf).round() as i32,
            (size.width as f32 / sf).round() as i32,
            (size.height as f32 / sf).round() as i32,
        ))
    })
    .flatten()
}

/// D4：某窗口所在显示器的**完整边界**（逻辑像素）当工作区。
///
/// Slint/winit 都没有「去掉菜单栏/Dock 的工作区」API，与 [`anchored_dialog_pos`] 同口径
/// （夹取保证不出屏；菜单栏/Dock 遮挡属平台行为，本批不做）。拿不到显示器返回 None。
fn monitor_work_area(win: &slint::Window) -> Option<(i32, i32, i32, i32)> {
    use slint::winit_030::WinitWindowAccessor;
    win.with_winit_window(|w| {
        w.current_monitor().map(|m| {
            let p = m.position().to_logical::<f32>(m.scale_factor());
            let s = m.size().to_logical::<f32>(m.scale_factor());
            (p.x as i32, p.y as i32, s.width as i32, s.height as i32)
        })
    })
    .flatten()
}

/// 计算对话框应处的左上角（逻辑像素）：把窗口锚在气泡**可视区**旁，靠屏边自动翻边（口径见
/// [`crate::syschat::anchor_window_pos`]）。
///
/// 工作区口径（**替代说明，已写进卡内**）：Slint/winit 都没有「工作区（去掉菜单栏/Dock）」API，
/// 故取气泡所在**显示器的完整边界**（物理坐标按 scale 换逻辑）作为工作区。夹取能保证不出屏；
/// 菜单栏/Dock 区域是否遮挡属平台行为，本批不做。
fn anchored_dialog_pos(
    bubble_win: &slint::Window,
    dialog_win: &slint::Window,
) -> Option<(i32, i32)> {
    use slint::winit_030::WinitWindowAccessor;
    let win_pos = bubble_win.with_winit_window(|w| w.outer_position().ok())??;
    let b =
        crate::syschat::BubblePos::from_physical(win_pos.x, win_pos.y, bubble_win.scale_factor());
    // 可视气泡 = 气泡窗口左上角 + 内缩（窗口比图标大）。
    let bubble = (b.x + BUBBLE_INSET, b.y + BUBBLE_INSET);
    // `Window::size()` 是**物理**像素，锚定数学全用逻辑像素（Retina 2x 下不换算差一倍）。
    let dsz = dialog_win.size().to_logical(dialog_win.scale_factor());
    let work = monitor_work_area(bubble_win)?;
    Some(crate::syschat::anchor_window_pos(
        bubble,
        (BUBBLE_VISUAL_SIZE, BUBBLE_VISUAL_SIZE),
        (dsz.width as i32, dsz.height as i32),
        work,
    ))
}

/// 逻辑像素矩形 `(x, y, w, h)`：跟随相关的几何统一用它（避免裸四元组到处飞）。
type Rect = (i32, i32, i32, i32);

/// 气泡跟随的**几何落点**（纯函数，单测钉住）：对话窗矩形 + 气泡**窗口**矩形 + 工作区
/// ⇒ 气泡窗口目标左上角（逻辑像素）。`None` = 已就位（不调 `set_position`，连多余的 `Moved`
/// 都不产生）。
///
/// 为什么单独抽出来：A 卡（`abb-v11-bubble-follow-lag`）之后有**两条**落位通道（事件路径与
/// 拖动期间的 common-modes 定时器），几何只允许有一份——两条通道都调本函数，
/// 四角择优与夹取仍在 [`crate::syschat::snap_bubble_to_window`]。
fn bubble_follow_target(dialog_rect: Rect, bubble_rect: Rect, work: Rect) -> Option<(i32, i32)> {
    // 气泡几何用**可视区**（窗口比图标大 BUBBLE_INSET，见常量注释），换回窗口位要减掉。
    let visual = (bubble_rect.0 + BUBBLE_INSET, bubble_rect.1 + BUBBLE_INSET);
    let (_, (tx, ty)) = crate::syschat::snap_bubble_to_window(
        dialog_rect,
        visual,
        (BUBBLE_VISUAL_SIZE, BUBBLE_VISUAL_SIZE),
        work,
    );
    let target = (tx - BUBBLE_INSET, ty - BUBBLE_INSET);
    if target == (bubble_rect.0, bubble_rect.1) {
        None
    } else {
        Some(target)
    }
}

/// D4：气泡跟随的**唯一落位入口**（开窗 / 对话窗 `Moved`·`Resized` / 拖气泡后的去抖存盘共用：
/// 同一套几何不写第二份）。
///
/// **单一事实源（卡 `abb-syschat-d4-bubble-follow`）= 对话窗矩形**：连续约束只有
/// 「对话窗 ⇒ 气泡」这一条方向；反向的 [`anchored_dialog_pos`]（气泡 ⇒ 窗口）**只在开窗那一刻**
/// 用一次，之后不再参与。所以本函数移动气泡**不会**让对话窗再动（对话窗只由用户/系统移动），
/// 也就不存在 `A 动→B 动→A 动` 的反馈环。几何本身（四角择优 + 夹取）在
/// [`crate::syschat::snap_bubble_to_window`]，并**幂等**（再算一次结果不变），两道合起来就是防环措施。
///
/// 返回是否真的移动了（供排障；生产不依赖返回值）。
fn snap_bubble_to_dialog(bubble: &AnchorBubble, dialog: &SystemChatWindow) -> bool {
    if !dialog.window().is_visible() {
        return false; // 窗没开时气泡自由摆放，不吸附
    }
    let Some(win_rect) = logical_window_rect(&dialog.window()) else {
        return false; // 对话窗还没 realize：没有可依附的矩形
    };
    let Some(bubble_rect) = logical_window_rect(&bubble.window()) else {
        return false;
    };
    // 工作区取自**对话窗所在显示器**（窗口是事实源，气泡跟着它跨屏）；拿不到再退回气泡所在显示器。
    let Some(work) =
        monitor_work_area(&dialog.window()).or_else(|| monitor_work_area(&bubble.window()))
    else {
        return false;
    };
    let Some(target) = bubble_follow_target(win_rect, bubble_rect, work) else {
        return false; // 已就位
    };
    bubble.window().set_position(slint::LogicalPosition::new(
        target.0 as f32,
        target.1 as f32,
    ));
    true
}

// ─────────── A（卡 `abb-v11-bubble-follow-lag`）：跟手跟随 ───────────
//
// ## 根因（实核，不靠感觉）
// owner：「气泡跟随窗口移动不够跟手」。实测链路：
// 1. `ui/app.slint` 的 `PixelTitleBar { native-move: true }` ⇒ Slint 1.18 `WindowMoveArea`
//    ⇒ `WindowAdapterInternal::start_window_move` ⇒ `winit_window.drag_window()`
//    ⇒ macOS `-[NSWindow performWindowDragWithEvent:]`：在
//    **`NSEventTrackingRunLoopMode`** 的嵌套 run loop 里一直跑到松手。
// 2. `start_window_move` 是 `if let Some(w) = self.winit_window_or_none.borrow().as_window()`
//    ——edition 2021 下 `if let` 的临时 `Ref` 活到整个 if-let 结束 ⇒ **拖动全程**
//    Slint 侧的窗口 RefCell 都是「已借出」。
// 3. 这期间 AppKit 照发 `windowDidMove:` → winit `emit_move_event()` → `queue_event()`
//    → `maybe_queue_event()`：此时 `event_handler.in_use()` 为真（我们正卡在它里面），
//    于是改走 `run_loop.queue_closure(...)`，而 `queue_closure` 明确只用
//    **`kCFRunLoopDefaultMode`**（winit 自带注释：「Any events that are queued while running
//    a modal or when live-resizing will instead wait, and be delivered to the application
//    afterwards.」）。
// 4. 同一原因，winit 的 `wakeup()` / `cleared()` 在事件处理器占用期间直接 return ⇒
//    `Slint::Timer` 也**不触发**。
//
// 结论：**纯事件方案不可能跟手** —— 拖动期间应用层收不到任何 `Moved`/`Resized`（松手后才
// 一次性补齐）。所以换机制：往主 run loop 的 **common modes**（含 event-tracking mode）挂一个
// 16ms 定时器（[`crate::platform::common_mode_timer`]），自己每拍读一次对话窗矩形。
//
// ## 为什么用 `Arc<winit::Window>` 而不是 Slint 的 `window()`
// 见上文第 2 点：拖动期间经 Slint 的窗口访问会 `RefCell` 重入 panic。winit 的 `Window`
// （`WinitWindowAccessor::winit_window()` 异步取得，只在句柄就绪时用）读写都不碰那个 RefCell。
//
// ## 不破 D4 防环
// 仍然只有「对话窗矩形 ⇒ 气泡位置」一个方向：[`bubble_follow_target`] 只吃对话窗矩形，
// 泵也**从不**根据气泡位置反推对话窗（反向的 `anchored_dialog_pos` 仍只在开窗用一次）。
// 另外泵只在「对话窗矩形变了」时才动作 ⇒ 用户拖气泡（对话窗不动）时泵不动手，
// 不会与原生拖动抢窗口位置。

/// 跟随泵的**动作判据**（纯函数，单测钉住）：只有「对话窗可见」且「对话窗矩形与上次不同」
/// 才动气泡。
///
/// 第二条同时是**防抢位置**的关键：用户拖气泡时对话窗不动 ⇒ 泵不动作。
fn follow_should_snap(prev_dialog: Option<Rect>, cur_dialog: Rect, visible: bool) -> bool {
    visible && prev_dialog != Some(cur_dialog)
}

/// 工作区缓存判据（纯函数）：缓存工作区**包含对话窗中心点** ⇒ 沿用缓存（没跨显示器）。
///
/// 显示器互不重叠，所以中心点一跨屏就会落到缓存工作区之外 ⇒ 自动重算；
/// 缓存命中时省掉 `current_monitor()`（跨 FFI + 显示器枚举，是原实现里最重的一步）。
fn work_area_cache_hit(cached: Rect, win: Rect) -> bool {
    let (cx, cy) = (win.0 + win.2 / 2, win.1 + win.3 / 2);
    cx >= cached.0 && cx < cached.0 + cached.2 && cy >= cached.1 && cy < cached.1 + cached.3
}

/// A：跟随泵的共享状态（事件路径与定时器路径共用）。
#[derive(Default)]
struct FollowState {
    /// 上一次 tick 看到的对话窗矩形（逻辑像素）——[`follow_should_snap`] 的基线。
    last_dialog: Option<Rect>,
    /// 工作区缓存：`(工作区, 命中它的对话窗矩形)`。
    work: Option<(Rect, Rect)>,
}

impl FollowState {
    /// 取工作区：缓存仍命中就沿用，否则用 `resolve` 重算并刷新缓存。
    fn work_area(
        &mut self,
        dialog_rect: Rect,
        resolve: impl FnOnce() -> Option<Rect>,
    ) -> Option<Rect> {
        if let Some((cached, _)) = self.work {
            if work_area_cache_hit(cached, dialog_rect) {
                return Some(cached);
            }
        }
        let w = resolve()?;
        self.work = Some((w, dialog_rect));
        Some(w)
    }
}

/// macOS：跟随泵的 winit 句柄（拖动期间唯一安全的读写通道，见本节根因）。
#[cfg(target_os = "macos")]
mod follow_macos {
    use std::sync::Arc;

    type WinitWindow = slint::winit_030::winit::window::Window;
    type Rect = super::Rect;

    /// 一个 winit 窗口的逻辑外框——同 [`logical_window_rect`] 的坐标口径，
    /// 但**不经过 Slint 的 window adapter**。
    pub(super) fn rect(w: &WinitWindow) -> Option<Rect> {
        let sf = w.scale_factor();
        let sf = if sf > 0.0 { sf } else { 1.0 };
        let pos = w.outer_position().ok()?;
        let size = w.inner_size();
        Some((
            (pos.x as f64 / sf).round() as i32,
            (pos.y as f64 / sf).round() as i32,
            (size.width as f64 / sf).round() as i32,
            (size.height as f64 / sf).round() as i32,
        ))
    }

    /// 某 winit 窗口所在显示器的完整边界（逻辑像素）——同 [`monitor_work_area`] 的口径。
    pub(super) fn work_area(w: &WinitWindow) -> Option<Rect> {
        w.current_monitor().map(|m| {
            let p = m.position().to_logical::<f32>(m.scale_factor());
            let s = m.size().to_logical::<f32>(m.scale_factor());
            (p.x as i32, p.y as i32, s.width as i32, s.height as i32)
        })
    }

    pub(super) struct Handles {
        pub(super) dialog: Arc<WinitWindow>,
        pub(super) bubble: Arc<WinitWindow>,
    }
}

/// A：气泡跟随泵。macOS 上句柄就绪后由 common-modes 定时器驱动；其它平台只走事件路径。
#[derive(Default)]
struct FollowPump {
    state: RefCell<FollowState>,
    #[cfg(target_os = "macos")]
    handles: RefCell<Option<follow_macos::Handles>>,
}

impl FollowPump {
    /// 定时器路径（仅 macOS）：**绝不碰 Slint 的 window adapter**（拖动期间它被借出）。
    #[cfg(target_os = "macos")]
    fn tick_handles(&self) -> bool {
        let handles = self.handles.borrow();
        let Some(h) = handles.as_ref() else {
            return false;
        };
        // 隐藏时连矩形都不读（托盘常驻，泵 60Hz 跑一整机寿命；这一步省掉 2 次 FFI）。
        if h.dialog.is_visible() != Some(true) {
            return false;
        }
        let Some(dialog_rect) = follow_macos::rect(&h.dialog) else {
            return false;
        };
        let mut st = self.state.borrow_mut();
        if !follow_should_snap(st.last_dialog, dialog_rect, true) {
            return false;
        }
        // 基线先推进：气泡/工作区这一拍拿不到时不要每 16ms 重试。
        st.last_dialog = Some(dialog_rect);
        let Some(bubble_rect) = follow_macos::rect(&h.bubble) else {
            return false;
        };
        let Some(work) = st.work_area(dialog_rect, || {
            follow_macos::work_area(&h.dialog).or_else(|| follow_macos::work_area(&h.bubble))
        }) else {
            return false;
        };
        let Some(target) = bubble_follow_target(dialog_rect, bubble_rect, work) else {
            return false;
        };
        h.bubble
            .set_outer_position(slint::winit_030::winit::dpi::LogicalPosition::new(
                target.0 as f32,
                target.1 as f32,
            ));
        true
    }

    /// 事件路径（对话窗 `Moved`/`Resized`）：句柄就绪走句柄版本（带缓存/去重），
    /// 未就绪（非 macOS，或 macOS 上 `spawn_local` 还没跑完）退回 Slint 版本。
    ///
    /// 为什么退回 Slint 版本在 macOS 上安全：拖动期间 macOS **不会**把 `Moved` 交到应用层
    /// （本节根因第 3 点），所以这个退路不可能在「RefCell 已被借出」的时机被走到。
    fn tick(
        &self,
        bubble: &slint::Weak<AnchorBubble>,
        dialog: &slint::Weak<SystemChatWindow>,
    ) -> bool {
        #[cfg(target_os = "macos")]
        if self.handles.borrow().is_some() {
            return self.tick_handles();
        }
        let (Some(b), Some(d)) = (bubble.upgrade(), dialog.upgrade()) else {
            return false;
        };
        snap_bubble_to_dialog(&b, &d)
    }
}

/// A：泵的 16ms 节拍（稿/卡口径：16ms 合并，约一帧）。
const FOLLOW_TICK_MS: u64 = 16;

/// D4/A：对话窗 `Moved` / `Resized` → 气泡贴到最近的角（跟随窗口移动/缩放/最大化）。
///
/// 这是「跟随」的**事件源**（另一个源是 macOS 上拖动期间的定时器，见 [`FollowPump`]）：
/// 只有对话窗自己的几何事件会触发吸附 —— 气泡自己的 `Moved` 绝不反向驱动对话窗
/// （那条在 D4 已拆除，见 [`snap_bubble_to_dialog`] 的防环说明）。
/// 返回 true 表示该事件属于几何跟随面。
fn handle_syschat_geometry_event(
    ev: &slint::winit_030::winit::event::WindowEvent,
    pump: &FollowPump,
    bubble: &slint::Weak<AnchorBubble>,
    dialog: &slint::Weak<SystemChatWindow>,
) -> bool {
    use slint::winit_030::winit::event::WindowEvent;
    if !matches!(ev, WindowEvent::Moved(_) | WindowEvent::Resized(_)) {
        return false;
    }
    pump.tick(bubble, dialog);
    true
}

/// 设置窗工作副本聚合（#80 减重）：除 bots 外的全部 work 副本装进一个 Rc，
/// load/snapshot 签名不再随字段增长；新增设置字段只改这里 + 声明处。
/// bots 副本（`work`）单独保留——它在多个 handler 里被复用为其它字段的别名
/// （如 `let work = notify_work.clone()`），统一进结构体会引入命名混乱。
struct SettingsWork {
    providers: Rc<RefCell<Vec<ProviderConfig>>>,
    /// 默认供应商名（#21 跨会话投递依赖）。
    default_provider: Rc<RefCell<String>>,
    /// 跨会话投递总开关（#21）。
    cross_delivery: Rc<RefCell<bool>>,
    /// 虚拟 Bot 自定义角色模板（#75）：弹窗里管理，随「保存」写盘。
    templates: Rc<RefCell<Vec<RoleTemplate>>>,
    /// #74 历史记录页：保留期 / 提醒开关。
    history_retention: Rc<RefCell<u32>>,
    notify: Rc<RefCell<bool>>,
    /// #78 会话归纳清理：全局开关（默认关）+ 过期天数（默认 7）。
    session_gc: Rc<RefCell<bool>>,
    session_gc_days: Rc<RefCell<u32>>,
    /// Jev 决策模型（授权者/受限会话门禁）。
    jev: Rc<RefCell<crate::config::JevConfig>>,
}

/// 把设置窗工作副本汇总成待写盘的 Config（「保存」与「草稿自动保存」共用同一份逻辑，
/// 保证两种路径行为一致：bots 保留运行期字段、供应商密钥留空沿用旧值、默认供应商防悬空）。
/// 返回 (Config, 丢弃的未命名供应商数)。
fn snapshot_config(work: &RefCell<Vec<BotConfig>>, wk: &SettingsWork) -> (Config, usize) {
    let mut c = Config::load().unwrap_or_default();
    // 用工作副本整体替换 bots（保留每个 bot 运行期的 primary_chat_id）
    let old = std::mem::take(&mut c.bots);
    let mut newb = work.borrow().clone();
    for nb in newb.iter_mut() {
        if let Some(ob) = old.iter().find(|o| o.key() == nb.key()) {
            nb.primary_chat_id = ob.primary_chat_id.clone();
            // mention_modes 由运行期 /mention 命令直接写 config（不经 GUI work），
            // work 可能是打开 GUI 时的旧快照、不含之后新增的免@开关——从最新 config 补回，
            // 避免 GUI 保存其它字段时用旧 work 覆盖清空 mention_modes（重启后免@开关丢失）。
            nb.mention_modes = ob.mention_modes.clone();
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
    c.cross_delivery_enabled = *wk.cross_delivery.borrow();
    // #74 历史记录页：保留期 / 提醒开关（全局生效）
    c.history_retention_days = *wk.history_retention.borrow();
    c.notify_enabled = *wk.notify.borrow();
    // #78 会话归纳清理：全局开关 + 过期天数
    c.session_gc_enabled = *wk.session_gc.borrow();
    c.session_gc_days = *wk.session_gc_days.borrow();

    // 供应商：用工作副本替换，但 api_key 留空=保留旧值（密码框不回显，编辑其它字段不该清密钥）。
    // 丢弃空 name 行（无效），并计数。
    let old_providers = std::mem::take(&mut c.providers);
    let mut dropped = 0;
    let mut newp: Vec<ProviderConfig> = Vec::new();
    for mut p in wk.providers.borrow().clone().into_iter() {
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
    let d = wk.default_provider.borrow().clone();
    // 默认供应商名若已不在列表里（被删/改名）→ 清空，避免悬空引用
    c.default_provider = if !d.is_empty() && c.providers.iter().any(|p| p.name == d) {
        d
    } else {
        String::new()
    };
    // 虚拟 Bot 自定义角色模板（#75）：弹窗里管理的工作副本，随保存写盘
    c.custom_roles = wk.templates.borrow().clone();
    // Jev 决策模型：api_key 留空 = 保留旧值（密码框不回显，编辑其它字段不该清密钥）。
    let old_jev = std::mem::take(&mut c.jev);
    let mut new_jev = wk.jev.borrow().clone();
    if new_jev.api_key.is_empty() {
        new_jev.api_key = old_jev.api_key.clone();
    }
    c.jev = new_jev;
    // #174：同名 bot 自动分配唯一 key suffix 并**落盘固化**（load 只做内存分配）
    c.assign_unique_keys();
    (c, dropped)
}

/// 虚拟 Bot 登记行（#75）：按 bot 读登记表 → slint 行。
fn vb_rows_for(bot: &BotConfig) -> Vec<VirtualBotRow> {
    VirtualBotStore::new()
        .load_for(&bot.key())
        .into_iter()
        .map(|v| VirtualBotRow {
            role_name: v.role_name.clone().into(),
            platform: kind_label(&bot.kind).into(),
            chat_id: v.chat_id.clone().into(),
            created_at: format_created(v.created_at).into(),
            // #91：虚拟 Bot 免 @ 状态 = mention_modes[chat_id] == "off"（事实源单一）
            mention_off: bot.mention_modes.get(&v.chat_id).map(String::as_str) == Some("off"),
        })
        .collect()
}

/// 刷新设置窗「虚拟 Bot」登记列表（选中 bot 维度；Rust 侧各操作完成后调用）。
fn refresh_vb_rows(w: &SettingsWindow, work: &RefCell<Vec<BotConfig>>) {
    let sel = w.get_selected();
    if sel < 0 {
        return;
    }
    if let Some(bot) = work.borrow().get(sel as usize) {
        w.set_virtual_bots(slint::ModelRc::from(Rc::new(slint::VecModel::from(
            vb_rows_for(bot),
        ))));
    }
}

/// #147 团队列表行：按 bot 读团队登记表（最近创建在前）→ slint 行。
/// 状态：全角色有群 = 运行中（ok 色）；有角色缺群 = 部分失败（warn 色）。
fn team_rows_for(bot: &BotConfig) -> Vec<TeamRow> {
    let mut teams = crate::teamreg::TeamStore::new().load_for(&bot.key());
    teams.sort_by_key(|t| std::cmp::Reverse(t.created_at));
    teams
        .into_iter()
        .map(|t| {
            let running = t.running();
            TeamRow {
                team_name: t.team_name.clone().into(),
                role_count: t.roles.len() as i32,
                status: (if running { "运行中" } else { "部分失败" }).into(),
                status_ok: running,
                created_at: format_created(t.created_at).into(),
                roles: slint::ModelRc::from(Rc::new(slint::VecModel::from(
                    t.roles
                        .into_iter()
                        .map(|r| TeamRoleRow {
                            role_name: r.role_name.into(),
                            member: r.member.into(),
                            duty: r.duty.into(),
                        })
                        .collect::<Vec<_>>(),
                ))),
            }
        })
        .collect()
}

/// 刷新设置窗「团队」列表（#147；选中 bot 维度，热读 teams.json——GUI 与聊天入口
/// 创建结果双向一致，不双写）。
fn refresh_team_rows(w: &SettingsWindow, work: &RefCell<Vec<BotConfig>>) {
    let sel = w.get_selected();
    if sel < 0 {
        return;
    }
    if let Some(bot) = work.borrow().get(sel as usize) {
        w.set_teams(slint::ModelRc::from(Rc::new(slint::VecModel::from(
            team_rows_for(bot),
        ))));
    }
}

/// 同步弹窗模板列表 = 内置（前 builtin-count 个，不可编辑）+ 自定义（工作副本）。
fn sync_vb_templates(dlg: &VirtualBotDialog, templates_work: &RefCell<Vec<RoleTemplate>>) {
    let mut rows: Vec<RoleTemplateRow> = builtin_templates()
        .iter()
        .map(|t| RoleTemplateRow {
            name: t.name.clone().into(),
            prompt: t.prompt.clone().into(),
            checked: false,
        })
        .collect();
    for t in templates_work.borrow().iter() {
        rows.push(RoleTemplateRow {
            name: t.name.clone().into(),
            prompt: t.prompt.clone().into(),
            checked: false,
        });
    }
    dlg.set_templates(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
    dlg.set_builtin_count(builtin_templates().len() as i32);
    dlg.set_selected_count(0);
}

/// #125 编辑弹窗异步预填状态机（防「登记旧名残留 + 保存回退平台群名」）：
/// - Pending：打开即异步拉平台群资料，期间禁止保存（防把登记旧名/空提示词写回平台）；
/// - Ok：Fetched 成功，回填平台真实群名/群介绍（用户已改动的字段不回填）；
/// - Failed：拉取失败，恢复登记旧名供核对 + 强提示，须用户显式改过群名才允许保存。
#[derive(Clone, Copy, PartialEq, Debug)]
enum VbFetchPhase {
    Pending,
    Ok,
    Failed,
}

#[derive(Clone)]
struct VbEditState {
    phase: VbFetchPhase,
    chat_id: String,       // 目标群（Fetched 迟到时按 chat_id 作废）
    fallback_name: String, // 登记旧名（Failed 时恢复显示，供用户核对）
    name_dirty: bool,      // 用户手动改过群名 → 回填不覆盖
    prompt_dirty: bool,    // 用户手动改过提示词 → 回填不覆盖
}

impl Default for VbEditState {
    fn default() -> Self {
        VbEditState {
            phase: VbFetchPhase::Pending,
            chat_id: String::new(),
            fallback_name: String::new(),
            name_dirty: false,
            prompt_dirty: false,
        }
    }
}

/// #125 纯逻辑：Fetched 事件推进状态机，返回需要回填的 (群名, 群介绍)。
/// 返回 None = 不回填该字段（用户已手动改动 dirty，或值无效）；
/// error=Some 时状态转 Failed，群名回填登记旧名供用户核对。
fn vb_edit_apply_fetched(
    st: &mut VbEditState,
    name: &str,
    desc: &str,
    error: Option<&str>,
) -> (Option<String>, Option<String>) {
    if let Some(_err) = error {
        st.phase = VbFetchPhase::Failed;
        let fallback = if st.fallback_name.is_empty() {
            None
        } else {
            Some(st.fallback_name.clone())
        };
        return (fallback, None);
    }
    st.phase = VbFetchPhase::Ok;
    let n = if st.name_dirty || name.is_empty() {
        None
    } else {
        Some(name.to_string())
    };
    let p = if st.prompt_dirty {
        None
    } else {
        Some(desc.to_string())
    };
    (n, p)
}

/// #125 纯逻辑：编辑模式保存是否被拦截。返回 Some(msg) = 拦截并提示；None = 放行。
/// Pending（还没拉到平台资料）或 Failed（拉到失败、恢复登记旧名且用户未显式修改群名）
/// 都禁止静默保存，防把旧名/空提示词写回平台。
fn vb_edit_save_blocked(st: &VbEditState, chat_id: &str) -> Option<&'static str> {
    if st.chat_id != chat_id {
        return Some("群资料加载状态异常，请关闭弹窗重试");
    }
    match st.phase {
        VbFetchPhase::Pending => Some("正在拉取群资料，请稍候再保存…"),
        VbFetchPhase::Failed if !st.name_dirty => Some(
            "读取群资料失败，当前为登记旧名：直接保存会把平台群名改回旧名并清空群介绍。请核对修改群名后再保存",
        ),
        _ => None,
    }
}

/// 弹窗校验提示（hint-error=true 红色报错 / false 灰色说明）。
fn vb_hint(dlg: &VirtualBotDialog, text: &str, is_error: bool) {
    dlg.set_hint_text(text.into());
    dlg.set_hint_error(is_error);
}

/// 打开虚拟 Bot 弹窗（#75）：按模式预填 + 清空上次操作的残留（结果/进度/提示）。
/// 创建/登记模式由「＋ 创建虚拟 Bot」进入；编辑模式由列表项进入（预填来自登记 +
/// 平台群资料，见 VirtualBotFetchInfo 回填）。
#[allow(clippy::too_many_arguments)]
fn vb_open_dialog(
    dlg: &VirtualBotDialog,
    bot: &BotConfig,
    mode: i32,
    edit: Option<(&VirtualBot, &str, &str)>, // 编辑模式：登记 + (群名, 群介绍) 预填
    templates_work: &RefCell<Vec<RoleTemplate>>,
) {
    let (title, name, prompt) = match edit {
        Some((_reg, n, p)) => ("编辑虚拟 Bot", n.to_string(), p.to_string()),
        None => ("创建虚拟 Bot", String::new(), String::new()),
    };
    dlg.set_window_title(title.into());
    dlg.set_bot_label(if bot.name.is_empty() {
        format!("（{}）", kind_label(&bot.kind)).into()
    } else {
        bot.name.clone().into()
    });
    dlg.set_platform_label(kind_label(&bot.kind).into());
    dlg.set_mode(mode);
    dlg.set_name_input(name.as_str().into());
    dlg.set_prompt_input(prompt.as_str().into());
    dlg.set_prefix_input("".into());
    dlg.set_register_chat_id("".into());
    dlg.set_edit_chat_id(
        edit.map(|(r, _, _)| r.chat_id.clone())
            .unwrap_or_default()
            .into(),
    );
    dlg.set_busy(false);
    dlg.set_progress_text("".into());
    dlg.set_results(slint::ModelRc::from(Rc::new(slint::VecModel::from(Vec::<
        slint::SharedString,
    >::new(
    )))));
    dlg.set_template_editing(false);
    dlg.set_template_edit_idx(-1);
    dlg.set_template_edit_name("".into());
    dlg.set_template_edit_prompt("".into());
    // 预填内容的字符计数（与编辑回调同口径：chars().count()，中文安全）
    dlg.set_name_count(name.chars().count() as i32);
    dlg.set_prompt_count(prompt.chars().count() as i32);
    vb_hint(dlg, "", false);
    sync_vb_templates(dlg, templates_work);
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

/// bot 集合的「结构/凭证签名」：增删/启停/kind/凭证变化才需要重启 service
/// （bot 事件循环在 service 启动时构建）；后端/供应商/模型等热读字段不在签名内。
fn bots_struct_sig(c: &Config) -> String {
    let mut parts: Vec<String> = c
        .bots
        .iter()
        .map(|b| {
            format!(
                "{}|{}|{}|{}|{}|{}|{}|{}|{}",
                b.key(),
                b.enabled,
                b.kind,
                b.app_id,
                b.app_secret,
                b.wx_token,
                b.wx_user_id,
                b.ding_user_id,
                b.ding_robot_code
            )
        })
        .collect();
    // 供应商入签名：harness 的供应商 env 是 service 启动快照（run_bot 内
    // env_for_bot 按 bot 装配时读盘），GUI 改供应商（含补填 API Key）后必须
    // 重启才生效——Windows 实机：空 key 补填后不重启 → codex 持续报
    // `Missing environment variable: AGENT_BRIDGE_MODEL_KEY`。api_key 以 sha256
    // 摘要入签名（明文 key 绝不进字符串，它可能被日志/UI 侧读到）。
    use sha2::Digest as _;
    for pr in &c.providers {
        let key_hash: String = sha2::Sha256::digest(pr.api_key.as_bytes())
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect();
        parts.push(format!(
            "prov|{}|{}|{}|{}|{}",
            pr.name, pr.kind, pr.base_url, pr.model, key_hash
        ));
    }
    // 路由变更同样入签名：resolve_provider 每消息热读、harness env 是启动快照——
    // 只改「用哪个供应商」（bot.provider / default_provider）而供应商本体没动时，
    // 不重启就会继续拿旧供应商的 base_url/key 跑（与空 key 补填同型的快照-热读分叉）。
    parts.push(format!("prov|route|{}", c.default_provider));
    for b in &c.bots {
        parts.push(format!("prov|route|{}|{}", b.key(), b.provider));
    }
    parts.sort();
    parts.join(";")
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
        owner_open_id: b.owner_open_id.clone().into(),
        wx_owner_configured: !b.wx_user_id.is_empty(),
        owners: slint::ModelRc::from(Rc::new(slint::VecModel::from(Vec::<OwnerRow>::new()))),
        granted: slint::ModelRc::from(Rc::new(granted)),
        app_id: b.app_id.clone().into(),
        app_secret: b.app_secret.clone().into(),
        bot_name: b.bot_name.clone().into(),
        bot_open_id: b.bot_open_id.clone().into(),
        // per-bot 供应商名（""=跟随全局默认），直接显示原值（下拉第一项是 ""）
        provider: b.provider.clone().into(),
        ding_user_id: b.ding_user_id.clone().into(),
        ding_owner_ids: b.ding_owner_ids.clone().into(),
        ding_granted: slint::ModelRc::from(Rc::new(ding_granted)),
        ding_robot_code: b.ding_robot_code.clone().into(),
        restrict_granted: b.restrict_granted_agent,
        tidy_enabled: b.tidy_enabled,
    }
}

/// 供应商工作副本 → Slint ProviderRow。is-default 由名字与 default_provider_work 比对得出。
/// api_key 不回填（密码框留空，保存时留空=保留旧值）。
fn provider_to_row(p: &ProviderConfig, default_name: &str) -> ProviderRow {
    ProviderRow {
        name: p.name.clone().into(),
        kind: p.kind.clone().into(),
        // 下拉下标与 label 同源，避免 Slint 侧再写一遍类型表（未知类型兜底 0=anthropic）
        kind_index: crate::config::provider_kind_index(&p.kind).unwrap_or(0) as i32,
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

// ─────────────── #74 未读提醒 + 历史记录页（展示名解析 / 弹窗显隐 / 托盘红点合成）───────────────

/// #74 展示名解析：bot_key/sender_id → 人类可读名（config 反查，查不到用原始 id 兜底）。
/// 发送者名字来自授权者名单（授权时已反查名字落盘，桥内无需再异步反查）。
/// 返回 (bot 名, 发送者名)。
pub(crate) fn resolve_display(cfg: &Config, bot_key: &str, sender_id: &str) -> (String, String) {
    let bot = cfg.bots.iter().find(|b| b.key() == bot_key);
    let bot_name = bot
        .map(|b| {
            if b.bot_name.is_empty() {
                b.key()
            } else {
                b.bot_name.clone()
            }
        })
        .unwrap_or_else(|| bot_key.to_string());
    let sender = bot
        .and_then(|b| {
            let infos = if b.is_dingtalk() {
                &b.ding_granted_infos
            } else {
                &b.granted_infos
            };
            infos
                .iter()
                .find(|i| i.open_id == sender_id)
                .filter(|i| !i.name.is_empty())
                .map(|i| i.name.clone())
        })
        .unwrap_or_else(|| sender_id.to_string());
    (bot_name, sender)
}

/// #74 时间显示：unix 秒 → "MM-DD HH:MM"（本地时区，与 chrono_lite::now 同口径 UTC+8）。
/// 消息都是保留期内（≤90 天）的近期记录，不显示年份（微信同款简写）。
fn fmt_msg_time(ts: i64) -> String {
    let t = ts.max(0) as u64 + 8 * 3600;
    let (_y, mo, d, h, mi, _s) = crate::chrono_lite::epoch_to_ymd(t);
    format!("{mo:02}-{d:02} {h:02}:{mi:02}")
}

/// #74 历史页会话行（chat_stats 聚合 → ChatSessionRow，按最后消息时间倒序）。
fn chat_stats_to_row(s: &crate::msgstore::ChatStats, cfg: &Config) -> ChatSessionRow {
    let bot = cfg.bots.iter().find(|b| b.key() == s.bot_key);
    let bot_name = bot
        .map(|b| b.name.clone())
        .unwrap_or_else(|| s.bot_key.clone());
    // 会话名按类型：群聊 = 群名（事件自带，空则「群聊」兜底）；
    // 会话名：chat_name（回填的飞书会话名）优先——群聊即群名、单聊即对方名。
    // 实测飞书 chat API 的 chat_type 值为 "private"（群私聊同值），不可作群标；
    // 群标仅在显式 group 时打（保守：宁可漏标不误标）。chat_name 空（旧数据未
    // 回填/反查失败）回落人名解析链。
    let (fallback, _) = resolve_display(cfg, &s.bot_key, &s.last_sender_id);
    let person = if !s.last_sender.is_empty() && s.last_sender_id != "assistant" {
        s.last_sender.clone()
    } else {
        fallback
    };
    let person = if person.is_empty() {
        format!("…{}", &s.chat_id[s.chat_id.len().saturating_sub(8)..])
    } else {
        person
    };
    let (kind_tag, sender) = if s.chat_type == "group" {
        (
            "群",
            if s.chat_name.is_empty() {
                "群聊".to_string()
            } else {
                s.chat_name.clone()
            },
        )
    } else if !s.chat_name.is_empty() {
        // 回填后的会话名（群名或单聊对方名）——飞书 type 恒 private 不可判群，
        // 无群标但名字正确（用户核心诉求：看得到群名）
        ("", s.chat_name.clone())
    } else {
        ("", person)
    };
    ChatSessionRow {
        bot_key: s.bot_key.clone().into(),
        chat_id: s.chat_id.clone().into(),
        bot: bot_name.into(),
        sender: sender.into(),
        kind_tag: kind_tag.into(),
        time: s.last_ts.map(fmt_msg_time).unwrap_or_default().into(),
        count: format!("{} 条", s.count_total).into(),
        preview: crate::agent::truncate(
            &format!(
                "{}：{}",
                if s.last_sender.is_empty() {
                    "…"
                } else {
                    &s.last_sender
                },
                s.last_text
            ),
            40,
        )
        .into(),
    }
}

/// #74 历史会话排序：**按真实时间戳**倒序（最近活跃在前）。
///
/// 为什么不能用显示字符串当键：`fmt_msg_time` 产出 `"MM-DD HH:MM"`（不带年），
/// 跨年时 `"12-31 23:00" > "01-01 01:00"` 会把**旧的**排到前面；`last_ts` 缺失时
/// 得到空串，语义也不稳。抽成纯函数是为了把这两条钉进单测（见文件末测试）。
fn order_sessions_by_recency(
    stats: &[crate::msgstore::ChatStats],
) -> Vec<&crate::msgstore::ChatStats> {
    let mut ordered: Vec<&crate::msgstore::ChatStats> = stats.iter().collect();
    // None（从未有过时间戳）恒排最后；`sort_by_key` 稳定，时间相同的保持入参顺序。
    ordered.sort_by_key(|s| std::cmp::Reverse(s.last_ts.unwrap_or(i64::MIN)));
    ordered
}

/// #74 历史会话列表 model 整体替换（2s 轮询刷新；last_ts 倒序——最近活跃在前）。
fn sync_chat_sessions_model(
    model: &slint::VecModel<ChatSessionRow>,
    stats: &[crate::msgstore::ChatStats],
    cfg: &Config,
) {
    let rows: Vec<ChatSessionRow> = order_sessions_by_recency(stats)
        .into_iter()
        .map(|s| chat_stats_to_row(s, cfg))
        .collect();
    model.set_vec(rows);
}

/// #74 选中会话的消息流（ts 正序逐条 → ChatMsgRow；标题 = 会话名 · bot · N 条）。
fn sync_history_msgs(
    model: &slint::VecModel<ChatMsgRow>,
    w: &SettingsWindow,
    s: &crate::msgstore::ChatStats,
    rows: &[crate::msgstore::MsgRow],
    cfg: &Config,
) {
    let bot = cfg.bots.iter().find(|b| b.key() == s.bot_key);
    let bot_name = bot
        .map(|b| b.name.clone())
        .unwrap_or_else(|| s.bot_key.clone());
    let session_name = if !s.chat_name.is_empty() {
        s.chat_name.clone()
    } else {
        let (fallback, _) = resolve_display(cfg, &s.bot_key, &s.last_sender_id);
        if !s.last_sender.is_empty() && s.last_sender_id != "assistant" {
            s.last_sender.clone()
        } else {
            fallback
        }
    };
    w.set_chat_title(
        format!(
            "{}{} · {} · {} 条",
            if s.chat_type == "group" { "[群] " } else { "" },
            if session_name.is_empty() {
                &s.chat_id
            } else {
                &session_name
            },
            bot_name,
            s.count_total
        )
        .into(),
    );
    let out: Vec<ChatMsgRow> = rows
        .iter()
        .map(|r| {
            let (who, _) = resolve_display(cfg, &r.bot_key, &r.sender_id);
            let sender = if r.direction == "assistant" {
                bot_name.clone()
            } else if !r.sender_name.is_empty() {
                r.sender_name.clone()
            } else {
                who
            };
            ChatMsgRow {
                time: fmt_msg_time(r.ts).into(),
                direction: (if r.direction == "assistant" {
                    "回复"
                } else {
                    "用户"
                })
                .into(),
                sender: sender.into(),
                text: r.text.clone().into(),
            }
        })
        .collect();
    model.set_vec(out);
}

/// #74 未读项 → 弹窗行（最多 8 条：toast 高度有限，更早的看历史页）。
fn notify_rows(items: &[crate::unread::UnreadItem], cfg: &Config) -> Vec<NotifyRow> {
    items
        .iter()
        .take(8)
        .map(|it| {
            let (bot_name, sender) = resolve_display(cfg, &it.bot_key, &it.sender);
            // 展示名优先 items.name（bridge 反查：未授权用户 API 反查的真实名字；
            // 授权者本地名单查）——8-20 用户反馈：提醒显示名字而不是 open_id
            let sender = if !it.name.is_empty() {
                it.name.clone()
            } else {
                sender
            };
            NotifyRow {
                sender: sender.into(),
                bot: bot_name.into(),
                preview: it.preview.clone().into(),
                time: fmt_msg_time(it.ts).into(),
            }
        })
        .collect()
}

/// #74 显示提醒弹窗（toast）：显式定位屏幕右上角——与设置窗「位置零干预」的记忆决策
/// 不冲突：那是主窗的日常摆放，toast 若走系统默认级联位会落在屏幕随机位置，违背 toast
/// 的角落锚定语义（此处是唯一例外，注释留痕）。顺序同 show_window_and_focus：先激活再
/// show（与 winit `focus_window()` 的两步同序）；show 后 set_position 发生在 macOS 窗口
/// map 之前（同步路径），无可见跳动。
fn show_notifications_window(w: &NotificationsWindow) {
    use slint::winit_030::WinitWindowAccessor;
    bring_app_to_front();
    let _ = w.show();
    // 右上角：主屏宽 - 窗宽 - 边距；y 从菜单栏下方开始（macOS 菜单栏高约 24-28px）。
    // primary_monitor 拿不到时回落 1920（旧 Mac 常见宽），定位失败只是位置偏移不致命。
    let size = w.window().size();
    let pos = w
        .window()
        .with_winit_window(|ww| {
            let mw = ww.primary_monitor().map(|m| m.size().width).unwrap_or(1920);
            let margin = 16i32;
            slint::PhysicalPosition::new(mw as i32 - size.width as i32 - margin, 36)
        })
        .unwrap_or_else(|| slint::PhysicalPosition::new(0, 0));
    w.window().set_position(pos);
    w.window().request_redraw();
}

/// #74 收起提醒弹窗（5s 自动 / 点「知道了」/ 点条目跳转后共用）：
/// 设置窗还开着（点条目跳历史页）时不动 dock；否则弹窗收起即调一次 `hide_dock`
/// （D5 后它只是把「ABB 恒 accessory」重新钉一遍，避免闪烁的「先 hide 后 hide_dock」顺序照旧）。
fn hide_notifications_window(
    notif: &slint::Weak<NotificationsWindow>,
    settings: &slint::Weak<SettingsWindow>,
    showing: &Cell<bool>,
) {
    if let Some(n) = notif.upgrade() {
        let _ = n.hide();
    }
    if let Some(s) = settings.upgrade() {
        if !s.window().is_visible() {
            platform::hide_dock();
        }
    }
    showing.set(false);
}

/// #74 托盘红点合成（选型见 app.slint Tray 注释）：状态图标像素 → 右上角画红点
/// （Apple 系统红 #FF3B30 + 1px 白描边保证任何底色可见）。启动时付一次合成代价
/// （4 张 16-22px 小图）；失败静默回落原图（红点缺失只是少个提示，不影响功能）。
fn composite_tray_dot(base: &slint::Image) -> slint::Image {
    let fallback = base.clone();
    let Some(buf) = base.to_rgba8() else {
        return fallback;
    };
    let (w, h) = (buf.width(), buf.height());
    let mut px: Vec<slint::Rgba8Pixel> = buf.as_slice().to_vec();
    // 红点半径取短边 1/6（≥2px），内缩 margin≈半径/2——点落在图标右上角内
    let r = ((w.min(h) / 6).max(2)) as i32;
    let margin = (r / 2).max(1);
    let (cx, cy) = (w as i32 - margin - r, margin + r);
    for dy in -r..=r {
        for dx in -r..=r {
            let dist2 = dx * dx + dy * dy;
            if dist2 > r * r {
                continue;
            }
            let (x, y) = (cx + dx, cy + dy);
            if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
                continue;
            }
            let idx = (y as u32 * w + x as u32) as usize;
            px[idx] = if dist2 > (r - 1) * (r - 1) {
                // 白描边（外圈约 1px）
                slint::Rgba8Pixel {
                    r: 0xff,
                    g: 0xff,
                    b: 0xff,
                    a: 0xff,
                }
            } else {
                slint::Rgba8Pixel {
                    r: 0xff,
                    g: 0x3b,
                    b: 0x30,
                    a: 0xff,
                }
            };
        }
    }
    // AsPixels 无 RGBA→RGBA 恒等实现（只有 RGB→RGBA 加 alpha），改走
    // 新建 + copy（make_mut_slice 直接写像素）。
    let mut buf2 = slint::SharedPixelBuffer::new(w, h);
    buf2.make_mut_slice().copy_from_slice(&px);
    slint::Image::from_rgba8(buf2)
}

/// #74 落 GUI → service 命令文件（存在即消费，service 的 history-gc 轮询执行）：
/// msg-read.command=弹窗已读、msg-clear.command=清除全部历史。
/// 统一出口：GUI 侧不直写 unread.json/消息库（只读连接纪律），全部走命令文件。
fn write_command_file(name: &str) {
    let p = crate::bridge_dir().join("logs").join(name);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&p, "1");
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
    // 内容与当前工作副本一致（保存后签名推进/无意义触碰是常态）就到此为止：
    // 重载会整体销毁重建编辑区实例，无谓重建会在用户点进输入框还没敲键的窗口期抢走焦点
    if serde_json::to_string(&*work.borrow()).ok() == serde_json::to_string(&cfg.bots).ok() {
        return;
    }
    let sel = w.get_selected();
    *work.borrow_mut() = cfg.bots.clone();
    sync_model(model, &cfg.bots);
    w.set_selected(sel);
    // work 整体被替换：开关/编辑框的单元素 model 不随 bots model 自动跟随，必须整体重建
    refresh_bot_panel(w, work);
}

/// 单元素 model 构造。关键在于「整体替换 model」这一动作本身（set_row_data 不行）：
/// model 身份变化才会让 for 循环销毁重建组件实例、绑定全新——这是绕开 slint
/// 「用户交互赋值永久移除属性绑定」的唯一杠杆。
fn single_model<T: 'static + Clone>(row: T) -> slint::ModelRc<T> {
    slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![row])))
}

/// 按当前选中 bot 重建全部派生显示（授权码行 + 互斥框 + 各开关 + 各编辑框 +
/// 虚拟 Bot 登记/团队列表 + ⋯ 菜单收起）。所有改变选中 bot 或整体替换 work 的路径
/// （装载/切 bot/增删 bot/外部改盘重载）都必须走这里：单元素 model 与按 bot.key()
/// 重读的列表都不随 work 自动更新，漏调一处就残留上一个 bot 的值（#80 回归类）。
fn refresh_bot_panel(w: &SettingsWindow, work: &RefCell<Vec<BotConfig>>) {
    refresh_owner_code_info(w, work);
    refresh_exclusive_checks(w, work);
    refresh_toggle_checks(w, work);
    refresh_editors(w, work);
    // 虚拟 Bot/团队列表按选中 bot 重读；展开的 ⋯ 菜单引用旧行号，一并收起
    refresh_vb_rows(w, work);
    w.set_vb_menu_open(-1);
    refresh_team_rows(w, work);
    w.set_team_menu_open(-1);
}

/// 按当前选中 bot 重建各独立开关（启用/授权者隔离/每日整理/群聊提及）的单元素 model。
/// 整体替换 model → for 循环重建 PixelCheckBox 实例，实例全新、checked 绑定全新——
/// 绕开 slint「用户交互移除 checked 绑定（内部赋值断绑，绑 model 行属性或独立
/// property 都一样）、状态残留到其它 bot」的坑（#80 回归：点 A 的每日整理，切到 B
/// 显示残留，诱导误操作后连配置一起串）。
fn refresh_toggle_checks(w: &SettingsWindow, work: &RefCell<Vec<BotConfig>>) {
    let bots = work.borrow();
    let bot = bots.get(w.get_selected() as usize);
    let mk = |v: bool| -> slint::ModelRc<OptionRow> {
        single_model(OptionRow {
            name: "".into(),
            checked: v,
        })
    };
    w.set_enabled_options(mk(bot.map(|b| b.enabled).unwrap_or(false)));
    w.set_restrict_options(mk(bot.map(|b| b.restrict_granted_agent).unwrap_or(false)));
    w.set_tidy_options(mk(bot.map(|b| b.tidy_enabled).unwrap_or(false)));
    // #91 群聊提及默认（bot 级）：true=免 @ 参与
    w.set_mention_options(mk(bot.map(|b| b.mention_default).unwrap_or(false)));
}

/// 按当前选中 bot 重建编辑框/下拉的单元素 model（名称/类型/供应商/Owner/
/// App ID/App Secret/钉钉机器人编码/钉钉 Owner）。输入/选择交互同样移除 text/index/value
/// 绑定（与 checkbox 同源坑），直接绑 model 行属性会在切 bot 后残留上一 bot 的值；
/// 整体替换 model → for 循环重建实例，绑定全新。密码框按现有语义回填原值（bot_to_row 已如此）。
fn refresh_editors(w: &SettingsWindow, work: &RefCell<Vec<BotConfig>>) {
    let bots = work.borrow();
    let b = bots.get(w.get_selected() as usize);
    let mk = |v: &str| -> slint::ModelRc<EditorRow> { single_model(EditorRow { value: v.into() }) };
    w.set_name_editor_options(mk(b.map(|b| b.name.as_str()).unwrap_or("")));
    // 类型下拉同款重建（PixelComboBox 点选内部赋值 index 同样断绑，残留上一 bot）
    w.set_kind_editor_options(mk(b.map(|b| b.kind.as_str()).unwrap_or("")));
    w.set_provider_editor_options(mk(b.map(|b| b.provider.as_str()).unwrap_or("")));
    w.set_owner_editor_options(mk(b.map(|b| b.owner_open_id.as_str()).unwrap_or("")));
    w.set_app_id_editor_options(mk(b.map(|b| b.app_id.as_str()).unwrap_or("")));
    w.set_app_secret_editor_options(mk(b.map(|b| b.app_secret.as_str()).unwrap_or("")));
    w.set_ding_robot_code_editor_options(mk(b.map(|b| b.ding_robot_code.as_str()).unwrap_or("")));
    w.set_ding_owner_ids_editor_options(mk(b.map(|b| b.ding_owner_ids.as_str()).unwrap_or("")));
}

/// 重算对话权限互斥 CheckBox 的勾选态（飞书/钉钉同模型）。
/// 切 bot / 装载设置窗时调用。整体替换 option model → for 循环重建 CheckBox 实例，
/// 绕开 slint「用户交互移除 checked 绑定、状态残留到其它 bot」的坑。
/// （P4.3：后端三选一组已随 UI 下架删除——执行层收口随包 abb-agent。）
fn refresh_exclusive_checks(w: &SettingsWindow, _work: &RefCell<Vec<BotConfig>>) {
    // #118：访问控制收紧后无「公开」一档，对话权限固定为「仅授权用户」
    // （open_access / ding_open_access 字段保留兼容旧 config，判定链已不读）。
    let access_model = slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![OptionRow {
        name: "仅授权用户".into(),
        checked: true,
    }])));
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

/// 跑一次依赖检测并把全部依赖状态回填到设置窗（node/python3/lark-cli/dingtalk-cli/git）。
/// P4.3：claude/codex/pi 与 ACP 适配器探测已下架——执行层收口随包 abb-agent（零安装），
/// 余下为 agent 扩展能力依赖（缺失不阻断聊天）。
fn push_deps_to_window(w: &SettingsWindow) {
    let all = crate::deps::detect_all();
    let get = |id: &str| {
        all.iter()
            .find(|d| d.id == id)
            .map(|d| d.found)
            .unwrap_or(false)
    };
    // #105 git 三态：git_ok = 已装且版本 >= MIN_GIT_VERSION（< 2.30 → 需升级）。
    let git = all.iter().find(|d| d.id == "git");
    let git_version = git.map(|d| d.version.clone()).unwrap_or_default();
    let git_ok = git.map(|d| d.found && d.version_ok).unwrap_or(false);
    w.set_node_installed(get("node"));
    w.set_python_installed(get("python3"));
    w.set_lark_installed(get("lark-cli"));
    w.set_dingtalk_installed(get("dingtalk-cli"));
    w.set_git_installed(get("git"));
    w.set_git_version(git_version.into());
    w.set_git_ok(git_ok);
    // 主动重新检测/启动 = 新的开始：清掉上次一键安装的失败计数
    //（失败详情 dep-detail 保留到下次安装；AllDone 分支在调用本函数后重新设回）
    w.set_dep_failed_count(0);
}

/// 把系统权限状态回填到设置窗（0=未授权 1=被拒绝 2=已授权 3=受系统策略限制）。
/// 3 只由 [`crate::deps::PermState::Restricted`] 产生（家长控制 / MDM / 屏幕使用时间），
/// 单列一档是因为它和「被拒绝」虽然都拿不到设备，但**点「去授权」没有用**。
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
                // 受限机器在隐私面板里授不了，展示成「被拒绝 + 去授权」会把人引到死路。
                PermState::Restricted => 3,
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
        use winit::platform::macos::WindowAttributesExtMacOS;
        // 渲染器：默认 femtovg（Cargo.toml 已去 skia）。SLINT_BACKEND（如
        // winit-software 兜底排查）由 BackendSelector 原生解析：含 "software"/"sw"
        // 短形式与 winit-* 前缀，未知取值打印 warning 后回退默认——不要在
        // 本路径自行复刻该解析（selector 不 trim，复刻必然漂移）。
        slint::BackendSelector::new()
            .with_winit_window_attributes_hook(|attrs| {
                attrs
                    .with_titlebar_transparent(true)
                    .with_title_hidden(true)
                    .with_fullsize_content_view(true)
            })
            .select()?;
    }
    #[cfg(not(target_os = "macos"))]
    slint::BackendSelector::new().select()?;
    Ok(())
}

// 无边框窗口缩放需要 `ResizeUi` 契约（`slint-pixel` 提供）：为生成的 SettingsWindow
// 实现 `on_begin_resize`，再由 `install_window_resize` 接到 winit 的 drag_resize_window。
// 宏必须在模块作用域调用（impl 块）。
slint_pixel::impl_resize_ui!(SettingsWindow);
// 对话窗也要能缩放（owner 2026-10-13）。
slint_pixel::impl_resize_ui!(SystemChatWindow);

// ── Windows：圆角窗口区域（SetWindowRgn） ─────────────────────────────────────
// 现象（用户实测）：`ui/app.slint` 根 Rectangle 已经画了 10px 圆角、圆角外 alpha=0，
// 但 Windows 上看着仍是直角方块。
// 归因：Slint 的 winit 后端在 Windows 上开的是**普通（非分层）窗口**——DWM 对普通窗口的
// 重定向位图按不透明处理，圆角外那圈 alpha=0 会被铺成实心；winit 唯一的 per-pixel alpha
// 路径 `DwmEnableBlurBehindWindow` 依赖系统「透明效果」（`HKCU\...\Personalize\`
// `EnableTransparency`），关掉时即失效。
// 修法：`SetWindowRgn` 是窗口管理器层面的**硬裁剪**，不经过 DWM 的 alpha 合成——区域外的
// 像素根本不属于窗口，桌面直接露出来，与系统透明效果开关无关（Win10/Win11 都成立）。
// 半径必须与 .slint 侧 `border-radius` 同值：不一致会在弧线外残留黑边或露底
//（两边同值 → 硬裁剪弧与绘制弧重合，这是本方案成立的前提）。
// 追加修复（用户实测）：圆角出来了，但**圆角弧上的黑色描边被砍掉一截**。
// 归因：`CreateRoundRectRgn` 用椭圆离散化生成弧线，其边界比理想圆弧**内切 1–3px**
//（scale=1.5 时顶行内切 3px，逐行实测），硬裁剪于是吃掉了设计里的外圈描边。
// 修法：不再用 `CreateRoundRectRgn`，改为逐行扫描线圆角矩形的**矩形带并集**
//（`round_rect_bands` 用与 Slint 绘制弧同一套栅格化规则 → 边界逐行重合，描边完整）。
/// Windows 窗口圆角半径（**逻辑 px**，与 `ui/app.slint` 根 Rectangle 的
/// `border-radius: 10px` 必须同值）。
#[cfg(target_os = "windows")]
const WINDOW_CORNER_RADIUS: f64 = 10.0;

/// `CombineRgn` 的并集模式（`RGN_OR`）。
#[cfg(target_os = "windows")]
const RGN_OR: i32 = 2;

/// 取 winit 窗口的 Win32 HWND。
#[cfg(target_os = "windows")]
fn win32_hwnd(window: &slint::winit_030::winit::window::Window) -> Option<*mut std::ffi::c_void> {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get() as *mut std::ffi::c_void),
        // 非 Win32 句柄只在 Windows 上不可达（后端就是 winit-win32），防御性返回
        _ => None,
    }
}

/// 逐行扫描线圆角矩形 → 单像素高矩形带（窗口坐标，右下为开区间）。
///
/// 为什么不用 `CreateRoundRectRgn`：它按椭圆离散化生成弧线，边界会**切进**理想圆弧
/// 1–3px（实测 scale=1.5 时顶行内切 3px），窗口区域于是把设计里的外圈描边吃掉一截，
/// 弧上的黑边看起来「被砍掉」。这里改用**像素中心落在理想圆弧内**的规则
/// `xl(y) = ceil(R - sqrt(R² - cy²) - 0.5)`（`cy = R - (y + 0.5)`，`R` 为物理半径，
/// 可为小数）——与 Slint 绘制圆角边框时的栅格化规则一致（实测白面板弧与同规则
/// `r=12` 的预测逐行吻合）——再用逐行
/// 矩形并集精确表达，区域边界与设计弧线重合，描边不再被切。
///
/// 相邻等宽行会并成一条带，返回的矩形两两不重叠（可安全 OR 进同一区域）。
///
/// `cfg(any(test, windows))`：这是**纯数学**函数（唯一的平台相关性只是它紧挨着 Win32 调用），
/// 让它在 `cargo test` 下也编译，才可能在 macOS 上给它写单测——否则这段边界逻辑只在
/// Windows 上被编译，CI 与本地都覆盖不到（审查意见）。
#[cfg(any(test, target_os = "windows"))]
fn round_rect_bands(width: i32, height: i32, radius: f64) -> Vec<(i32, i32, i32, i32)> {
    if width <= 0 || height <= 0 {
        return Vec::new();
    }
    // 有效半径按**半宽/半高**钳制（独立审查建议）。只钳 `xl` 不够：
    // ① `width < 2r` 时 `width - xl` 会小于 `xl` → `CreateRectRgn` 收到反向矩形并被规范化，
    //    形状与绘制弧不符（枚举实测 w=1,h=1,r=7.5 → (5,0,-4,1)）；
    // ② `height < 2r` 时上下圆角区**重叠**，`if y < r` 分支恒先命中，上下不再对称。
    // 钳到半宽半高后两个问题一起消失，且弧只在放得下的范围内生成。
    let r = radius
        .max(1.0)
        .min(width as f64 / 2.0)
        .min(height as f64 / 2.0);
    let inset = |y: i32| -> i32 {
        // 该行像素中心到圆心 (R, R) 的竖直距离
        let cy = r - (y as f64 + 0.5);
        if cy <= 0.0 {
            return 0; // 已进入直边区
        }
        let dx = (r * r - cy * cy).max(0.0).sqrt();
        // 像素中心落在圆内的最小 x 列
        ((r - dx) - 0.5).ceil().max(0.0) as i32
    };
    let mut bands: Vec<(i32, i32, i32, i32)> = Vec::with_capacity(height as usize);
    for y in 0..height {
        let xl = if (y as f64) < r {
            inset(y)
        } else if (y as f64) >= height as f64 - r {
            inset(height - 1 - y) // 下边缘与上边缘对称
        } else {
            0
        };
        // 退化保护：窗口比圆角直径还窄时（极小窗 / 将来改小 min-width / 复用本函数），
        // `xl` 可能大到让 `width - xl <= xl` —— `CreateRectRgn` 收到反向矩形会把它规范化成
        // 「从右往里」的一条，形状与绘制弧不符（审查用枚举脚本扫出 width < 2r 时存在反向
        // 矩形）。这种尺寸本来就放不下圆角，直接**整行铺满**：既是良定义形状，也保证返回的
        // 矩形永远 l < r、t < b（注意不能钳成 width/2——那会得到零宽带）。
        let xl = if xl.saturating_mul(2) >= width { 0 } else { xl };
        debug_assert!(xl < width - xl, "带必须非空：xl={xl} width={width}");
        let band = (xl, y, width - xl, y + 1);
        match bands.last_mut() {
            // 连续（上一带下沿 == 当前行）且同宽 → 并入
            Some(last) if last.0 == band.0 && last.2 == band.2 && last.3 == band.1 => {
                last.3 = band.3;
            }
            _ => bands.push(band),
        }
    }
    bands
}

/// 把设置窗的窗口区域裁成圆角矩形。幂等：每次按当前尺寸/DPI 重算，尺寸变化后重调即刷新。
///
/// 最大化时清掉区域（贴满屏幕的窗口不该有圆角）。`SetWindowRgn` 成功后区域**归系统所有**，
/// 调用方不得再 `DeleteObject`；只有失败（返回 0）时才由本函数自行释放。
#[cfg(target_os = "windows")]
fn apply_round_window_region(window: &slint::Window) {
    use slint::winit_030::WinitWindowAccessor;
    use std::ffi::c_void;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetWindowRgn(hWnd: *mut c_void, hRgn: *mut c_void, bRedraw: i32) -> i32;
    }
    #[link(name = "gdi32")]
    unsafe extern "system" {
        fn CreateRectRgn(x1: i32, y1: i32, x2: i32, y2: i32) -> *mut c_void;
        fn CombineRgn(dst: *mut c_void, src1: *mut c_void, src2: *mut c_void, mode: i32) -> i32;
        fn DeleteObject(ho: *mut c_void) -> i32;
    }

    let _ = window.with_winit_window(|w| {
        let Some(hwnd) = win32_hwnd(w) else { return };
        // 最大化：清区域恢复整窗矩形（region = NULL 即「无区域」）
        if w.is_maximized() {
            // SAFETY: hwnd 取自 winit 窗口；NULL 区域是 SetWindowRgn 的合法入参（清除区域）
            unsafe { SetWindowRgn(hwnd, std::ptr::null_mut(), 1) };
            return;
        }
        let size = w.inner_size();
        let (width, height) = (size.width as i32, size.height as i32);
        if width <= 0 || height <= 0 {
            return; // 最小化等 0 尺寸：保持现有区域不动
        }
        // 物理半径 = 逻辑半径 × scale_factor（.slint 侧写的是逻辑 10px）。用浮点不做整数
        // 取整：绘制弧本就在 10×scale 处（如 1.5 → 15.0，1.25 → 12.5），取整会与绘制弧
        // 差半像素；逐行扫描线规则天然支持浮点半径，边界可与绘制弧精确重合。
        let radius = WINDOW_CORNER_RADIUS * w.scale_factor();
        // 圆角用逐行矩形带并集表达（见 round_rect_bands：与绘制弧逐行重合，
        // 不像 CreateRoundRectRgn 那样内切 1–3px 把描边吃掉）。
        // SAFETY: 纯 GDI 调用，参数为窗口尺寸与常量
        let rgn = unsafe { CreateRectRgn(0, 0, 0, 0) };
        if rgn.is_null() {
            return;
        }
        let mut ok = true;
        for (l, t, r, b) in round_rect_bands(width, height, radius) {
            // SAFETY: 同上；band 用完即释放
            let band = unsafe { CreateRectRgn(l, t, r, b) };
            if band.is_null() {
                ok = false;
                break;
            }
            // SAFETY: rgn/band 均为本函数创建的合法区域；RGN_OR 就地求并
            let rc = unsafe { CombineRgn(rgn, rgn, band, RGN_OR) };
            unsafe { DeleteObject(band) };
            if rc == 0 {
                ok = false;
                break;
            }
        }
        // bRedraw=1：区域变化立刻重绘（否则要等下一次内容刷新才见效）
        // SAFETY: rgn 是刚创建的合法区域；成功后所有权移交系统（不能再 DeleteObject）
        if !ok || unsafe { SetWindowRgn(hwnd, rgn, 1) } == 0 {
            // SAFETY: 失败（或区域没建好）时系统未接管，必须自己释放，否则泄漏 GDI 对象
            unsafe { DeleteObject(rgn) };
        }
    });
}

/// 平台相关的窗口事件副作用（挂在 `run_gui` 里**同一个** winit window-event filter 上——
/// `on_winit_window_event` 是单槽 `set`，另注册一个会把 `CloseRequested` 那条整个顶掉）。
/// - Windows：尺寸/DPI 变化后重算圆角窗口区域（窗口创建后的首个 `Resized` 也走这里，
///   即首帧拿到真实尺寸后补上圆角）。
/// - 其它平台：无（macOS 圆角由系统按 `border-radius` 合成，Linux 暂不处理）。
fn handle_platform_window_event(
    window: &slint::Window,
    event: &slint::winit_030::winit::event::WindowEvent,
) {
    #[cfg(target_os = "windows")]
    {
        use slint::winit_030::winit::event::WindowEvent;
        if matches!(
            event,
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. }
        ) {
            apply_round_window_region(window);
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (window, event);
    }
}

pub fn run_gui() -> Result<()> {
    configure_backend()?;

    // 旧「单 bot 平铺」数据 → workspaces/<key>/（幂等）。#187：顺带把
    // workspaces/<legacy_key>/ 折入 dest，service 的隔离键迁移不再被非空 dest 拦住
    //（contested——旧键被别的 bot 留守——时不折入，归属两序一致）。
    // flat 遗留是单 bot 时代产物，归首 bot；bot2+ 的迁移由 service 的 migrate_keys 负责。
    if let Ok(c) = Config::load() {
        platform::migrate_legacy_state(&c);
        // 新模型（2026-10-04）：退役旧的高权限常驻计划任务，避免它和托盘的普通用户服务抢锁。
        platform::retire_legacy_bridge_task();
    }

    let tray = Tray::new()?;
    // #74 托盘红点合成（启动一次）：状态图 → 右上角红点版，供 Tray 的 unread-count>0
    // 分支取用。合成失败静默回落原图（红点缺失只是少个提示）。
    tray.set_icon_online_dot(composite_tray_dot(&tray.get_icon_online()));
    tray.set_icon_connecting_dot(composite_tray_dot(&tray.get_icon_connecting()));
    tray.set_icon_offline_dot(composite_tray_dot(&tray.get_icon_offline()));
    tray.set_icon_dark_dot(composite_tray_dot(&tray.get_icon_dark()));
    let settings = SettingsWindow::new()?;
    // 像素自绘窗口：PixelTitleBar 窗口控制接线（关闭=隐藏，托盘应用惯例）
    slint_pixel::install_title_bar_controls_no_quit(&settings);
    // 无边框窗口缩放：把 `PixelWindowResize` 热区的方向回调接到 winit 的
    // `drag_resize_window`（系统级缩放循环）。macOS 的系统缩放本来就可用，且 .slint 里
    // 刻意没给 macOS 放热区，所以这里装上也无副作用（回调不会被触发）。
    slint_pixel::install_window_resize(&settings);
    // 缩放热区开关：**按目标平台显式给**（Windows/Linux 无系统缩放边框 → 自绘；
    // macOS 交给系统）。不用 `platform` 属性门控，避免依赖权限回填路径。
    // 口径收口在 [`resize_hit_areas_enabled`]（两个窗口共用一份，见那里的 macOS 取证）。
    settings.set_resize_hit_areas(resize_hit_areas_enabled());
    // 版本号随编译注入（Cargo.toml 单一事实源），侧栏底部展示
    settings.set_version(env!("CARGO_PKG_VERSION").into());
    // 托盘菜单的版本项也常驻当前版本（updater::CURRENT 同源）
    tray.set_version(env!("CARGO_PKG_VERSION").into());
    let qr_dialog = QrDialog::new()?;
    let unsaved_dialog = UnsavedDialog::new()?;
    // 虚拟 Bot（#75）：创建/编辑/登记弹窗 + 取消登记/解散群确认弹窗
    let vb_dialog = VirtualBotDialog::new()?;
    let team_dialog = TeamDialog::new()?; // #124 一键创建团队（P2，mock）
    let appoint_dialog = TeamAppointDialog::new()?; // #147 任命成员小表单
    let vb_confirm = ConfirmDialog::new()?;
    // 「服务管理密码」确认框（2026-10-04 owner：启动/停止/重启服务统一由管理密码把关）。
    // 与 vb_confirm 分开实例：文案与 need-password 状态互不干扰。
    let svc_pw = ConfirmDialog::new()?;
    svc_pw.set_confirm_text("确认".into());
    svc_pw.set_cancel_text("取消".into());
    svc_pw.set_danger(true);
    // #74 提醒弹窗：授权者私聊 toast（右上角、5s 自动收起）；创建后一直隐藏，
    // 2s 轮询发现未读时由 show_notifications_window 显示。
    let notifications = NotificationsWindow::new()?;
    // #74 清除历史二次确认弹窗：复用 UnsavedDialog 组件（独立实例，不干扰
    // 未保存修改那套对话框，各自接线）。
    let clear_dialog = UnsavedDialog::new()?;
    clear_dialog.set_title_text("清除历史记录".into());
    clear_dialog.set_message("将删除全部历史消息记录与未读提醒，不可恢复。确定清除？".into());
    clear_dialog.set_primary_text("清除".into());
    clear_dialog.set_discard_text("取消".into());
    clear_dialog.set_show_stay(false);
    // 弹窗是否正在显示（防重复弹）；5s 自动收起定时器（每次弹出 restart，见 tick 内）
    let notif_showing = Rc::new(Cell::new(false));
    let notif_timer = Rc::new(slint::Timer::default());
    // 设置窗编辑脏标记：任何字段/开关被改过 → true；保存/重新加载 → false。
    // 有未保存修改时，关闭/取消要先弹确认，避免静默丢编辑（红点/按钮都走这条保护）。
    let dirty = Rc::new(Cell::new(false));

    use slint::ComponentHandle;

    // ── 系统级对话：全局锚点气泡 + AI 对话框（GUI 专属）───────────────────────
    // 这是「系统级 Owner Bot」：owner 身份、整机全权限，不经任何 IM 渠道。
    // 气泡只在托盘进程运行时存在——ABB 无头（`--service`）时不实例化任何窗口，
    // 那时与 ABB 对话只能走 IM bot（产品里本就如此）。
    let syschat_win = SystemChatWindow::new()?;
    // 缩放：接 winit 的 drag_resize_window（`impl_resize_ui!` 提供回调实现）。
    // ⚠️ 热区**按平台**给（同设置窗，见 [`resize_hit_areas_enabled`]）：D 批「裸平台也开」在
    // macOS 上把标题栏顶部/两端变成了死区（`drag_resize_window` 在 macOS 返回 NotSupported，
    // 热区只吃按压不干活），也是 D6「抓标题栏拖不动窗口」的一个来源。
    slint_pixel::install_window_resize(&syschat_win);
    syschat_win.set_resize_hit_areas(resize_hit_areas_enabled());
    // 初始尺寸：`no-frame` 窗实测没吃到 `preferred-width`（开出来是 min 宽 340，
    // 于是 ①②③ 挤不进标题栏）。显式给一次 520×560（= 稿的落位宽度），此后由用户自己缩。
    syschat_win
        .window()
        .set_size(slint::LogicalSize::new(520.0, 560.0));
    slint_pixel::install_title_bar_controls_no_quit(&syschat_win);
    // D2（稿 ①②③）：标题栏控件条的落位计划。
    // 初次用 preferred-width 兜底（此时窗口尚未 realize，`size()` 可能是 0），
    // 之后改由窗口宽变化驱动：`.slint` 里的 `changed titlebar-watch => titlebar-width-changed(root.width)`
    // （首帧拿到真宽、缩放、DPI 变化都会触发）。为什么不用 winit `Resized`：同一件事，
    // Slint 侧的宽度就是布局真正用的值，不用再做物理→逻辑换算。
    {
        let cw = syschat_win.as_weak();
        syschat_win.on_titlebar_width_changed(move |w| {
            if let Some(win) = cw.upgrade() {
                apply_syschat_layout(&win, w);
            }
        });
    }
    apply_syschat_layout(&syschat_win, syschat_logical_width(&syschat_win));
    let syschat_model: Rc<slint::VecModel<SysChatBlockRow>> = Rc::new(slint::VecModel::default());
    syschat_win.set_blocks(slint::ModelRc::from(syschat_model.clone()));

    // 一段消息 → 渲染行：角色行 + 若干内容块（B 批：完整 markdown）。
    fn role_row(label: &str) -> SysChatBlockRow {
        SysChatBlockRow {
            kind: "role".into(),
            text: label.into(),
            styled: slint::StyledText::default(),
            level: 0,
            order: 0,
            checked: false,
            marker: "".into(),
            lang: "".into(),
            cells: slint::ModelRc::from(Rc::new(slint::VecModel::from(
                Vec::<slint::SharedString>::new(),
            ))),
            header: false,
            src: "".into(),
            alt: "".into(),
            img: slint::Image::default(),
            img_ok: false,
            think_expanded: false,
            think_live: false,
            think_elapsed: "".into(),
        }
    }
    /// 加载块图片：本地路径直读；http(s) 读本地缓存（未缓存则先空，由后台下载后重载）。
    /// 本地路径统一按 **syschat 工作区**基准解析（[`crate::syschat::resolve_media_path`]）。
    fn load_block_image(src: &str) -> (slint::Image, bool) {
        if src.is_empty() {
            return (slint::Image::default(), false);
        }
        let path: Option<std::path::PathBuf> =
            if src.starts_with("http://") || src.starts_with("https://") {
                crate::syschat::media_cache_path(src)
            } else {
                // 不能用进程 CWD：agent 输出的相对路径是相对它的工作区（= syschat 工作区）
                let p = crate::syschat::resolve_media_path(src);
                p.is_file().then_some(p)
            };
        match path.and_then(|p| slint::Image::load_from_path(&p).ok()) {
            Some(img) => (img, true),
            None => (slint::Image::default(), false),
        }
    }
    /// 加载待发附件缩略图（本地文件）。失败回落 `thumb-ok=false`（UI 显示字形，不空白）。
    fn load_thumb(path: &std::path::Path) -> (slint::Image, bool) {
        match slint::Image::load_from_path(path) {
            Ok(img) => (img, true),
            Err(_) => (slint::Image::default(), false),
        }
    }
    fn block_row(b: crate::mdblocks::MdBlock) -> SysChatBlockRow {
        let (img, img_ok) = load_block_image(&b.src);
        // 列表标记在 Rust 侧算好（Slint 里 int→string 拼接不可靠）
        let marker: String = match b.kind.as_str() {
            "task" => {
                if b.checked {
                    "☑".to_string()
                } else {
                    "☐".to_string()
                }
            }
            "list" if b.order > 0 => format!("{}. ", b.order),
            "list" => "•".to_string(),
            _ => String::new(),
        };
        SysChatBlockRow {
            kind: b.kind.into(),
            // styled 必须在 text 之前算（text 会 move b.text）
            styled: crate::mdblocks::styled(&b.text),
            text: b.text.into(),
            level: b.level,
            order: b.order,
            checked: b.checked,
            marker: marker.into(),
            lang: b.lang.into(),
            cells: slint::ModelRc::from(Rc::new(slint::VecModel::from(
                b.cells
                    .into_iter()
                    .map(slint::SharedString::from)
                    .collect::<Vec<_>>(),
            ))),
            header: b.header,
            src: b.src.into(),
            alt: b.alt.into(),
            img,
            img_ok,
            think_expanded: false,
            think_live: false,
            think_elapsed: "".into(),
        }
    }
    /// 思考块行（E2）：内容放 `text`，展开态/进行态/耗时放专用字段；
    /// 默认**折叠**（`expanded=false`）；`live` = 思考仍在进行（流式展开让打字机可见，
    /// 一旦进入正文或收尾就自动收起）。
    fn think_row(text: &str, elapsed: &str, expanded: bool, live: bool) -> SysChatBlockRow {
        SysChatBlockRow {
            kind: "think".into(),
            text: text.into(),
            styled: slint::StyledText::default(),
            level: 0,
            order: 0,
            checked: false,
            marker: "".into(),
            lang: "".into(),
            cells: slint::ModelRc::from(Rc::new(slint::VecModel::from(
                Vec::<slint::SharedString>::new(),
            ))),
            header: false,
            src: "".into(),
            alt: "".into(),
            img: slint::Image::default(),
            img_ok: false,
            think_expanded: expanded,
            think_live: live,
            think_elapsed: elapsed.into(),
        }
    }
    fn rows_for(role: &str, text: &str) -> Vec<SysChatBlockRow> {
        let label = if role == "assistant" { "怹" } else { "你" };
        let mut rows = vec![role_row(label)];
        for b in crate::mdblocks::parse(text) {
            rows.push(block_row(b));
        }
        rows
    }
    /// 一轮历史 → 渲染行：角色行 +（有思考则）思考块 + 正文 markdown 块。
    fn rows_for_turn(t: &crate::syschat::Turn) -> Vec<SysChatBlockRow> {
        let mut rows = rows_for(&t.role, &t.text);
        if !t.thought.is_empty() {
            // 历史里的思考块**默认折叠**（需求 #11），耗时历史未存 ⇒ 不显示数字。
            rows.insert(1, think_row(&t.thought, "", false, false));
        }
        rows
    }
    /// 流式状态的当前快照 → 渲染行：角色行 +（有思考则）思考块 + 正文块。
    /// `expanded` 由 `live` 决定（进行中展开以显示打字机，结束/进正文即收起）。
    fn stream_rows(
        revealed_text: &str,
        has_thought: bool,
        thought_text: &str,
        elapsed: &str,
        live: bool,
    ) -> Vec<SysChatBlockRow> {
        let mut rows = vec![role_row("怹")];
        if has_thought {
            rows.push(think_row(thought_text, elapsed, live, live));
        }
        for b in crate::mdblocks::parse(revealed_text) {
            rows.push(block_row(b));
        }
        rows
    }
    /// 用当前累积文本重建「流式气泡」的行：从 `start` 起到末尾全替换。
    ///
    /// 为什么整段重建：增量改行需要知道 markdown 分块边界，而 `mdblocks::parse`
    /// 随文本增长会重新分块；整段替换是唯一不会留残行/重复角色的做法。
    /// 去重口径：回合结束时调用方用 `final_text` 重建同一段——不保留旧拼接、不追加第二条。
    ///
    /// 已知边界（E2）：流式期间手动展开的**已完成**思考块会在下一次重建时恢复默认折叠
    ///（重建按内容重算行，不跨重建携带用户点开的态）；回合收尾后的点击则不会被重建覆盖。
    fn sync_stream_rows(
        model: &slint::VecModel<SysChatBlockRow>,
        start: usize,
        rows: Vec<SysChatBlockRow>,
    ) {
        while model.row_count() > start {
            model.remove(model.row_count() - 1);
        }
        for r in rows {
            model.push(r);
        }
    }
    // 当前会话里还没缓存的 http(s) 媒体（图片/音视频）——交给后台下载。
    fn pending_http_media() -> Vec<String> {
        let mut v = Vec::new();
        for t in crate::syschat::load_history() {
            for b in crate::mdblocks::parse(&t.text) {
                if matches!(b.kind.as_str(), "image" | "audio" | "video")
                    && (b.src.starts_with("http://") || b.src.starts_with("https://"))
                    && crate::syschat::media_cache_path(&b.src).is_none()
                {
                    v.push(b.src);
                }
            }
        }
        v
    }
    // 打开对话框时把落盘历史灌进 model（跨重启保留的可见证据）。
    fn load_syschat_into(win: &SystemChatWindow, model: &slint::VecModel<SysChatBlockRow>) {
        let turns = crate::syschat::load_history();
        let mut rows: Vec<SysChatBlockRow> = Vec::new();
        for t in &turns {
            rows.extend(rows_for_turn(t));
        }
        model.set_vec(rows);
        // 请求滞底：递增 epoch，由 .slint 的 changed 处理器把 content-y 设成越界值、
        // 交给 Flickable 的 in-bound 夹取（不能直接给 content-y 绑值，见 app.slint 注释）。
        win.set_scroll_epoch(win.get_scroll_epoch().wrapping_add(1));
    }
    load_syschat_into(&syschat_win, &syschat_model);

    // 系统对话 A 批：会话 / 模型选择的下拉状态工作副本。
    let sc_sessions: Rc<RefCell<Vec<crate::syschat::SessionMeta>>> =
        Rc::new(RefCell::new(Vec::new()));
    // (供应商 name, 下拉展示 label)
    let sc_models: Rc<RefCell<Vec<(String, String)>>> = Rc::new(RefCell::new(Vec::new()));

    /// 刷新会话/模型两个下拉：模型候选只来自「设置 → 供应商」；会话按 updated_at 倒序。
    fn refresh_syschat_sel(
        win: &SystemChatWindow,
        sessions: &Rc<RefCell<Vec<crate::syschat::SessionMeta>>>,
        models: &Rc<RefCell<Vec<(String, String)>>>,
    ) {
        let cfg = Config::load().unwrap_or_default();
        // v11：模型行要同时拿到 label 与 **provider kind**（稿：非当前项右侧写 anthropic/openai），
        // 故在这里一次性算出「名字 / label / kind」三元组，而不是事后拼平行数组。
        let ms: Vec<(String, String, String)> = cfg
            .providers
            .iter()
            .map(|p| {
                let label = if p.model.trim().is_empty() {
                    p.name.clone()
                } else {
                    p.model.clone()
                };
                (p.name.clone(), label, p.kind.clone())
            })
            .collect();
        let (list, current) = crate::syschat::session_list();
        let labels: Vec<slint::SharedString> =
            list.iter().map(|s| s.title.clone().into()).collect();
        let sidx = list
            .iter()
            .position(|s| s.id == current)
            .map(|i| i as i32)
            .unwrap_or(-1);
        // 会话 provider 为空 = 跟随全局默认 → 下拉回显全局默认那一项
        let cur = list
            .iter()
            .find(|s| s.id == current)
            .map(|s| s.provider.clone())
            .unwrap_or_default();
        let effective = if cur.is_empty() {
            cfg.default_provider.clone()
        } else {
            cur
        };
        let midx = ms
            .iter()
            .position(|(n, _, _)| *n == effective)
            .map(|i| i as i32);
        win.set_session_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(labels))));
        let mlabels: Vec<slint::SharedString> =
            ms.iter().map(|(_, l, _)| l.clone().into()).collect();
        win.set_model_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(
            mlabels,
        ))));
        win.set_session_index(sidx);
        win.set_model_index(midx.unwrap_or(-1));
        // v11（卡 `abb-v11-panels-align`）：两个面板的行数据（标题 + 右侧 tag/✓）。
        // 与 labels 同一次刷新写入 ⇒ 面板与 chip 不会看到一个新列表一个旧列表。
        let now = crate::chrono_lite::unix_secs();
        let srows: Vec<SysChatMenuRow> = list
            .iter()
            .map(|s| SysChatMenuRow {
                text: s.title.clone().into(),
                tag: crate::syschat::session_tag(s.updated_at, now, s.id == current).into(),
                tick: false,
            })
            .collect();
        win.set_session_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(srows))));
        let mrows: Vec<SysChatMenuRow> = ms
            .iter()
            .enumerate()
            .map(|(i, (_, label, kind))| SysChatMenuRow {
                text: label.clone().into(),
                // 当前项右侧画 ✓（tag 置空）；其它项右侧写 provider kind（稿 anthropic/openai）。
                tag: if Some(i as i32) == midx {
                    slint::SharedString::default()
                } else {
                    kind.clone().into()
                },
                tick: Some(i as i32) == midx,
            })
            .collect();
        win.set_model_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(mrows))));
        refresh_syschat_usage(win);
        *sessions.borrow_mut() = list;
        *models.borrow_mut() = ms.iter().map(|(n, l, _)| (n.clone(), l.clone())).collect();
        // D2：会话/模型 chip 的显示文案随列表与选中项变（按当前窗口宽重新截断）。
        apply_syschat_layout(win, syschat_logical_width(win));
    }

    refresh_syschat_sel(&syschat_win, &sc_sessions, &sc_models);

    // ── C1 批：待发附件（📎 多选 / ⌘V 粘贴）──────────────────────────────────
    //
    // 待发附件只在 **UI 主线程**维护（Slint 窗口与 Rc 都非 Send）；选取/粘贴后立即落地
    // 到工作区附件目录并上屏，发送时路径随消息文本一起进 prompt（见
    // `syschat::prompt_with_attachments`）——二进制不进 prompt。
    let sc_atts: Rc<RefCell<Vec<crate::syschat::Attachment>>> = Rc::new(RefCell::new(Vec::new()));
    // E1/E2 流式状态：分段累积器 + 两路客户端打字机（正文 / 思考）+ 气泡在 model 里的
    // 起始行（`usize::MAX` = 当前没有流式气泡）+ 在途回合的外部取消令牌（⏹ 停止）
    // + 回合结局（Reply 要等打字机推完再收尾；Stopped 立即收）。都是 UI 线程本地状态。
    let sc_stream: Rc<RefCell<crate::syschat::StreamingBubble>> =
        Rc::new(RefCell::new(crate::syschat::StreamingBubble::default()));
    // 两路打字机：后端一次吐一大段时（实测 abb-agent 就是），按拍把文本逐段推上屏。
    let sc_tw_text: Rc<RefCell<crate::syschat::Typewriter>> =
        Rc::new(RefCell::new(crate::syschat::Typewriter::new()));
    let sc_tw_think: Rc<RefCell<crate::syschat::Typewriter>> =
        Rc::new(RefCell::new(crate::syschat::Typewriter::new()));
    let sc_stream_end: Rc<RefCell<Option<StreamEnd>>> = Rc::new(RefCell::new(None));
    let sc_stream_start: Rc<Cell<usize>> = Rc::new(Cell::new(usize::MAX));
    let sc_cancel: Rc<RefCell<Option<tokio_util::sync::CancellationToken>>> =
        Rc::new(RefCell::new(None));
    // 卡 `abb-syschat-d10-insert-interrupt`：发送→回合的串行泵 + 生成中插入端点。
    // - `sc_pump`：纯逻辑（`syschat::SendPump`，单测钉住）——插入/排队判定 + 队列串行化；
    // - `sc_inject`：在途回合的注入端（仅能力=Supported 时写入）；
    // - `sc_cap`：在途回合的 steer 能力 watch（harness initialize 后落锤，UI 每拍同步进泵）。
    let sc_pump: Rc<RefCell<crate::syschat::SendPump>> =
        Rc::new(RefCell::new(crate::syschat::SendPump::new()));
    let sc_inject: Rc<
        RefCell<Option<tokio::sync::mpsc::UnboundedSender<crate::runtime::queue::InboundMsg>>>,
    > = Rc::new(RefCell::new(None));
    let sc_cap: Rc<
        RefCell<Option<tokio::sync::watch::Receiver<crate::runtime::harness::SteerSupport>>>,
    > = Rc::new(RefCell::new(None));
    // D 批 3+N：展开/折叠态。纯逻辑在 `syschat::PendingState`（可单测），这里只存状态。
    let sc_pending: Rc<Cell<crate::syschat::PendingState>> =
        Rc::new(Cell::new(crate::syschat::PendingState::new()));

    /// 刷新对话窗的待发附件行（D 批：框内 3+N chips，替换 C1 的 `📎 N 个：…` 简行）。
    /// `attach-count` 控制整行显隐；`pending-rows` = 按折叠态裁好的可见行；`pending-more` = 折叠数 N。
    /// 列表清空时把展开态复位——否则发完一批后残留「展开」，下一批一上来就全展开（非预期）。
    fn refresh_syschat_atts(
        win: &SystemChatWindow,
        atts: &Rc<RefCell<Vec<crate::syschat::Attachment>>>,
        pending: &Rc<Cell<crate::syschat::PendingState>>,
    ) {
        let v = atts.borrow();
        if v.is_empty() {
            pending.set(crate::syschat::PendingState::new());
        }
        let state = pending.get();
        let view = state.view(v.len());
        let rows: Vec<PendingAttachRow> = v
            .iter()
            .take(view.shown)
            .enumerate()
            .map(|(i, a)| {
                let is_image = crate::syschat::is_image_attachment(&a.name);
                let (thumb, thumb_ok) = if is_image {
                    load_thumb(&a.path)
                } else {
                    (slint::Image::default(), false)
                };
                PendingAttachRow {
                    index: i as i32,
                    name: a.name.clone().into(),
                    is_image,
                    thumb,
                    thumb_ok,
                    ext: crate::syschat::attachment_kind_label(&a.name).into(),
                }
            })
            .collect();
        win.set_attach_count(v.len() as i32);
        win.set_pending_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
        // `N == 0` ⇒ 不渲染 `+N`（由 Slint 的 `pending-more > 0` 控制）。
        win.set_pending_more(if view.has_more() {
            view.hidden as i32
        } else {
            0
        });
        // `+N` 文案在 Rust 侧拼（Slint 的 int→string 拼接不可靠，见 `.slint` 的列表标记注释）。
        win.set_pending_more_text(format!("+{}", view.hidden).into());
        win.set_pending_expanded(state.is_expanded());
    }
    refresh_syschat_atts(&syschat_win, &sc_atts, &sc_pending);

    // ── 卡 d9：表情面板 /「更多（定时）」菜单的 UI 线程状态 ──────────────────
    //
    // - `sc_emoji_cat` / `sc_emoji_page`：面板的分组/页码（内容由 `emoji.rs` 纯函数算）；
    // - `sc_sched`：定时发送队列（`syschat_sched`，落盘 `~/.agent-bridge/syschat/scheduled.json`），
    //   到点由下方 2s 主定时器取出并走**与手动发送同一个提交入口**（`submit_sc`）。
    let sc_emoji_cat: Rc<Cell<usize>> = Rc::new(Cell::new(0));
    let sc_emoji_page: Rc<Cell<usize>> = Rc::new(Cell::new(0));
    let sc_sched: Rc<crate::syschat_sched::SchedStore> =
        Rc::new(crate::syschat_sched::SchedStore::load());

    /// 把 `emoji.rs` 算出的当前页/分组写进窗口（分类 tab、格子、页码、翻页可用性）。
    fn refresh_emoji_panel(win: &SystemChatWindow, cat: &Rc<Cell<usize>>, page: &Rc<Cell<usize>>) {
        let c = cat.get();
        let p = crate::emoji::clamp_page(c, page.get() as i32);
        page.set(p);
        let cells: Vec<slint::SharedString> = crate::emoji::page_cells(c, p)
            .into_iter()
            .map(slint::SharedString::from)
            .collect();
        win.set_emoji_cells(slint::ModelRc::from(Rc::new(slint::VecModel::from(cells))));
        win.set_emoji_cat_index(c as i32);
        win.set_emoji_page_label(crate::emoji::page_label(c, p).into());
        win.set_emoji_prev_enabled(p > 0);
        win.set_emoji_next_enabled(p + 1 < crate::emoji::page_count(c));
    }

    /// 把定时档位文案 + 待发列表写进「更多」菜单（每次打开/增删后调）。
    fn refresh_sched_menu(win: &SystemChatWindow, store: &Rc<crate::syschat_sched::SchedStore>) {
        let now = crate::schedule::DateTime::now();
        let labels: Vec<slint::SharedString> = crate::syschat_sched::preset_labels(now)
            .into_iter()
            .map(slint::SharedString::from)
            .collect();
        win.set_sched_preset_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(labels))));
        let items = store.list();
        let count = items.len() as i32;
        let rows: Vec<slint::SharedString> = items
            .iter()
            .map(|m| slint::SharedString::from(crate::syschat_sched::menu_row(m)))
            .collect();
        win.set_sched_items(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
        win.set_sched_count(count);
        win.set_sched_cancel_all_label(format!("取消全部定时（{count}）").into());
    }

    // 分类 tab 是常量，初始化就写一次（之后面板打开不必重设）。
    {
        let cats: Vec<slint::SharedString> = crate::emoji::category_names()
            .into_iter()
            .map(slint::SharedString::from)
            .collect();
        syschat_win.set_emoji_cats(slint::ModelRc::from(Rc::new(slint::VecModel::from(cats))));
        refresh_emoji_panel(&syschat_win, &sc_emoji_cat, &sc_emoji_page);
    }

    // 把一批本地路径落地进工作区附件目录并并入待发列表。失败 ⇒ 对话框状态行显示可读原因
    // （复用既有错误展示通道），**不静默丢弃**；成功时才把落盘记录并入待发。
    let land_into_pending: Rc<dyn Fn(Vec<std::path::PathBuf>)> = {
        let cw = syschat_win.as_weak();
        let atts = sc_atts.clone();
        let pending = sc_pending.clone();
        Rc::new(move |paths: Vec<std::path::PathBuf>| {
            if paths.is_empty() {
                return; // 取消选择 / 剪贴板无文件：不算失败，什么都不做
            }
            let Some(w) = cw.upgrade() else { return };
            match crate::syschat::land_attachments(&paths) {
                Ok(landed) => {
                    let n = landed.len();
                    atts.borrow_mut().extend(landed);
                    refresh_syschat_atts(&w, &atts, &pending);
                    w.set_status_is_error(false);
                    w.set_status_line(format!("已附加 {n} 个文件（随下一条消息发送）").into());
                }
                Err(e) => {
                    // land_attachments 整批回滚：不改待发列表，只报错。
                    w.set_status_is_error(true);
                    w.set_status_line(format!("附件落地失败：{e}").into());
                }
            }
        })
    };

    // 📎 多选文件：原生 `NSOpenPanel`（零依赖手写 FFI，见 platform 注释）。
    // **为什么在 Slint 回调里直接调**：AppKit 要求主线程，而 Slint 回调本就在 UI 主线程；
    // `UiCmd` 那条通道由后台 tokio 线程消费（见 `std::thread::spawn` 处的 `rx.recv()`），
    // 把 NSOpenPanel 塞过去就是在后台线程摸 AppKit——不要这么做。
    {
        let cw = syschat_win.as_weak();
        let land = land_into_pending.clone();
        syschat_win.on_attach_pick(move || {
            if cw.upgrade().is_none() {
                return; // 窗口已销毁（托盘退出中）：不再弹面板
            }
            bring_app_to_front(); // 面板要置于前台（先激活再弹，同 show_window_and_focus 的理由）
            land(crate::platform::pick_files_multi());
        });
    }
    // 「清空」：只清待发列表，**不删**已落盘文件（它们可能已被已发出的消息引用）。
    {
        let cw = syschat_win.as_weak();
        let atts = sc_atts.clone();
        let pending = sc_pending.clone();
        syschat_win.on_attach_clear(move || {
            atts.borrow_mut().clear();
            if let Some(w) = cw.upgrade() {
                refresh_syschat_atts(&w, &atts, &pending);
                w.set_status_is_error(false);
                w.set_status_line("已清空待发附件（已落盘的文件保留）".into());
            }
        });
    }
    // D 批：单个 ✕ —— 按**完整列表下标**移除；只动待发列表，**不删**已落盘文件，移除后重新分组。
    {
        let cw = syschat_win.as_weak();
        let atts = sc_atts.clone();
        let pending = sc_pending.clone();
        syschat_win.on_attach_remove(move |idx| {
            let new_len =
                crate::syschat::remove_pending_at(&mut atts.borrow_mut(), idx.max(0) as usize);
            if let Some(w) = cw.upgrade() {
                refresh_syschat_atts(&w, &atts, &pending);
                w.set_status_is_error(false);
                w.set_status_line(
                    format!("已移出 1 个待发附件（现 {new_len} 个；已落盘文件保留）").into(),
                );
            }
        });
    }
    // D 批：点 `+N` 展开全部 / 再点收起（`PendingState` 纯逻辑，见 syschat 单测）。
    {
        let cw = syschat_win.as_weak();
        let atts = sc_atts.clone();
        let pending = sc_pending.clone();
        syschat_win.on_pending_toggle(move || {
            let mut st = pending.get();
            st.toggle();
            pending.set(st);
            if let Some(w) = cw.upgrade() {
                refresh_syschat_atts(&w, &atts, &pending);
            }
        });
    }

    let bubble = AnchorBubble::new()?;
    // 气泡位置：统一在**事件循环活跃后的首个 tick** 里施加（见 tick）——启动前 set_position 会被
    // macOS 的窗口级联覆盖，保存的位置实际不生效（2026-10-13 实测：设 (1398,120) 后窗口落在 (1206,65)）。
    // 坐标统一用**逻辑像素**（winit 的 MonitorHandle::size() 是物理像素，Retina 下差 2 倍）。
    let bubble_pos_apply = Rc::new(Cell::new(true));
    // 气泡显示/隐藏统一走 refresh_bubble_visibility（只按托盘开关；v10 起与对话框显隐无关）。
    refresh_bubble_visibility(&bubble);

    // 点窗口红点（traffic-light）关闭 → 有未保存修改先弹确认，否则降回 accessory / 直接隐藏。
    // （取消/保存按钮走各自回调 hide；红点这条系统路径单独拦：CloseRequested 返回
    //  PreventDefault 可阻止 Slint 默认 hide，跨平台生效）
    {
        use slint::winit_030::{winit::event::WindowEvent, EventResult, WinitWindowAccessor};
        let dirty = dirty.clone();
        let dlg = unsaved_dialog.as_weak();
        settings.window().on_winit_window_event(move |w, ev| {
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
            handle_platform_window_event(w, ev);
            EventResult::Propagate // 让 Slint 照常把窗口 hide
        });
        qr_dialog.window().on_winit_window_event(|_w, ev| {
            // 子窗口关闭不动 dock 状态（8-20 用户反馈：关闭子窗口不该影响主设置窗）
            let _ = ev;
            EventResult::Propagate
        });
        // 虚拟 Bot 弹窗/确认弹窗：红点关闭 = 隐藏（同 QrDialog；busy 时禁止关闭防半途退出）
        {
            let vb_dw = vb_dialog.as_weak();
            vb_dialog.window().on_winit_window_event(move |_w, ev| {
                if matches!(ev, WindowEvent::CloseRequested)
                    && vb_dw.upgrade().map(|d| d.get_busy()).unwrap_or(false)
                {
                    return EventResult::PreventDefault; // 创建中：不让用户关掉弹窗丢进度
                }
                // 子窗口关闭不动 dock 状态（8-20 用户反馈：关闭子窗口不该影响主设置窗）
                EventResult::Propagate
            });
        }
        let vb_cw = vb_confirm.as_weak();
        vb_confirm.window().on_winit_window_event(move |_w, ev| {
            // 执行中（busy）禁止关闭：结果未返回前关掉弹窗会丢结果（8-20 用户反馈：
            // "拿到解散结果并且成功后再关闭"）——与 vb_dialog 的 busy 拦截同款
            if matches!(ev, WindowEvent::CloseRequested)
                && vb_cw.upgrade().map(|c| c.get_busy()).unwrap_or(false)
            {
                return EventResult::PreventDefault;
            }
            // 子窗口关闭不动 dock 状态（8-20 用户反馈：关闭子窗口不该影响主设置窗）
            EventResult::Propagate
        });
    }

    // 取消按钮：关掉二维码弹窗（登录轮询会自然超时结束）
    {
        let qw = qr_dialog.as_weak();
        qr_dialog.on_close_clicked(move || {
            if let Some(d) = qw.upgrade() {
                // 子窗口关闭不动 dock（8-20 用户反馈）
                let _ = d.hide();
            }
        });
    }

    // 虚拟 Bot 弹窗关闭：busy 时禁用（创建中不能关，见窗口事件拦截）；关闭 = 隐藏
    {
        let dw = vb_dialog.as_weak();
        vb_dialog.on_close_clicked(move || {
            if let Some(d) = dw.upgrade() {
                if !d.get_busy() {
                    // 子窗口关闭不动 dock（8-20 用户反馈）
                    let _ = d.hide();
                }
            }
        });
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<UiCmd>();
    // 卡 `abb-syschat-d10-insert-interrupt`：开一轮的**唯一出处**（on_send 的 Immediate
    // 与回合结束后的队列排空都调它），只负责回合态：取消令牌 / 注入端点 / 能力 watch /
    // 流式累积器复位 + 发 UiCmd。用户行与待发附件由调用方先处理（排队项提交时已上屏）。
    let start_sc_turn: Rc<dyn Fn(String, Vec<crate::syschat::Attachment>)> = {
        let cw = syschat_win.as_weak();
        let bw = bubble.as_weak();
        let tx_turn = tx.clone();
        let cancel_slot = sc_cancel.clone();
        let inject_slot = sc_inject.clone();
        let cap_slot = sc_cap.clone();
        let stream = sc_stream.clone();
        let stream_start = sc_stream_start.clone();
        let stream_end = sc_stream_end.clone();
        let tw_text = sc_tw_text.clone();
        let tw_think = sc_tw_think.clone();
        let pump = sc_pump.clone();
        let f = move |text: String, atts_now: Vec<crate::syschat::Attachment>| {
            let Some(w) = cw.upgrade() else { return };
            // 串行泵进“在途”（真源）：之后到达的提交据此判 Queue/Steer。
            pump.borrow_mut().begin();
            let att_note = if atts_now.is_empty() {
                String::new()
            } else {
                format!("（附 {} 个文件）", atts_now.len())
            };
            w.set_busy(true);
            w.set_status_is_error(false);
            w.set_status_line(format!("怹正在思考{att_note}……").into());
            w.set_scroll_epoch(w.get_scroll_epoch().wrapping_add(1));
            if let Some(b) = bw.upgrade() {
                b.set_busy(true);
            }
            // E1/E2：为本轮建外部取消令牌（⏹ 停止直接 `.cancel()`），并重置流式累积/打字机/结局态。
            let token = tokio_util::sync::CancellationToken::new();
            *cancel_slot.borrow_mut() = Some(token.clone());
            // 本卡：本轮的生中插入端点与 steer 能力 watch（每轮一套，回合结束即失效）。
            let (inject_tx, inject_rx) = tokio::sync::mpsc::unbounded_channel();
            *inject_slot.borrow_mut() = Some(inject_tx);
            let (cap_tx, cap_rx) =
                tokio::sync::watch::channel(crate::runtime::harness::SteerSupport::Unknown);
            *cap_slot.borrow_mut() = Some(cap_rx);
            *stream.borrow_mut() = crate::syschat::StreamingBubble::start();
            *tw_text.borrow_mut() = crate::syschat::Typewriter::new();
            *tw_think.borrow_mut() = crate::syschat::Typewriter::new();
            *stream_end.borrow_mut() = None;
            stream_start.set(usize::MAX);
            let _ = tx_turn.send(UiCmd::SysChatSend {
                text,
                atts: atts_now,
                cancel: token,
                inject_rx,
                cap_tx,
            });
        };
        Rc::new(f)
    };
    let (bot_tx, bot_rx) =
        std_mpsc::channel::<(i32, std::result::Result<(String, String), String>)>();
    let (wx_tx, wx_rx) = std_mpsc::channel::<WxEvt>();
    // 依赖安装 / 供应商测试结果（后台 → 主线程）
    let (dep_tx, dep_rx) = std_mpsc::channel::<DepEvt>();
    let (prov_tx, prov_rx) = std_mpsc::channel::<(i32, std::result::Result<String, String>)>();
    // 虚拟 Bot 操作结果（创建/编辑/登记/解散/预填）
    let (vb_tx, vb_rx) = std_mpsc::channel::<VirtualBotEvt>();
    // #141 一键创建团队结果（生成/建群）
    let (team_tx, team_rx) = std_mpsc::channel::<TeamEvt>();
    // 系统级对话回合结果（后台 → 主线程；气泡与对话框共用）
    let (syschat_tx, syschat_rx) = std_mpsc::channel::<SysChatEvt>();
    // #141 团队弹窗上下文：打开时记录 (bot 下标, bot_key)；生成/建群按下标从工作副本取最新 bot。
    let team_ctx: Rc<RefCell<Option<(i32, String)>>> = Rc::new(RefCell::new(None));
    // #147 任命弹窗上下文：打开时记录 (bot_key, 团队名, 角色名)；确认时按它写团队登记表。
    let appoint_ctx: Rc<RefCell<Option<(String, String, String)>>> = Rc::new(RefCell::new(None));
    // #141 当前生成的方案 JSON（确认建群时复用，避免二次生成）。
    let team_plan: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    // 虚拟 Bot 弹窗的归属 bot 上下文（bot 下标, bot_key, kind）——打开时记录，
    // 点「创建」时按下标从工作副本取最新 app_id/app_secret（用户可能刚改过没保存）。
    let vb_ctx: Rc<RefCell<Option<(i32, String, String)>>> = Rc::new(RefCell::new(None));
    // #125 编辑弹窗异步预填状态（见 VbEditState）：打开编辑时重置，Fetched 回填推进。
    let vb_edit: Rc<RefCell<VbEditState>> = Rc::new(RefCell::new(VbEditState::default()));
    // 确认弹窗待执行操作（确认回调 take；取消/关闭清空）
    let vb_action: Rc<RefCell<Option<VbAction>>> = Rc::new(RefCell::new(None));
    // #147 解散团队待执行操作（与 vb_action 互斥共用 vb_confirm 弹窗）
    let team_action: Rc<RefCell<Option<TeamAction>>> = Rc::new(RefCell::new(None));
    // 确认弹窗：取消 = 清空待执行操作并隐藏
    {
        let cw = vb_confirm.as_weak();
        let action = vb_action.clone();
        vb_confirm.on_canceled(move || {
            action.borrow_mut().take();
            if let Some(c) = cw.upgrade() {
                // 子窗口关闭不动 dock（8-20 用户反馈）
                let _ = c.hide();
            }
        });
    }
    // 解散团队取消：清空待操作（与 vb_action 互斥，共用同一确认弹窗）
    {
        let cw = vb_confirm.as_weak();
        let team_action = team_action.clone();
        vb_confirm.on_canceled(move || {
            team_action.borrow_mut().take();
            if let Some(c) = cw.upgrade() {
                let _ = c.hide();
            }
        });
    }
    // 确认弹窗：确认 = 执行待操作（按 VbAction / TeamAction 分发到后台线程）。
    // 执行中不关窗（busy）：结果由 VirtualBotEvt::Done / TeamEvt::Manage 回填——
    // 成功才关，失败在弹窗里显示错误（8-20 用户反馈"点了窗口就关了但群还在"——
    // 失败必须可见，不能静默）。
    {
        let cw = vb_confirm.as_weak();
        let action = vb_action.clone();
        let team_action = team_action.clone();
        let tx = tx.clone();
        vb_confirm.on_confirmed(move || {
            crate::log!("[gui] 确认弹窗：确认");
            // clone 而非 take（8-20 用户反馈：失败态「重试」需要保留 action 重发；
            // 成功后在 Done / Manage 里 take 清空）
            if let Some(a) = action.borrow().clone() {
                // 有 action：执行中（busy），**不关窗**——结果由 Done 回填（成功态
                // 才可点「知道了」关闭；失败态「重试」保留 action）。曾残留旧 hide()
                // 导致确认后立即关窗、结果回填到隐藏窗口（8-20 用户实测"还是立刻关了"）。
                if let Some(c) = cw.upgrade() {
                    c.set_busy(true);
                    c.set_failed(false);
                    c.set_message("执行中…".into());
                }
                let _ = tx.send(match a {
                    VbAction::Deregister { bot_key, chat_id } => {
                        UiCmd::VirtualBotDeregister { bot_key, chat_id }
                    }
                    VbAction::Disband {
                        bot_key,
                        kind,
                        app_id,
                        app_secret,
                        chat_id,
                        owner,
                    } => UiCmd::VirtualBotDisband {
                        bot_key,
                        kind,
                        app_id,
                        app_secret,
                        chat_id,
                        owner,
                    },
                });
            } else if let Some(ta) = team_action.borrow().clone() {
                // #147 解散团队：结果经 TeamEvt::Manage 回填（成功才关，失败可重试）
                if let Some(c) = cw.upgrade() {
                    c.set_busy(true);
                    c.set_failed(false);
                    c.set_message("正在解散团队…".into());
                }
                let _ = tx.send(match ta {
                    TeamAction::Dissolve { bot_key, team_name } => {
                        UiCmd::TeamDissolve { bot_key, team_name }
                    }
                });
            } else {
                // 无 action（成功态点「知道了」）：关窗，不动 dock（8-20 用户反馈）
                if let Some(c) = cw.upgrade() {
                    let _ = c.hide();
                }
            }
        });
    }

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
    // 供应商工作副本 + model + 全局默认名工作副本（#80：除 bots 外的 work 全部
    // 聚合进 SettingsWork，load/snapshot 签名不再随字段增长）
    let providers_model: Rc<slint::VecModel<ProviderRow>> = Rc::new(slint::VecModel::default());
    let wk = Rc::new(SettingsWork {
        providers: Rc::new(RefCell::new(Vec::new())),
        default_provider: Rc::new(RefCell::new(String::new())),
        cross_delivery: Rc::new(RefCell::new(false)),
        templates: Rc::new(RefCell::new(Vec::new())),
        history_retention: Rc::new(RefCell::new(30)),
        notify: Rc::new(RefCell::new(true)),
        session_gc: Rc::new(RefCell::new(false)),
        session_gc_days: Rc::new(RefCell::new(7)),
        jev: Rc::new(RefCell::new(crate::config::JevConfig::default())),
    });
    // 历史记录页：会话列表 + 选中会话的消息流双 model（2s 轮询整体替换）+
    // 选中会话快照（轮询期消息新增时标题/条数随 tick 更新，消息流重查）。
    let sessions_model: Rc<slint::VecModel<ChatSessionRow>> = Rc::new(slint::VecModel::default());
    settings.set_chat_sessions(slint::ModelRc::from(sessions_model.clone()));
    let msgs_model: Rc<slint::VecModel<ChatMsgRow>> = Rc::new(slint::VecModel::default());
    settings.set_history_msgs(slint::ModelRc::from(msgs_model.clone()));
    let selected_chat: Rc<RefCell<Option<crate::msgstore::ChatStats>>> =
        Rc::new(RefCell::new(None));
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

    // 打开设置窗时装载 config → 各工作副本 + model（#80：非 bots 副本聚合在 wk 内）
    fn load_into(
        w: &SettingsWindow,
        c: &Config,
        work: &RefCell<Vec<BotConfig>>,
        model: &slint::VecModel<BotRow>,
        providers_model: &slint::VecModel<ProviderRow>,
        wk: &SettingsWork,
    ) {
        *work.borrow_mut() = c.bots.clone();
        sync_model(model, &c.bots);
        *wk.providers.borrow_mut() = c.providers.clone();
        *wk.default_provider.borrow_mut() = c.default_provider.clone();
        *wk.cross_delivery.borrow_mut() = c.cross_delivery_enabled;
        w.set_cross_delivery_enabled(c.cross_delivery_enabled);
        // 虚拟 Bot 自定义角色模板（#75）：工作副本装载；登记列表按选中 bot 刷新
        *wk.templates.borrow_mut() = c.custom_roles.clone();
        // #74 历史记录页工作副本（保留期/提醒开关，随保存走既有重启链路）
        *wk.history_retention.borrow_mut() = c.history_retention_days;
        w.set_history_retention_days(c.history_retention_days as i32);
        *wk.notify.borrow_mut() = c.notify_enabled;
        w.set_notify_enabled(c.notify_enabled);
        // #78 会话归纳清理：工作副本装载 + 控件状态回写
        *wk.session_gc.borrow_mut() = c.session_gc_enabled;
        w.set_session_gc_enabled(c.session_gc_enabled);
        *wk.session_gc_days.borrow_mut() = c.session_gc_days;
        w.set_session_gc_days(c.session_gc_days as i32);
        // Jev 决策模型：工作副本装载 + 控件状态（key 不回显，只回显 base_url/model）。
        *wk.jev.borrow_mut() = c.jev.clone();
        w.set_jev_api_key("".into()); // 密码框不回显已存 key
        w.set_jev_base_url(c.jev.base_url.clone().into());
        w.set_jev_model(c.jev.model.clone().into());
        sync_providers_model(providers_model, &c.providers, &c.default_provider);
        w.set_provider_names(slint::ModelRc::from(Rc::new(slint::VecModel::from(
            build_provider_names(&c.providers),
        ))));
        // Owner、供应商 都是 per-bot（bots[i].owner_open_id / .provider）
        w.set_selected(if c.bots.is_empty() { -1 } else { 0 });
        w.set_provider_selected(if c.providers.is_empty() { -1 } else { 0 });
        refresh_bot_panel(w, work);
        w.set_status_line("".into());
        // 依赖检测：node/python3/lark-cli/dingtalk-cli/git 是否在本机可执行路径上。
        push_deps_to_window(w);
        // 系统权限检测（macOS）：完全磁盘/辅助功能/屏幕录制/自动化。
        push_perms_to_window(w);
    }

    /// 装载设置窗：发现比正式配置新的草稿 → 静默恢复（返回 true，标记 dirty 并给一行提示）；
    /// 否则按正式配置装载（返回 false）。「静默恢复」= 不弹选择框，直接把草稿当工作底稿。
    fn load_with_draft(
        w: &SettingsWindow,
        dirty: &Cell<bool>,
        work: &RefCell<Vec<BotConfig>>,
        model: &slint::VecModel<BotRow>,
        providers_model: &slint::VecModel<ProviderRow>,
        wk: &SettingsWork,
    ) -> bool {
        if Config::draft_is_newer() {
            let draft = Config::load_draft().unwrap_or_default();
            load_into(w, &draft, work, model, providers_model, wk);
            dirty.set(true);
            w.set_status_is_error(false);
            w.set_status_line("已恢复上次未保存的草稿（编辑后点「保存」写入配置）".into());
            true
        } else {
            let cfg = Config::load().unwrap_or_default();
            load_into(w, &cfg, work, model, providers_model, wk);
            dirty.set(false);
            false
        }
    }

    // ── 后台 tokio 线程：处理慢操作（HTTP/起停），结果回主线程 ──
    let tray_weak_bg = tray.as_weak();
    // 评审 R22 的 P8c + 2026-09-29 owner 实报（Windows「停不了/重启不了也没提示」）：
    // **启动/停止/重启三种服务动作失败都必须看得见**。`slint::Timer`/`Rc<Cell<_>>` 都不是
    // Send，不能进后台线程 —— 这里只跨线程传字符串，由 UI 线程的 2s 定时器负责弹出
    // （复用未读提醒那套 toast + 5s 自动收起）。
    let svc_toast: std::sync::Arc<std::sync::Mutex<Option<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let svc_toast_bg = svc_toast.clone();
    // 系统级对话回合结果发送端（气泡/对话框共用）
    let syschat_tx_bg = syschat_tx.clone();
    // 版本升级：最近一次检查发现的可用新版本（CheckUpdate 写，InstallUpdate 读）
    let latest_rel =
        std::sync::Arc::new(std::sync::Mutex::new(None::<crate::updater::LatestRelease>));
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(async move {
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    UiCmd::Start => {
                        // 起没起来要确认：起完即退（单实例锁被占）在旧实现里是静默的。
                        if let Err(e) =
                            install::svc_start_verified(std::time::Duration::from_secs(5))
                        {
                            crate::log!("[watchdog] 启动服务未完成：{e:#}");
                            if let Ok(mut slot) = svc_toast_bg.lock() {
                                *slot = Some(svc_action_toast("启动未完成", &e));
                            }
                        }
                        refresh(&tray_weak_bg);
                    }
                    UiCmd::Stop => {
                        // 本批起「停止」需要授权（owner：服务不能被随便杀死）。
                        if let Err(e) = install::svc_stop_authorized() {
                            crate::log!("[watchdog] 停止服务未完成：{e:#}");
                            // 交给 UI 线程弹 toast（见上面的 svc_toast 说明）。
                            if let Ok(mut slot) = svc_toast_bg.lock() {
                                *slot = Some(svc_action_toast("停止未完成", &e));
                            }
                        }
                        refresh(&tray_weak_bg);
                    }
                    UiCmd::Restart => {
                        // 旧实现只写日志不弹提示 ⇒ Windows 上「点了重启没反应」查无实据。
                        if let Err(e) = install::svc_restart() {
                            crate::log!("[watchdog] 重启服务未完成：{e:#}");
                            if let Ok(mut slot) = svc_toast_bg.lock() {
                                *slot = Some(svc_action_toast("重启未完成", &e));
                            }
                        }
                        refresh(&tray_weak_bg);
                    }
                    UiCmd::OpenLogs => platform::open_path(&crate::bridge_dir().join("logs")),
                    UiCmd::OpenFolder => platform::open_path(&crate::bridge_dir()),
                    UiCmd::Save(cfg) => {
                        // 热切换：保存前快照盘上旧配置，save 后比对「结构/凭证签名」——
                        // 只有 bot 增删/启停/kind/凭证变化才重启；backend/provider/model
                        // 等运行期字段 service 每消息热读，保存即生效（免重启几秒断连）。
                        let prev = Config::load().unwrap_or_default();
                        let res = cfg.save();
                        if res.is_ok() {
                            Config::remove_draft(); // 正式配置已落盘，草稿作废
                        }
                        if res.is_ok()
                            && install::status().running
                            && bots_struct_sig(&prev) != bots_struct_sig(&cfg)
                        {
                            // 后台热重启：失败写日志即可（配置已保存；服务没起来时看门狗会再拉）。
                            if let Err(e) = install::svc_restart() {
                                crate::log!("[watchdog] 保存后热重启失败：{e:#}");
                            }
                        }
                        // 接入飞书 bot → 后台自动装 lark-cli + lark 技能（幂等/best-effort）。
                        // GUI 路径免等 service 重启；装不上只 log 警告。只对飞书 bot 触发。
                        if res.is_ok() && cfg.bots.iter().any(|b| b.enabled && b.kind == "feishu") {
                            // #69 审计：GUI（托盘）进程侧短命任务（装完即收尾），
                            // 与 service 进程不同生命周期，不登记。
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
                        // #60：与一键装同款 spawn 解耦——recv 循环是单消费者串行处理所有
                        // UiCmd，inline await 安装会把托盘 Start/Stop 阻塞数分钟。
                        // 鲁棒性（用户反馈）：panic 防护——安装内部异常也要回结果，
                        // 否则 dep-busy 永久卡死、安装按钮全部禁用至重启。
                        let dep_tx = dep_tx.clone();
                        // #69 审计：GUI 进程侧短命任务（安装收尾经 DepEvt 回主线程），不登记。
                        tokio::spawn(async move {
                            let r = futures_util::FutureExt::catch_unwind(
                                std::panic::AssertUnwindSafe(crate::deps::run_install(&dep_id)),
                            )
                            .await
                            .map_err(|_| "安装内部错误（异常中断）".to_string())
                            .and_then(|r| r);
                            let _ = dep_tx.send(DepEvt::Done { dep_id, result: r });
                        });
                    }
                    UiCmd::InstallAllMissing => {
                        let dep_tx = dep_tx.clone();
                        // #69 审计：GUI 进程侧短命任务（AllDone 回主线程），不登记。
                        tokio::spawn(async move {
                            let outcome = futures_util::FutureExt::catch_unwind(
                                std::panic::AssertUnwindSafe(crate::deps::install_all_missing(
                                    |evt| {
                                        let _ = dep_tx.send(DepEvt::AllProgress {
                                            label: evt.label,
                                            idx: evt.idx,
                                            total: evt.total,
                                        });
                                    },
                                )),
                            )
                            .await
                            .unwrap_or_else(|_| {
                                // panic → 全失败单条汇总（如实呈现而非卡死）
                                let mut o = crate::deps::AllInstallOutcome::default();
                                o.failed.push((
                                    "内部错误".to_string(),
                                    "安装过程异常中断（已记录日志）".to_string(),
                                ));
                                o
                            });
                            let _ = dep_tx.send(DepEvt::AllDone(outcome));
                        });
                    }
                    UiCmd::TestProvider { idx, snapshot } => {
                        let r = test_provider(&snapshot).await;
                        let _ = prov_tx.send((idx, r));
                    }
                    // ── 版本升级：检查（GitHub latest release）──
                    UiCmd::CheckUpdate { silent } => {
                        let tw = tray_weak_bg.clone();
                        let latest = latest_rel.clone();
                        tokio::spawn(async move {
                            if !silent {
                                let tw2 = tw.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(t) = tw2.upgrade() {
                                        t.set_update_state(1); // 检查中
                                    }
                                });
                            }
                            let res = async {
                                let up = crate::updater::Updater::new()?;
                                up.check_latest().await
                            }
                            .await;
                            match res {
                                Ok(rel) => {
                                    let newer = crate::updater::is_newer(
                                        &rel.version,
                                        crate::updater::CURRENT,
                                    );
                                    if newer {
                                        // 升级链路的日志一律走 updater::log_update：GUI 进程的
                                        // stdout 在 Windows（无控制台）与 macOS（open 拉起）都会
                                        // 蒸发，只有 logs/update.log 留得下（复核 reviewer-41 问题 1）。
                                        crate::updater::log_update(&format!(
                                            "[update] 发现新版本 v{}（当前 v{}）",
                                            rel.version,
                                            crate::updater::CURRENT
                                        ));
                                    }
                                    let ver = rel.version.clone();
                                    let can = rel.asset_url.is_some();
                                    if newer {
                                        *latest.lock().unwrap() = Some(rel);
                                    }
                                    let _ = slint::invoke_from_event_loop(move || {
                                        if let Some(t) = tw.upgrade() {
                                            t.set_update_version(ver.into());
                                            t.set_update_can_install(can);
                                            t.set_update_state(if newer { 3 } else { 2 });
                                        }
                                    });
                                }
                                Err(e) => {
                                    // 静默检查（启动/周期）失败不打扰：保持原状态
                                    crate::updater::log_update(&format!(
                                        "[update] 检查失败{}：{e:#}",
                                        if silent { "（静默）" } else { "" }
                                    ));
                                    if !silent {
                                        let _ = slint::invoke_from_event_loop(move || {
                                            if let Some(t) = tw.upgrade() {
                                                t.set_update_state(6);
                                            }
                                        });
                                    }
                                }
                            }
                        });
                    }
                    // ── 版本升级：下载 + 安装 + 退出（新版由分离脚本/安装器拉起）──
                    UiCmd::InstallUpdate => {
                        let rel = latest_rel.lock().unwrap().clone();
                        let tw = tray_weak_bg.clone();
                        tokio::spawn(async move {
                            let Some(rel) = rel else { return };
                            let Some(url) = rel.asset_url.clone() else {
                                return;
                            };
                            let tw2 = tw.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(t) = tw2.upgrade() {
                                    t.set_update_progress("连接中…".into());
                                    t.set_update_state(4); // 下载安装中
                                }
                            });
                            let dest = crate::updater::download_dest(&rel.version);
                            // 进度回调：按整百分比（无总长则按整 MB）节流，变化才推给主线程重建菜单
                            let last_tick =
                                std::sync::Arc::new(std::sync::atomic::AtomicI64::new(-1));
                            let twp = tw.clone();
                            let on_progress = move |done: u64, total: Option<u64>| {
                                let (label, tick) = match total {
                                    Some(t) if t > 0 => {
                                        let pct = done.saturating_mul(100) / t;
                                        (
                                            format!(
                                                "{pct}%（{:.1}/{:.1} MB）",
                                                done as f64 / 1e6,
                                                t as f64 / 1e6
                                            ),
                                            pct as i64,
                                        )
                                    }
                                    _ => (
                                        format!("已下载 {:.1} MB", done as f64 / 1e6),
                                        (done / 1_000_000) as i64,
                                    ),
                                };
                                if tick == last_tick.load(std::sync::atomic::Ordering::Relaxed) {
                                    return;
                                }
                                last_tick.store(tick, std::sync::atomic::Ordering::Relaxed);
                                let tw3 = twp.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(t) = tw3.upgrade() {
                                        t.set_update_progress(label.into());
                                    }
                                });
                            };
                            let r = async {
                                let up = crate::updater::Updater::new()?;
                                up.download_to(&url, &dest, &on_progress).await?;
                                // 安装前完整性校验（fail-closed：release 无校验清单也拒绝装）
                                crate::updater::verify_sha256(
                                    &dest,
                                    &crate::updater::asset_file_name(&rel.version),
                                    rel.asset_sha256.as_deref(),
                                )?;
                                let tw4 = tw.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(t) = tw4.upgrade() {
                                        t.set_update_progress("正在安装…".into());
                                    }
                                });
                                crate::updater::install_and_relaunch(&dest)
                            }
                            .await;
                            match r {
                                Ok(()) => {
                                    crate::updater::log_update(&format!(
                                        "[update] v{} 已启动安装包（更新器拿不到安装器退出码；成败看 %TEMP%\\Setup Log *.txt），本进程退出让安装器换文件",
                                        rel.version
                                    ));
                                    let _ = slint::invoke_from_event_loop(move || {
                                        // 用 keep_desired：这次停是为了让安装器换文件，不是
                                        // 「用户不要 bridge 了」。清掉标记的话，重启后的新实例
                                        // 看门狗不会把 service 拉回来 —— 升级完看着像「ABB 没运行」。
                                        // 停不掉也要继续走安装（装完会重启），但必须留痕。
                                        if let Err(e) = install::svc_stop_keep_desired() {
                                            crate::log!(
                                                "[watchdog] 升级前停 service 失败（继续安装）：{e:#}"
                                            );
                                        }
                                        let _ = slint::quit_event_loop();
                                    });
                                }
                                Err(e) => {
                                    // 这条尤其重要：macOS 侧 macos_install 的多个 bail!（hdiutil
                                    // attach / ditto / 重启辅助脚本）只在这里留下原因文本。
                                    crate::updater::log_update(&format!("[update] 安装失败：{e:#}"));
                                    let _ = slint::invoke_from_event_loop(move || {
                                        if let Some(t) = tw.upgrade() {
                                            // 明确的失败态（不是退回"升级"，避免看起来像没发生过）
                                            t.set_update_state(5);
                                        }
                                    });
                                }
                            }
                        });
                    }
                    // ── 虚拟 Bot（#75）：群管理操作。后台现造平台客户端（与
                    // FetchBotInfo 同路径：GUI 进程自己调 API，不依赖 service）。
                    // 结果统一经 vb_tx 回主线程（进度 + 逐项汇总）。
                    UiCmd::VirtualBotCreate {
                        bot_key,
                        kind,
                        app_id,
                        app_secret,
                        owner,
                        items,
                    } => {
                        let vb_tx = vb_tx.clone();
                        tokio::spawn(async move {
                            let total = items.len();
                            let mut results = Vec::with_capacity(total);
                            // 客户端复用（tenant_token 是实例内缓存）：批量建群只取一次
                            // token，不用每群现造。kind 已固定，非 dingtalk 才需要 feishu。
                            let feishu =
                                (kind != "dingtalk").then(|| FeishuClient::new(&app_id, &app_secret));
                            for (i, (name, prompt)) in items.into_iter().enumerate() {
                                let _ = vb_tx.send(VirtualBotEvt::Progress { done: i, total });
                                let r = async {
                                    match kind.as_str() {
                                        "dingtalk" => DingTalkClient::new(&app_id, &app_secret)
                                            .create_chat(&name, &prompt)
                                            .await
                                            .map_err(|e| format!("{e:#}")),
                                        _ => {
                                            // 建群必须带 owner：群里只有机器人时用户飞书
                                            // 客户端看不到群（8-20 实测）。owner 设为群主 +
                                            // bot 管理员（set_bot_manager），用户才有编辑
                                            // 群名/介绍权限（平台为准的核心交互）。
                                            // owner 在点击时已从工作副本解析（与 app_id/
                                            // secret 同一 bot 快照）；缺失 = 配置未填 owner。
                                            let owner = owner
                                                .as_deref()
                                                .ok_or_else(|| {
                                                    "该 bot 未配置 owner_open_id（群主）：虚拟 Bot 建群"
                                                        .to_string()
                                                        + "必须有群主，否则群里只有机器人、用户飞书客户端看不到群。"
                                                        + "请在 bot 设置里填写 owner 白名单后重试，"
                                                        + "或手动建群后走「手动登记」。"
                                                })?;
                                            feishu
                                                .as_ref()
                                                .expect("非 dingtalk 分支必有 feishu client")
                                                .create_chat(&name, &prompt, owner)
                                                .await
                                                .map_err(|e| format!("{e:#}"))
                                        }
                                    }
                                }
                                .await;
                                match r {
                                    Ok(chat_id) => {
                                        // 群已建 → 写登记表（群名=角色名）
                                        let reg = VirtualBot {
                                            bot_key: bot_key.clone(),
                                            chat_id: chat_id.clone(),
                                            role_name: name.clone(),
                                            created_at: crate::chrono_lite::unix_secs(),
                                        };
                                        match VirtualBotStore::new().add(reg) {
                                            Ok(()) => results.push((
                                                name,
                                                Ok("已创建并登记".to_string()),
                                            )),
                                            Err(e) => results.push((
                                                name,
                                                Err(format!(
                                                    "群已创建（chat_id={chat_id}）但登记失败：{e}，请用「手动登记」补登"
                                                )),
                                            )),
                                        }
                                    }
                                    Err(e) => {
                                        // 钉钉建群大概率失败（能力边界）：错误里带降级提示
                                        let e = if kind == "dingtalk" {
                                            format!(
                                                "{e}（钉钉建群能力待实测：可手动建群后走「手动登记」）"
                                            )
                                        } else {
                                            e
                                        };
                                        // 飞书权限不足：给 owner 私聊发授权指引（可执行下一步）
                                        let mut notified = false;
                                        if kind == "feishu" {
                                            if let (Some(owner), Some((scopes, link))) =
                                                (&owner, crate::feishu::scope_hint(&e))
                                            {
                                                let fs =
                                                    FeishuClient::new(&app_id, &app_secret);
                                                let msg = format!(
                                                    "⚠️ 虚拟 Bot 操作需要飞书授权\n\
                                                     操作「创建虚拟 Bot」被拒绝：应用缺少权限 {scopes}\n\
                                                     请点击开通（任选其一）：\n{link}\n\
                                                     开通后重新操作即可。"
                                                );
                                                crate::log!(
                                                    "[gui] 创建失败权限不足，向 owner 发送授权指引"
                                                );
                                                if fs.send_text_to_user(owner, &msg).await.is_ok()
                                                {
                                                    notified = true;
                                                }
                                            }
                                        }
                                        results.push((
                                            name,
                                            Err(if notified {
                                                format!("{e}（已向 owner 发送授权指引）")
                                            } else {
                                                e
                                            }),
                                        ));
                                    }
                                }
                            }
                            let _ = vb_tx.send(VirtualBotEvt::Done { results });
                        });
                    }
                    UiCmd::VirtualBotUpdate {
                        bot_key,
                        kind,
                        app_id,
                        app_secret,
                        chat_id,
                        name,
                        prompt,
                        owner,
                    } => {
                        let vb_tx = vb_tx.clone();
                        tokio::spawn(async move {
                            let r = async {
                                match kind.as_str() {
                                    "dingtalk" => DingTalkClient::new(&app_id, &app_secret)
                                        .update_chat(&chat_id, &name, &prompt)
                                        .await
                                        .map_err(|e| format!("{e:#}")),
                                    _ => FeishuClient::new(&app_id, &app_secret)
                                        .update_chat(&chat_id, &name, &prompt)
                                        .await
                                        .map_err(|e| format!("{e:#}")),
                                }
                            }
                            .await;
                            let mut result = match r {
                                Ok(()) => match VirtualBotStore::new().update_role(
                                    &bot_key,
                                    &chat_id,
                                    &name,
                                ) {
                                    Ok(()) => Ok("已更新（平台 + 登记同步）".to_string()),
                                    Err(e) => Err(format!("平台已更新，登记同步失败：{e}")),
                                },
                                Err(e) => Err(e),
                            };
                            // 权限不足（飞书 99991672）：给 owner 私聊发授权指引
                            if let (Err(e), Some(owner)) = (&result, &owner) {
                                if kind == "feishu" {
                                    if let Some((scopes, link)) = crate::feishu::scope_hint(e) {
                                        let fs = FeishuClient::new(&app_id, &app_secret);
                                        let msg = format!(
                                            "⚠️ 虚拟 Bot 操作需要飞书授权\n\
                                             操作「编辑群资料」被拒绝：应用缺少权限 {scopes}\n\
                                             请点击开通（任选其一）：\n{link}\n\
                                             开通后重新操作即可。"
                                        );
                                        crate::log!("[gui] 编辑失败权限不足，向 owner 发送授权指引");
                                        match fs.send_text_to_user(owner, &msg).await {
                                            Ok(()) => {
                                                result = Err(format!(
                                                    "{e}（已向 owner 发送授权指引）"
                                                ));
                                            }
                                            Err(se) => crate::log!(
                                                "[gui] 向 owner 发送授权指引失败: {se:#}"
                                            ),
                                        }
                                    }
                                }
                            }
                            let _ = vb_tx.send(VirtualBotEvt::Done {
                                results: vec![(name, result)],
                            });
                        });
                    }
                    UiCmd::VirtualBotRegister {
                        bot_key,
                        name,
                        chat_id,
                    } => {
                        let vb_tx = vb_tx.clone();
                        tokio::spawn(async move {
                            let reg = VirtualBot {
                                bot_key,
                                chat_id: chat_id.clone(),
                                role_name: name.clone(),
                                created_at: crate::chrono_lite::unix_secs(),
                            };
                            let result = match VirtualBotStore::new().add(reg) {
                                Ok(()) => Ok("已登记".to_string()),
                                Err(e) => Err(e),
                            };
                            let _ = vb_tx.send(VirtualBotEvt::Done {
                                results: vec![(name, result)],
                            });
                        });
                    }
                    UiCmd::VirtualBotDeregister { bot_key, chat_id } => {
                        let vb_tx = vb_tx.clone();
                        tokio::spawn(async move {
                            crate::log!(
                                "[gui] 取消登记 bot={} chat={}",
                                bot_key,
                                crate::agent::truncate(&chat_id, 12)
                            );
                            let store = VirtualBotStore::new();
                            // 结果行的名字用角色名（比 chat_id 可读）；查不到才回落 id
                            let role = store
                                .load()
                                .into_iter()
                                .find(|v| v.bot_key == bot_key && v.chat_id == chat_id)
                                .map(|v| v.role_name)
                                .unwrap_or_else(|| chat_id.clone());
                            let ok = store.remove(&bot_key, &chat_id);
                            // #147 双向一致：取消登记 → 团队条目对应角色 chat_id 清空（状态转「部分失败」）
                            crate::teamreg::TeamStore::new().clear_chat(&bot_key, &chat_id);
                            let result = if ok {
                                Ok("已取消登记（群保留）".to_string())
                            } else {
                                Err("该群不在登记表里（可能已被移除）".to_string())
                            };
                            crate::log!(
                                "[gui] 取消登记结果: {}",
                                if ok { "成功" } else { "失败（不在登记表）" }
                            );
                            let _ = vb_tx.send(VirtualBotEvt::Done {
                                results: vec![(role, result)],
                            });
                        });
                    }
                    UiCmd::VirtualBotDisband {
                        bot_key,
                        kind,
                        app_id,
                        app_secret,
                        chat_id,
                        owner,
                    } => {
                        let vb_tx = vb_tx.clone();
                        tokio::spawn(async move {
                            let store = VirtualBotStore::new();
                            let role = store
                                .load()
                                .into_iter()
                                .find(|v| v.bot_key == bot_key && v.chat_id == chat_id)
                                .map(|v| v.role_name)
                                .unwrap_or_else(|| chat_id.clone());
                            // 先解散平台群（不可恢复；确认弹窗已挡过一次），成功再移除登记
                            crate::log!(
                                "[gui] 解散群 bot={} kind={} chat={}",
                                bot_key,
                                kind,
                                crate::agent::truncate(&chat_id, 12)
                            );
                            let r = async {
                                match kind.as_str() {
                                    "dingtalk" => Err("钉钉暂无解散群 API".to_string()),
                                    _ => FeishuClient::new(&app_id, &app_secret)
                                        .delete_chat(&chat_id)
                                        .await
                                        .map_err(|e| format!("{e:#}")),
                                }
                            }
                            .await;
                            let mut result = match r {
                                Ok(()) => {
                                    store.remove(&bot_key, &chat_id);
                                    // #147 双向一致：解散群 → 团队条目对应角色 chat_id 清空
                                    crate::teamreg::TeamStore::new()
                                        .clear_chat(&bot_key, &chat_id);
                                    // 解散成功同样归档会话历史（与事件/刷新路径一致——
                                    // 8-20 用户追问后补：三路径统一，历史移入 archive/）
                                    let archived =
                                        VirtualBotStore::archive_chat_history(&bot_key, &chat_id);
                                    Ok(if archived > 0 {
                                        format!("群已解散，登记已移除，历史已归档（{archived} 个文件）")
                                    } else {
                                        "群已解散，登记已移除".to_string()
                                    })
                                }
                                Err(e) => {
                                    // 232017（操作者非群主/管理员）：8-20 实测——群主
                                    // 转让给 owner 后 bot 降级普通成员，解散被拒。附可执行
                                    // 指引（飞书里把机器人设为管理员，或把群主转回机器人）。
                                    let es = e.to_string();
                                    if es.contains("232017") {
                                        Err(format!(
                                            "解散失败（登记保留）：机器人不是该群群主/管理员。请在飞书群设置里把机器人设为管理员（或把群主转回机器人）后重试。{es}"
                                        ))
                                    } else {
                                        Err(format!("解散失败（登记保留）：{e}"))
                                    }
                                }
                            };
                            // 权限不足（飞书 99991672）：给 owner 私聊发授权指引——平台权限
                            // 问题不该只躺在状态行/日志里，owner 需要可执行的下一步。
                            if let Err(e) = &result {
                                if let Some(owner) = &owner {
                                    if kind == "feishu" {
                                        if let Some((scopes, link)) =
                                            crate::feishu::scope_hint(e)
                                        {
                                            let fs = FeishuClient::new(&app_id, &app_secret);
                                            let msg = format!(
                                                "⚠️ 虚拟 Bot 操作需要飞书授权\n\
                                                 操作「解散群」被拒绝：应用缺少权限 {scopes}\n\
                                                 请点击开通（任选其一）：\n{link}\n\
                                                 开通后重新操作即可。"
                                            );
                                            crate::log!("[gui] 权限不足，向 owner 发送授权指引");
                                            match fs.send_text_to_user(owner, &msg).await {
                                                Ok(()) => {
                                                    result = Err(format!(
                                                        "解散失败（登记保留）：{e}（已向 owner 发送授权指引）"
                                                    ));
                                                }
                                                Err(se) => crate::log!(
                                                    "[gui] 向 owner 发送授权指引失败: {se:#}"
                                                ),
                                            }
                                        }
                                    }
                                }
                            }
                            crate::log!(
                                "[gui] 解散群结果: {}",
                                match &result {
                                    Ok(s) => s,
                                    Err(e) => e,
                                }
                            );
                            let _ = vb_tx.send(VirtualBotEvt::Done {
                                results: vec![(role, result)],
                            });
                        });
                    }
                    UiCmd::VirtualBotFetchInfo {
                        kind,
                        app_id,
                        app_secret,
                        chat_id,
                    } => {
                        let vb_tx = vb_tx.clone();
                        tokio::spawn(async move {
                            let r = async {
                                match kind.as_str() {
                                    "dingtalk" => DingTalkClient::new(&app_id, &app_secret)
                                        .get_chat_info(&chat_id)
                                        .await
                                        .map_err(|e| format!("{e:#}")),
                                    _ => FeishuClient::new(&app_id, &app_secret)
                                        .get_chat_info(&chat_id)
                                        .await
                                        .map_err(|e| format!("{e:#}")),
                                }
                            }
                            .await;
                            match r {
                                Ok((name, desc)) => {
                                    let _ = vb_tx.send(VirtualBotEvt::Fetched {
                                        chat_id,
                                        name,
                                        desc,
                                        error: None,
                                    });
                                }
                                Err(e) => {
                                    let _ = vb_tx.send(VirtualBotEvt::Fetched {
                                        chat_id,
                                        name: String::new(),
                                        desc: String::new(),
                                        error: Some(format!("{e:#}")),
                                    });
                                }
                            }
                        });
                    }
                    UiCmd::VirtualBotVerify {
                        bot_key,
                        kind,
                        app_id,
                        app_secret,
                    } => {
                        let vb_tx = vb_tx.clone();
                        tokio::spawn(async move {
                            // 逐个登记群验证存在性（get_chat_info）——平台解散群的兜底
                            // （im.chat.deleted 事件未订阅/丢失时）；确认已解散 → 移除
                            // 登记 + 归档会话历史。失败但错误不像"群不存在"（网络/权限/
                            // 频控）→ 保留登记并提示，避免误删。
                            crate::log!(
                                "[gui] 手动刷新虚拟 Bot 登记 bot={}",
                                crate::agent::truncate(&bot_key, 12)
                            );
                            let regs = VirtualBotStore::new().load_for(&bot_key);
                            let client = match kind.as_str() {
                                "dingtalk" => None,
                                _ => Some(FeishuClient::new(&app_id, &app_secret)),
                            };
                            let mut results: Vec<(String, Result<String, String>)> = Vec::new();
                            for reg in &regs {
                                let r = match &client {
                                    Some(c) => c.get_chat_info(&reg.chat_id).await,
                                    // 钉钉无群信息 API（无该能力）：跳过验证，登记保留
                                    None => Err(anyhow::anyhow!("钉钉不支持群验证")),
                                };
                                match r {
                                    Ok((name, _desc)) => {
                                        // #101：刷新同步群名——平台群名回写登记（群名=角色名约定）。
                                        // 平台侧改名后刷新即同步，deliver @新角色名 立即可用；
                                        // 重名冲突（update_role 拦截）/超长时保留旧名并提示，不静默改错。
                                        // 钉钉无群信息 API → 恒走“正常”。
                                        let name = name.trim().to_string();
                                        if name.is_empty() || name == reg.role_name {
                                            results.push((
                                                reg.role_name.clone(),
                                                Ok("正常".to_string()),
                                            ));
                                        } else if name.chars().count() > ROLE_NAME_MAX {
                                            results.push((
                                                reg.role_name.clone(),
                                                Err(format!(
                                                    "群名超长（{} 字 > {ROLE_NAME_MAX}），未同步（保留旧名）",
                                                    name.chars().count()
                                                )),
                                            ));
                                        } else {
                                            match VirtualBotStore::new().update_role(
                                                &bot_key,
                                                &reg.chat_id,
                                                &name,
                                            ) {
                                                Ok(_) => results.push((
                                                    reg.role_name.clone(),
                                                    Ok(format!(
                                                        "群名已同步：{} → {}",
                                                        reg.role_name, name
                                                    )),
                                                )),
                                                Err(e) => results.push((
                                                    reg.role_name.clone(),
                                                    Err(format!("群名未同步（保留旧名）：{e}")),
                                                )),
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        let es = format!("{e:#}");
                                        let gone = es.contains("不存在")
                                            || es.contains("解散")
                                            || es.contains("not found")
                                            || es.contains("404");
                                        if gone {
                                            let removed =
                                                VirtualBotStore::new().remove(&bot_key, &reg.chat_id);
                                            // #147 双向一致：平台解散刷新发现 → 团队角色 chat_id 清空
                                            crate::teamreg::TeamStore::new()
                                                .clear_chat(&bot_key, &reg.chat_id);
                                            let archived = VirtualBotStore::archive_chat_history(
                                                &bot_key,
                                                &reg.chat_id,
                                            );
                                            crate::log!(
                                                "[gui] 刷新发现群已解散：{}（登记移除={}，归档文件={}）",
                                                reg.role_name,
                                                removed,
                                                archived
                                            );
                                            results.push((
                                                reg.role_name.clone(),
                                                Err(format!(
                                                    "群已解散：登记已移除，历史已归档（{archived} 个文件）"
                                                )),
                                            ));
                                        } else {
                                            crate::log!(
                                                "[gui] 刷新验证失败（保留登记）：{}: {es}",
                                                reg.role_name
                                            );
                                            results.push((
                                                reg.role_name.clone(),
                                                Err(format!("验证失败（登记保留）：{es}")),
                                            ));
                                        }
                                    }
                                }
                            }
                            if regs.is_empty() {
                                results.push(("无".to_string(), Ok("暂无登记".to_string())));
                            }
                            let _ = vb_tx.send(VirtualBotEvt::Done { results });
                        });
                    }
                    UiCmd::GeneratePrompt { bot_key, name } => {
                        let vb_tx = vb_tx.clone();
                        tokio::spawn(async move {
                            crate::log!(
                                "[gui] 生成提示词 bot={} 角色={}",
                                crate::agent::truncate(&bot_key, 12),
                                crate::agent::truncate(&name, 20)
                            );
                            // 单后端化 P3.3：全 bot 收口 runtime oneshot（不再按 backend
                            // 分流 CLI spawn）；装配需要 bot 完整配置 + 全局 config。
                            let r = match Config::load() {
                                Ok(cfg) => match cfg.bots.iter().find(|b| b.key() == bot_key) {
                                    Some(bot) => {
                                        crate::agent::generate_role_prompt(bot, &cfg, &name).await
                                    }
                                    None => Err(anyhow::anyhow!("bot 配置不存在：{bot_key}")),
                                },
                                Err(e) => Err(anyhow::anyhow!("config.json 读取失败：{e:#}")),
                            };
                            match r {
                                Ok(text) => {
                                    let _ = vb_tx.send(VirtualBotEvt::PromptGenerated {
                                        text: Some(text),
                                        error: None,
                                    });
                                }
                                Err(e) => {
                                    let _ = vb_tx.send(VirtualBotEvt::PromptGenerated {
                                        text: None,
                                        error: Some(format!("{e:#}")),
                                    });
                                }
                            }
                        });
                    }
                    // #141 团队生成（单后端化 P3.4：oneshot 同步回合）：加载配置 → 找 bot →
                    // 生成方案后经 team_rx 回主线程（与 TeamCreate 同款的 cfg/bot 解析臂）。
                    UiCmd::TeamGenerate {
                        idx,
                        bot_key,
                        target,
                    } => {
                        let team_tx = team_tx.clone();
                        tokio::spawn(async move {
                            let r = async {
                                let cfg = Config::load()
                                    .map_err(|e| format!("读取配置失败：{e:#}"))?;
                                let bot = cfg
                                    .bots
                                    .iter()
                                    .find(|b| b.key() == bot_key)
                                    .cloned()
                                    .ok_or_else(|| "找不到该 bot 配置".to_string())?;
                                crate::teambuilder::generate_team_plan(
                                    &bot, &cfg, &target, &[], None,
                                )
                                .await
                            }
                            .await;
                            let _ = team_tx.send(TeamEvt::Generate {
                                result: r.map(|plan| {
                                    serde_json::to_string(&plan).unwrap_or_default()
                                }),
                            });
                            let _ = idx;
                        });
                    }
                    // #141 确认建群：messenger::build + teamflow::create_team_groups（幂等），
                    // 逐角色建群 + 登记，结果经 team_rx 回主线程。
                    UiCmd::TeamCreate {
                        idx,
                        bot_key,
                        plan,
                    } => {
                        let team_tx = team_tx.clone();
                        tokio::spawn(async move {
                            let r = async {
                                let plan: crate::teambuilder::TeamPlan =
                                    serde_json::from_str(&plan)
                                        .map_err(|e| format!("方案解析失败：{e}"))?;
                                let cfg = Config::load()
                                    .map_err(|e| format!("读取配置失败：{e:#}"))?;
                                let bot = cfg
                                    .bots
                                    .iter()
                                    .find(|b| b.key() == bot_key)
                                    .cloned()
                                    .ok_or_else(|| "找不到该 bot 配置".to_string())?;
                                let msgr =
                                    crate::messenger::build(&bot).map_err(|e| format!("{e:#}"))?;
                                // owner 平台 id（飞书建群必须拉进群；与 bridge/teamflow 同口径）
                                let owner = if bot.is_wechat() {
                                    bot.wx_user_id.clone()
                                } else if bot.is_dingtalk() {
                                    crate::config::first_owner_id(&bot.ding_owner_ids)
                                        .unwrap_or_default()
                                } else {
                                    crate::config::first_owner_id(&bot.owner_open_id)
                                        .unwrap_or_default()
                                };
                                if owner.is_empty() {
                                    return Err("该 bot 未配置 owner（群主）：建群必须有群主。请在 bot 设置里填写 owner 白名单后重试。"
                                        .to_string());
                                }
                                let outcomes = crate::teamflow::create_team_groups(
                                    msgr.as_ref(),
                                    &crate::virtualbot::VirtualBotStore::new(),
                                    &bot_key,
                                    &owner,
                                    &plan,
                                )
                                .await;
                                // #147：建群完成后登记团队（GUI ↔ 聊天入口同一份数据源；
                                // 部分成功也登记，重试时 register_created 合并补建成功的角色）
                                if outcomes.iter().any(|o| o.ok) {
                                    let store = crate::virtualbot::VirtualBotStore::new();
                                    let regs = crate::teamreg::role_regs_from_plan(
                                        &plan,
                                        &store,
                                        &bot_key,
                                    );
                                    if let Err(e) = crate::teamreg::TeamStore::new()
                                        .register_created(&bot_key, &plan.team_name, regs)
                                    {
                                        crate::log!(
                                            "[gui] 团队登记失败 bot={} team={}: {e}",
                                            bot_key,
                                            plan.team_name
                                        );
                                    }
                                }
                                let rows: Vec<(String, String, bool, String)> = outcomes
                                    .into_iter()
                                    .map(|o| (o.role_name, o.member, o.ok, o.detail))
                                    .collect();
                                Ok(rows)
                            }
                            .await;
                            let _ = team_tx.send(TeamEvt::Create { result: r });
                            let _ = idx;
                        });
                    }
                    // #147 任命成员：只写团队登记表（成员元数据；建群/寻址仍走 virtual-bots.json）
                    UiCmd::TeamAppoint {
                        bot_key,
                        team_name,
                        role_name,
                        member,
                    } => {
                        let team_tx = team_tx.clone();
                        tokio::spawn(async move {
                            let r = crate::teamreg::TeamStore::new()
                                .set_member(&bot_key, &team_name, &role_name, &member)
                                .map(|()| {
                                    if member.trim().is_empty() {
                                        format!("已恢复「{role_name}」为待任命")
                                    } else {
                                        format!("已任命 {member} 为「{role_name}」")
                                    }
                                });
                            let _ = team_tx.send(TeamEvt::Manage { result: r });
                        });
                    }
                    // #147 解散团队：逐个移除角色虚拟 Bot 登记 + 归档聊天历史 + 删团队条目。
                    // 平台群物理保留（钉钉无解散 API，飞书单群解散走虚拟 Bot 列表的「解散群」）。
                    UiCmd::TeamDissolve { bot_key, team_name } => {
                        let team_tx = team_tx.clone();
                        tokio::spawn(async move {
                            let r = async {
                                let store = crate::virtualbot::VirtualBotStore::new();
                                let team = crate::teamreg::TeamStore::new()
                                    .find(&bot_key, &team_name)
                                    .ok_or_else(|| "团队不存在（可能已解散）".to_string())?;
                                let mut removed = 0usize;
                                let mut archived = 0usize;
                                for role in &team.roles {
                                    if role.chat_id.is_empty() {
                                        continue;
                                    }
                                    if store.remove(&bot_key, &role.chat_id) {
                                        removed += 1;
                                    }
                                    archived += crate::virtualbot::VirtualBotStore::
                                        archive_chat_history(&bot_key, &role.chat_id);
                                }
                                if !crate::teamreg::TeamStore::new().remove(&bot_key, &team_name)
                                {
                                    return Err("团队条目移除失败（可能已被删除）".to_string());
                                }
                                Ok(format!(
                                    "团队「{team_name}」已解散：移除 {removed} 个角色登记，归档 {archived} 个聊天历史文件（平台群保留）"
                                ))
                            }
                            .await;
                            let _ = team_tx.send(TeamEvt::Manage { result: r });
                        });
                    }
                    // 系统级对话（气泡/对话框）：无需管理密码（它不是服务动作，且执行层
                    // 已是 owner 全权限——与其它 GUI 内部一次性回合同权限）。
                    UiCmd::SysChatSend {
                        text,
                        atts,
                        cancel,
                        inject_rx,
                        cap_tx,
                    } => {
                        let etx = syschat_tx_bg.clone();
                        tokio::spawn(async move {
                            let evt = match Config::load() {
                                Ok(cfg) => {
                                    // E1/E2：统一走流式入口；分片（正文与思考都带 kind）先经转发任务
                                    // 回主线程（`Delta`），回合结局再给 Reply/Stopped/Error。
                                    // **不新造第二条通道**：思考与正文共用同一个 sink，只是 kind 不同。
                                    let (delta_tx, mut delta_rx) =
                                        tokio::sync::mpsc::unbounded_channel::<
                                            crate::runtime::acp::TurnChunk,
                                        >();
                                    let etx_delta = etx.clone();
                                    let forwarder = tokio::spawn(async move {
                                        while let Some(chunk) = delta_rx.recv().await {
                                            let _ = etx_delta.send(SysChatEvt::Delta {
                                                kind: chunk.kind,
                                                text: chunk.text,
                                            });
                                        }
                                    });
                                    let end = crate::syschat::turn_streaming(
                                        &cfg,
                                        &text,
                                        &atts,
                                        delta_tx,
                                        cancel,
                                        Some(crate::runtime::oneshot::SteerWiring {
                                            cap_tx,
                                            inject_rx,
                                        }),
                                    )
                                    .await;
                                    // 回合已收尾 ⇒ sink 已摘除；等转发任务冲完缓冲分片，
                                    // 保证所有 `Delta` 排在 `Reply`/`Stopped` 之前。
                                    let _ = forwarder.await;
                                    match end {
                                        Ok(crate::syschat::TurnEnd::Done(reply)) => {
                                            SysChatEvt::Reply(reply)
                                        }
                                        Ok(crate::syschat::TurnEnd::Stopped) => {
                                            SysChatEvt::Stopped
                                        }
                                        Err(e) => SysChatEvt::Error(e),
                                    }
                                }
                                Err(e) => SysChatEvt::Error(format!("读取配置失败：{e:#}")),
                            };
                            let _ = etx.send(evt);
                        });
                    }
                    // 后台下载 http(s) 媒体到本地缓存；下完发 MediaReady 让 GUI 重载
                    UiCmd::FetchMedia { url } => {
                        let etx = syschat_tx_bg.clone();
                        tokio::spawn(async move {
                            if crate::syschat::fetch_media(&url).await.is_ok() {
                                let _ = etx.send(SysChatEvt::MediaReady);
                            }
                        });
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

    // ── 受管理密码保护的服务动作（2026-10-04 owner：启动/停止/重启统一要密码）──
    //
    // 为什么在 UI 层：这是**第二道门**（防误操作 / 防 agent 顺手停）。让服务杀不掉的仍是
    // 它的托管形态（Windows 计划任务 / macOS launchd）——两者互补，别互相替代：同用户进程
    // 永远能改 config.json，所以「密码」挡不住有意绕过者（见 admin_pass 模块文档）。
    let svc_pw_pending: Rc<RefCell<Option<UiCmd>>> = Rc::new(RefCell::new(None));
    let ask_admin: Rc<dyn Fn(UiCmd)> = {
        let dlg = svc_pw.as_weak();
        let pending = svc_pw_pending.clone();
        Rc::new(move |cmd: UiCmd| {
            *pending.borrow_mut() = Some(cmd);
            let record = crate::config::Config::load()
                .map(|c| c.admin_password)
                .unwrap_or_default();
            if let Some(d) = dlg.upgrade() {
                let first = !crate::admin_pass::is_set(&record);
                d.set_need_password(true);
                d.set_password_input("".into());
                d.set_failed(false);
                d.set_busy(false);
                d.set_title_text(if first {
                    "首次设置管理密码".into()
                } else {
                    "需要管理密码".into()
                });
                d.set_message(if first {
                    "今后「启动 / 停止 / 重启服务」都要输它。只存哈希不存明文；忘了就把 config.json 里的 admin_password 清空重设。".into()
                } else {
                    "启动 / 停止 / 重启服务需要管理密码。".into()
                });
                let _ = d.show();
            }
        })
    };
    {
        let dlg = svc_pw.as_weak();
        let pending = svc_pw_pending.clone();
        let tx = tx.clone();
        svc_pw.on_confirmed(move || {
            let Some(cmd) = pending.borrow_mut().take() else {
                return;
            };
            let pw = dlg
                .upgrade()
                .map(|d| d.get_password_input().to_string())
                .unwrap_or_default();
            let mut cfg = match crate::config::Config::load() {
                Ok(c) => c,
                Err(e) => {
                    if let Some(d) = dlg.upgrade() {
                        d.set_failed(true);
                        d.set_message(format!("读配置失败：{e:#}").into());
                    }
                    *pending.borrow_mut() = Some(cmd);
                    return;
                }
            };
            if crate::admin_pass::is_set(&cfg.admin_password) {
                if !crate::admin_pass::verify_record(&cfg.admin_password, &pw) {
                    crate::log!("[gui] 服务动作：管理密码错误，拒绝");
                    if let Some(d) = dlg.upgrade() {
                        d.set_failed(true);
                        d.set_message("管理密码错误 —— 可重试或取消".into());
                    }
                    *pending.borrow_mut() = Some(cmd);
                    return;
                }
            } else {
                if pw.chars().count() < 4 {
                    if let Some(d) = dlg.upgrade() {
                        d.set_failed(true);
                        d.set_message("密码太短（至少 4 位），请重输".into());
                    }
                    *pending.borrow_mut() = Some(cmd);
                    return;
                }
                cfg.admin_password = crate::admin_pass::new_record(&pw);
                if let Err(e) = cfg.save() {
                    if let Some(d) = dlg.upgrade() {
                        d.set_failed(true);
                        d.set_message(format!("保存管理密码失败：{e:#}").into());
                    }
                    *pending.borrow_mut() = Some(cmd);
                    return;
                }
                crate::log!("[gui] 已设置服务管理密码（启动/停止/重启都要密码）");
            }
            if let Some(d) = dlg.upgrade() {
                let _ = d.hide();
            }
            let _ = tx.send(cmd);
        });
        // 取消 = 丢弃待执行的服务动作 + 关窗。
        //
        // 2026-10-04 修（owner 问「那个取消按钮的作用是什么」）：此前**只接了 on_confirmed**，
        // 而 Slint 的取消按钮只 `root.canceled()`、不会自己关窗 ⇒ 点「取消」没有任何反应：
        // 窗不关、待动作还挂着。这里补齐，与 vb_confirm / team_action 的取消同一套语义。
        let dlg = svc_pw.as_weak();
        let pending = svc_pw_pending.clone();
        svc_pw.on_canceled(move || {
            pending.borrow_mut().take();
            crate::log!("[gui] 服务动作已取消（未执行）");
            if let Some(d) = dlg.upgrade() {
                let _ = d.hide();
            }
        });
    }

    // ── 托盘回调 ──
    {
        let txc = || tx.clone();
        // 三个动作都先过管理密码门（ask_admin 里校验/首设，通过后才发 UiCmd）。
        tray.on_start_service({
            let ask = ask_admin.clone();
            move || ask(UiCmd::Start)
        });
        tray.on_stop_service({
            let ask = ask_admin.clone();
            move || ask(UiCmd::Stop)
        });
        tray.on_restart_service({
            let ask = ask_admin.clone();
            move || ask(UiCmd::Restart)
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
            // 失败不再静默吞（老写法是 `let _ =`）：~/Library/LaunchAgents 不可写、
            // launchctl bootstrap 被拒都是真故障。成败审计由 platform::set_autostart
            // 单点写 logs/autostart.log（GUI 由 open 起时 stdout 指 /dev/null，只
            // crate::log! 等于什么都不留），这里不再重复记一条以免两处文案早晚分叉；
            // 这行 stdout 日志只为「终端里跑 GUI」调试时可见。菜单回显走 2s tick 的
            // autostart_enabled()，读的是 plist 真值。
            if let Err(e) = platform::set_autostart(on) {
                crate::log!(
                    "[ui] ⚠️ 开机自启设置失败（想设为{}）: {e:#}",
                    if on { "开" } else { "关" }
                );
            }
        });
        tray.on_toggle_bubble({
            let bw = bubble.as_weak();
            let tw = tray.as_weak();
            move |on| {
                // 走 config 的写锁 RMW（与其它写方同口径）；失败只记日志，可见性仍按 config 真值。
                if !Config::set_syschat_bubble(on) {
                    crate::log!("[ui] ⚠️ 气泡开关写入 config 失败，可见性按 config 真值对齐");
                }
                // 菜单回显：此前只在启动时 set_bubble_enabled，切菜单后不重建（陈旧）。
                if let Some(tw) = tw.upgrade() {
                    tw.set_bubble_enabled(Config::load().map(|c| c.syschat_bubble).unwrap_or(on));
                }
                // 可见性**统一**由 refresh_bubble_visibility 按 config 决定（v10：与对话框显隐无关，
                // 气泡常驻置顶；托盘开关仍管气泡）。
                if let Some(b) = bw.upgrade() {
                    refresh_bubble_visibility(&b);
                }
            }
        });
        tray.on_check_update({
            let tx = txc();
            move || {
                let _ = tx.send(UiCmd::CheckUpdate { silent: false });
            }
        });
        tray.on_install_update({
            let tx = txc();
            move || {
                let _ = tx.send(UiCmd::InstallUpdate);
            }
        });
    }
    // 启动 20s 后静默检查一次更新，之后每 30min 复查（失败静默；有新版本时托盘菜单出现「升级」项）。
    // 曾为 6h：检出一次新版本后菜单只剩「升级」动作、无手动重查入口（state=3），
    // 密集发版日（一天跳 7 级）菜单会在数小时内一直显示旧版本号——30min 让检出
    // 结果自然刷新（GitHub API 匿名限流 60/h，2 次/h 无压力）。
    {
        let tx2 = tx.clone();
        let t = slint::Timer::default();
        t.start(
            slint::TimerMode::SingleShot,
            Duration::from_secs(20),
            move || {
                let _ = tx2.send(UiCmd::CheckUpdate { silent: true });
            },
        );
        // Timer 需保活到触发：drop 会取消（与 show_window_and_focus 同款泄漏处理，百字节级）
        std::mem::forget(t);
        let tx3 = tx.clone();
        let t6 = slint::Timer::default();
        t6.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(30 * 60),
            move || {
                let _ = tx3.send(UiCmd::CheckUpdate { silent: true });
            },
        );
        std::mem::forget(t6);
    }
    // --show-settings 调试参数：启动即弹出设置窗。复用托盘打开同一条路径
    //（load_into → 状态行），保证窗口内容完整（不只是空窗）。
    // P4.3：「缺 claude/codex/pi → 启动自动弹窗引导安装」已随三后端 CLI 下架删除——
    // 执行层收口随包 abb-agent（零安装），新装用户开箱即可聊天。
    if std::env::args().any(|a| a == "--show-settings") {
        let work = work.clone();
        let model = bots_model.clone();
        let pmodel = providers_model.clone();
        load_with_draft(&settings, &dirty, &work, &model, &pmodel, &wk);
        push_settings_status(&settings, &install::status());
        // `--page N`（配合 --show-settings）：直接落在指定页。页序号同 ui/app.slint 的
        // current-page：0 首页 / 1 机器人配置 / 2 环境配置 / 3 模型供应商 / 4 历史记录。
        // 用途：UI 改动的 before/after 截图取证——不必靠合成点击去点标签（在提权窗口上
        // 合成输入会被 UIPI 拦、多显示器 DPI 下坐标还会被虚拟化，实测不可靠）。
        // 不带该参数时行为不变（首页）。
        let argv: Vec<String> = std::env::args().collect();
        if let Some(i) = argv.iter().position(|a| a == "--page") {
            if let Some(page) = argv.get(i + 1).and_then(|v| v.parse::<i32>().ok()) {
                settings.set_current_page(page.clamp(0, 4));
            }
        }
        show_window_and_focus(&settings);
    }
    // --show-syschat 调试参数：启动即弹出系统对话窗（UI 改动的截图取证，同 --show-settings）。
    if std::env::args().any(|a| a == "--show-syschat") {
        show_window_and_focus(&syschat_win);
    }
    {
        let sw = settings.as_weak();
        let work = work.clone();
        let model = bots_model.clone();
        let pmodel = providers_model.clone();
        let dirty_open = dirty.clone();
        // 托盘「设置…」的显示逻辑：草稿恢复 + 显示置前。
        // （v10 稿 v8 第 4 条：Dock 点击已改成**只开对话窗**，不再走这里——接线在系统对话窗那段，
        //  与气泡点击共用同一个 `open_syschat` 收口。）
        let tray_sw = sw.clone();
        tray.on_open_settings({
            let wk = wk.clone();
            move || {
                if let Some(w) = tray_sw.upgrade() {
                    // 草稿比正式配置新（上次编辑没保存就退出/崩溃）→ 静默恢复为工作底稿
                    load_with_draft(&w, &dirty_open, &work, &model, &pmodel, &wk);
                    push_settings_status(&w, &install::status());
                    show_window_and_focus(&w); // 先 show 再激活再重绘（见该函数注释：避免内容区透明）
                }
            }
        });
    }
    tray.on_quit_app(|| {
        // 语义变更（owner 2026-09-28）：**退出托盘 ≠ 停止服务**。bridge 由 launchd/计划任务
        // 托管，托盘只是客户端；这里不再 `svc_stop()`（那会顺手杀掉 bridge 并清掉「意图=运行」）。
        // 真要停服务请走「停止服务」（需要授权）；退出托盘只关掉这个控制台。
        crate::log!("[gui] 退出托盘（服务保持运行，停止服务需要授权）");
        let _ = slint::quit_event_loop();
    });

    // ── 设置窗首页 hero：启动/停止/重启服务 + 打开日志，复用托盘同一套 UiCmd（后台线程串行执行）──
    {
        let txc = || tx.clone();
        // 设置窗首页同样过管理密码门（与托盘同一把门，避免绕过）。
        settings.on_start_service({
            let ask = ask_admin.clone();
            move || ask(UiCmd::Start)
        });
        settings.on_stop_service({
            let ask = ask_admin.clone();
            move || ask(UiCmd::Stop)
        });
        settings.on_restart_service({
            let ask = ask_admin.clone();
            move || ask(UiCmd::Restart)
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
                // Rust 侧 set_selected 不触发 slint 的 selection-changed（只有列表点击会发），
                // 必须显式重建面板显示，否则新 bot 面板残留上一个 bot 的值（#80 回归类）
                refresh_bot_panel(&w, &work);
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
                // 同 add_bot：删除后 selected 原地不变（min(len-1, idx)==selected）时连属性
                // 变化都没有，必须显式重建面板，否则显示的是被删 bot 的值、编辑会写进移位上来的 bot
                refresh_bot_panel(&w, &work);
            }
        });
    }
    {
        let work = work.clone();
        let model = bots_model.clone();
        // 按字段回写（slint 侧只传被改的那一个字段）：杜绝「未改字段从过期 model 读回」
        // 导致的连改两字段互相回滚（旧 bot-edited 的 CRITICAL bug）。
        let dirty = dirty.clone();
        let sw = settings.as_weak();
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
                        "provider" => bot.provider = value.to_string(),
                        "owner" => bot.owner_open_id = value.trim().to_string(),
                        "app_id" => bot.app_id = value.trim().to_string(),
                        "app_secret" => bot.app_secret = value.trim().to_string(),
                        "ding_user_id" => bot.ding_user_id = value.trim().to_string(),
                        "ding_owner_ids" => bot.ding_owner_ids = value.trim().to_string(),
                        "ding_robot_code" => bot.ding_robot_code = value.trim().to_string(),
                        _ => {}
                    }
                }
            }
            if refresh {
                let b = work.borrow();
                sync_model(&model, &b);
                // 输入框/下拉断绑后不跟随 model 重建，这里一并重建（kind 切换）
                if let Some(w) = sw.upgrade() {
                    refresh_editors(&w, &work);
                }
            }
        });
    }
    {
        let work = work.clone();
        let model = bots_model.clone();
        let dirty = dirty.clone();
        let sw = settings.as_weak();
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
            // 重建单元素 model → for 重建实例（交互断绑后实例不跟随，必须重建）；
            // 统一走 refresh_toggle_checks 从 work 重读（slint 侧 idx 恒为 root.selected）
            if let Some(w) = sw.upgrade() {
                refresh_toggle_checks(&w, &work);
            }
        });
    }
    // 「授权者 agent 隔离」开关（安全默认开启）：关闭=授权者与 owner 同权限（现状全权限）。
    // 同 set_bot_enabled 的独立 bool callback 模式（避开 slint CheckBox checked 绑定坑）。
    {
        let work = work.clone();
        let model = bots_model.clone();
        let dirty = dirty.clone();
        let sw = settings.as_weak();
        settings.on_set_restrict_granted(move |idx, enabled| {
            dirty.set(true);
            {
                let mut b = work.borrow_mut();
                if let Some(bot) = b.get_mut(idx as usize) {
                    bot.restrict_granted_agent = enabled;
                }
            }
            // 同步回写 model：勾选框状态绑 model，不刷新会显示旧值
            let b = work.borrow();
            sync_model(&model, &b);
            // 重建单元素 model → for 重建实例（交互断绑后实例不跟随，必须重建）
            if let Some(w) = sw.upgrade() {
                refresh_toggle_checks(&w, &work);
            }
        });
    }
    // 「每日工作目录整理」开关（默认关）：孤儿会话文件/临时文件/超期历史截断/
    // 文档归档 + git 留痕。同独立 bool callback 模式（避开 CheckBox 绑定坑）。
    {
        let work = work.clone();
        let model = bots_model.clone();
        let dirty = dirty.clone();
        let sw = settings.as_weak();
        settings.on_set_tidy_enabled(move |idx, enabled| {
            dirty.set(true);
            {
                let mut b = work.borrow_mut();
                if let Some(bot) = b.get_mut(idx as usize) {
                    bot.tidy_enabled = enabled;
                }
            }
            // 同步回写 model：勾选框状态绑 model，不刷新会显示旧值
            let b = work.borrow();
            sync_model(&model, &b);
            // 重建单元素 model → for 重建实例（交互断绑后实例不跟随，必须重建）
            if let Some(w) = sw.upgrade() {
                refresh_toggle_checks(&w, &work);
            }
        });
    }
    // #91 群聊提及默认（bot 级）：true=免 @ 参与。同独立 bool callback 模式。
    {
        let work = work.clone();
        let model = bots_model.clone();
        let dirty = dirty.clone();
        let sw = settings.as_weak();
        settings.on_set_mention_default(move |idx, enabled| {
            dirty.set(true);
            {
                let mut b = work.borrow_mut();
                if let Some(bot) = b.get_mut(idx as usize) {
                    bot.mention_default = enabled;
                }
            }
            let b = work.borrow();
            sync_model(&model, &b);
            if let Some(w) = sw.upgrade() {
                refresh_toggle_checks(&w, &work);
            }
        });
    }
    {
        let cdwork = wk.cross_delivery.clone();
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
                refresh_bot_panel(&w, &work);
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
    // ── 虚拟 Bot 弹窗（#75）：交互接线 ──
    {
        let dlg = vb_dialog.as_weak();
        vb_dialog.on_mode_switched(move |_m| {
            // 切模式：清掉上次操作的结果/进度/提示（残留会误导用户）
            if let Some(d) = dlg.upgrade() {
                d.set_results(slint::ModelRc::from(Rc::new(slint::VecModel::from(Vec::<
                    slint::SharedString,
                >::new(
                )))));
                d.set_progress_text("".into());
                d.set_busy(false);
                vb_hint(&d, "", false);
            }
        });
    }
    {
        let dlg = vb_dialog.as_weak();
        let edit = vb_edit.clone();
        vb_dialog.on_name_edited(move || {
            if let Some(d) = dlg.upgrade() {
                // 字符计数按 chars()（中文安全）；≤60 为飞书群名限制
                d.set_name_count(d.get_name_input().chars().count() as i32);
                // #125：用户手动编辑群名 → dirty，异步回填不得覆盖（Fetched 竞态）
                if d.get_mode() == 3 {
                    edit.borrow_mut().name_dirty = true;
                }
            }
        });
    }
    {
        let dlg = vb_dialog.as_weak();
        let edit = vb_edit.clone();
        vb_dialog.on_prompt_edited(move || {
            if let Some(d) = dlg.upgrade() {
                d.set_prompt_count(d.get_prompt_input().chars().count() as i32);
                // #125：用户手动编辑提示词 → dirty，异步回填不得覆盖
                if d.get_mode() == 3 {
                    edit.borrow_mut().prompt_dirty = true;
                }
            }
        });
    }
    // 「✨ 生成」提示词（8-20 需求）：根据群名走该 bot 生效后端生成系统提示词，
    // 生成期间弹窗 busy（按钮文字"生成中…"防连点），结果 PromptGenerated 回填
    {
        let tx = tx.clone();
        let ctx = vb_ctx.clone();
        let dlg = vb_dialog.as_weak();
        vb_dialog.on_generate_prompt(move || {
            let Some((_idx, bot_key, _kind)) = ctx.borrow().clone() else {
                return;
            };
            let name = dlg
                .upgrade()
                .map(|d| d.get_name_input().to_string())
                .unwrap_or_default();
            if name.trim().is_empty() {
                return;
            }
            if let Some(d) = dlg.upgrade() {
                d.set_busy(true); // 复用 busy：禁用按钮/输入，生成中
                vb_hint(&d, "", false);
            }
            let _ = tx.send(UiCmd::GeneratePrompt { bot_key, name });
        });
    }
    {
        let dlg = vb_dialog.as_weak();
        vb_dialog.on_template_toggled(move |idx| {
            if let Some(d) = dlg.upgrade() {
                let model = d.get_templates();
                if idx >= 0 && (idx as usize) < model.row_count() {
                    if let Some(mut row) = model.row_data(idx as usize) {
                        row.checked = !row.checked;
                        model.set_row_data(idx as usize, row);
                    }
                }
                // 勾选数（summary「将创建 N 个群」）
                let mut n = 0i32;
                for i in 0..model.row_count() {
                    if let Some(r) = model.row_data(i) {
                        if r.checked {
                            n += 1;
                        }
                    }
                }
                d.set_selected_count(n);
            }
        });
    }
    {
        let dlg = vb_dialog.as_weak();
        vb_dialog.on_template_edit_clicked(move |idx| {
            if let Some(d) = dlg.upgrade() {
                d.set_template_editing(true);
                d.set_template_edit_idx(idx);
                if idx >= 0 {
                    let model = d.get_templates();
                    if (idx as usize) < model.row_count() {
                        if let Some(r) = model.row_data(idx as usize) {
                            d.set_template_edit_name(r.name);
                            d.set_template_edit_prompt(r.prompt);
                        }
                    }
                } else {
                    d.set_template_edit_name("".into());
                    d.set_template_edit_prompt("".into());
                }
                vb_hint(&d, "", false);
            }
        });
    }
    {
        let dlg = vb_dialog.as_weak();
        vb_dialog.on_template_cancel_clicked(move || {
            if let Some(d) = dlg.upgrade() {
                d.set_template_editing(false);
                d.set_template_edit_idx(-1);
                vb_hint(&d, "", false);
            }
        });
    }
    {
        let dlg = vb_dialog.as_weak();
        let twork = wk.templates.clone();
        let dirty = dirty.clone();
        vb_dialog.on_template_save_clicked(move || {
            if let Some(d) = dlg.upgrade() {
                let name = d.get_template_edit_name().trim().to_string();
                let prompt = d.get_template_edit_prompt().trim().to_string();
                let idx = d.get_template_edit_idx();
                if name.is_empty() {
                    vb_hint(&d, "模板名不能为空", true);
                    return;
                }
                if name.chars().count() > ROLE_NAME_MAX {
                    vb_hint(&d, &format!("模板名超长（≤{ROLE_NAME_MAX} 字符）"), true);
                    return;
                }
                if prompt.is_empty() {
                    vb_hint(&d, "提示词不能为空", true);
                    return;
                }
                if prompt.chars().count() > ROLE_PROMPT_MAX {
                    vb_hint(&d, &format!("提示词超长（≤{ROLE_PROMPT_MAX} 字符）"), true);
                    return;
                }
                // 重名（内置 + 其它自定义）拒绝：模板按名寻址，重名无法区分
                let dup = builtin_templates().iter().any(|t| t.name == name)
                    || twork
                        .borrow()
                        .iter()
                        .enumerate()
                        .any(|(i, t)| t.name == name && i as i32 != idx);
                if dup {
                    vb_hint(&d, "该模板名已存在（内置或其它自定义模板）", true);
                    return;
                }
                {
                    let mut w = twork.borrow_mut();
                    if idx < 0 {
                        w.push(RoleTemplate { name, prompt });
                    } else if (idx as usize) < w.len() {
                        w[idx as usize] = RoleTemplate { name, prompt };
                    }
                }
                dirty.set(true); // 模板改动随设置窗「保存」写盘
                d.set_template_editing(false);
                d.set_template_edit_idx(-1);
                vb_hint(&d, "", false);
                sync_vb_templates(&d, &twork);
            }
        });
    }
    {
        let dlg = vb_dialog.as_weak();
        let twork = wk.templates.clone();
        let dirty = dirty.clone();
        vb_dialog.on_template_delete_clicked(move |idx| {
            if let Some(d) = dlg.upgrade() {
                let builtin = builtin_templates().len() as i32;
                if idx < builtin {
                    vb_hint(&d, "内置模板不可删除", true);
                    return;
                }
                let ci = (idx - builtin) as usize;
                let mut w = twork.borrow_mut();
                if ci < w.len() {
                    w.remove(ci);
                    drop(w);
                    dirty.set(true);
                    sync_vb_templates(&d, &twork);
                }
            }
        });
    }
    {
        let dlg = vb_dialog.as_weak();
        let tx = tx.clone();
        let work = work.clone();
        let ctx = vb_ctx.clone();
        let edit = vb_edit.clone();
        vb_dialog.on_create_clicked(move || {
            let Some(d) = dlg.upgrade() else { return };
            // 归属 bot 上下文（打开时记录；app_id/secret 点创建时从工作副本取最新——
            // 用户可能改了没保存，用旧值会建到旧应用下）
            let Some((idx, bot_key, kind)) = ctx.borrow().clone() else {
                return;
            };
            // app_id/secret/owner 都取工作副本最新（用户可能改了没保存，用旧值会建到旧
            // 应用下；owner 同理：磁盘配置可能比工作副本旧，且按 key 匹配在改名后失效）。
            let (app_id, app_secret, owner) = {
                let b = work.borrow();
                match b.get(idx as usize) {
                    Some(bot) => (
                        bot.app_id.clone(),
                        bot.app_secret.clone(),
                        first_owner_id(&bot.owner_open_id),
                    ),
                    None => (String::new(), String::new(), None),
                }
            };
            let mode = d.get_mode();
            match mode {
                // 0=自定义创建 3=编辑
                0 | 3 => {
                    let name = d.get_name_input().trim().to_string();
                    let prompt = d.get_prompt_input().trim().to_string();
                    if name.is_empty() {
                        vb_hint(&d, "群名不能为空", true);
                        return;
                    }
                    if name.chars().count() > ROLE_NAME_MAX {
                        vb_hint(&d, &format!("群名超长（≤{ROLE_NAME_MAX} 字符）"), true);
                        return;
                    }
                    if prompt.chars().count() > ROLE_PROMPT_MAX {
                        vb_hint(&d, &format!("提示词超长（≤{ROLE_PROMPT_MAX} 字符）"), true);
                        return;
                    }
                    if mode == 0 {
                        let _ = tx.send(UiCmd::VirtualBotCreate {
                            bot_key,
                            kind,
                            app_id,
                            app_secret,
                            owner: owner.clone(),
                            items: vec![(name, prompt)],
                        });
                    } else {
                        // #125：编辑保存前校验异步预填状态——Pending（未拉到平台资料）
                        // 或 Failed（拉到失败、当前为登记旧名）都禁止静默保存，防把
                        // 旧名/空提示词写回平台
                        let st = edit.borrow().clone();
                        if let Some(msg) = vb_edit_save_blocked(&st, d.get_edit_chat_id().as_str())
                        {
                            vb_hint(&d, msg, true);
                            return;
                        }
                        let chat_id = d.get_edit_chat_id().to_string();
                        let _ = tx.send(UiCmd::VirtualBotUpdate {
                            bot_key,
                            kind,
                            app_id,
                            app_secret,
                            chat_id,
                            name,
                            prompt,
                            owner: owner.clone(),
                        });
                    }
                }
                // 1=模板批量创建
                1 => {
                    let prefix = d.get_prefix_input().trim().to_string();
                    let model = d.get_templates();
                    let mut items: Vec<(String, String)> = Vec::new();
                    for i in 0..model.row_count() {
                        if let Some(r) = model.row_data(i) {
                            if r.checked {
                                let name = if prefix.is_empty() {
                                    r.name.to_string()
                                } else {
                                    format!("{prefix}·{}", r.name)
                                };
                                items.push((name, r.prompt.to_string()));
                            }
                        }
                    }
                    if items.is_empty() {
                        vb_hint(&d, "请至少勾选一个模板", true);
                        return;
                    }
                    if let Some((n, _)) = items
                        .iter()
                        .find(|(n, _)| n.chars().count() > ROLE_NAME_MAX)
                    {
                        vb_hint(
                            &d,
                            &format!("群名超长（≤{ROLE_NAME_MAX} 字符）：{n}（前缀太长？）"),
                            true,
                        );
                        return;
                    }
                    let _ = tx.send(UiCmd::VirtualBotCreate {
                        bot_key,
                        kind,
                        app_id,
                        app_secret,
                        owner: owner.clone(),
                        items,
                    });
                }
                // 2=手动登记（降级路径）
                _ => {
                    let name = d.get_name_input().trim().to_string();
                    let chat_id = d.get_register_chat_id().trim().to_string();
                    if name.is_empty() {
                        vb_hint(&d, "群名不能为空", true);
                        return;
                    }
                    if name.chars().count() > ROLE_NAME_MAX {
                        vb_hint(&d, &format!("群名超长（≤{ROLE_NAME_MAX} 字符）"), true);
                        return;
                    }
                    if chat_id.is_empty() {
                        vb_hint(&d, "chat_id 不能为空", true);
                        return;
                    }
                    let _ = tx.send(UiCmd::VirtualBotRegister {
                        bot_key,
                        name,
                        chat_id,
                    });
                }
            }
            // 提交后进 busy：进度/结果由 vb_rx 驱动（定时器轮询回填弹窗）
            vb_hint(&d, "", false);
            d.set_busy(true);
            d.set_progress_text("处理中…".into());
        });
    }
    // ── 设置窗虚拟 Bot 区（#75）：创建/编辑/取消登记/解散 ──
    {
        let work = work.clone();
        let ctx = vb_ctx.clone();
        let dlg = vb_dialog.as_weak();
        let twork = wk.templates.clone();
        settings.on_virtual_bot_create(move |idx| {
            let b = work.borrow();
            if let Some(bot) = b.get(idx as usize) {
                if bot.kind == "wechat" {
                    return; // 微信排除（slint 已隐藏该区，这里双保险）
                }
                *ctx.borrow_mut() = Some((idx, bot.key(), bot.kind.clone()));
                if let Some(d) = dlg.upgrade() {
                    vb_open_dialog(&d, bot, 0, None, &twork);
                    show_window_and_focus(&d);
                }
            }
        });
    }
    // #124 一键创建团队（P2，mock 阶段）：UI 骨架先行，数据为预设，后端 #123 就绪后换真数据流。
    {
        let td = team_dialog.as_weak();
        let work = work.clone();
        let team_ctx = team_ctx.clone();
        let team_plan = team_plan.clone();
        settings.on_team_create_clicked(move |idx| {
            let Some(d) = td.upgrade() else { return };
            let label = work
                .borrow()
                .get(idx as usize)
                .map(|bot| {
                    if bot.name.is_empty() {
                        format!("（{}）", kind_label(&bot.kind))
                    } else {
                        bot.name.clone()
                    }
                })
                .unwrap_or_default();
            // #141：记录归属 bot（生成/建群按下标取最新配置）
            let bot_key = work.borrow().get(idx as usize).map(|b| b.key());
            if let Some(k) = bot_key {
                *team_ctx.borrow_mut() = Some((idx, k));
            }
            *team_plan.borrow_mut() = None; // 新会话清掉旧方案
            d.set_bot_label(label.into());
            d.set_mode(0);
            d.set_target_input("".into());
            d.set_team_name("".into());
            d.set_flow("".into());
            d.set_roles(slint::ModelRc::from(Rc::new(slint::VecModel::from(Vec::<
                TeamRoleRow,
            >::new(
            )))));
            d.set_results(slint::ModelRc::from(Rc::new(slint::VecModel::from(Vec::<
                TeamResultRow,
            >::new(
            )))));
            d.set_busy(false);
            show_window_and_focus(&d);
        });
    }
    // #147 任命成员：打开小表单弹窗（预填当前成员；确认后发 TeamAppoint 写团队登记表）
    {
        let sw = settings.as_weak();
        let work = work.clone();
        let dlg = appoint_dialog.as_weak();
        let ctx = appoint_ctx.clone();
        settings.on_team_appoint(move |team_idx, role_idx| {
            let Some(w) = sw.upgrade() else { return };
            let sel = w.get_selected();
            let b = work.borrow();
            let Some(bot) = b.get(sel as usize) else {
                return;
            };
            let teams = crate::teamreg::TeamStore::new().load_for(&bot.key());
            let Some(team) = teams.get(team_idx as usize) else {
                return;
            };
            let Some(role) = team.roles.get(role_idx as usize) else {
                return;
            };
            *ctx.borrow_mut() = Some((bot.key(), team.team_name.clone(), role.role_name.clone()));
            let bot_label = if bot.name.is_empty() {
                kind_label(&bot.kind)
            } else {
                bot.name.clone()
            };
            if let Some(d) = dlg.upgrade() {
                d.set_team_label(format!("{}（{}）", team.team_name, bot_label).into());
                d.set_role_name(role.role_name.clone().into());
                d.set_member_input(role.member.clone().into());
                d.set_window_title("任命成员".into());
                show_window_and_focus(&d);
            }
        });
    }
    // #147 跳转角色群：打开对应平台会话（复用 open-url 回调；未建成角色 = 部分失败，提示不跳）
    {
        let sw = settings.as_weak();
        let work = work.clone();
        settings.on_team_jump(move |team_idx, role_idx| {
            let Some(w) = sw.upgrade() else { return };
            let sel = w.get_selected();
            let b = work.borrow();
            let Some(bot) = b.get(sel as usize) else {
                return;
            };
            let teams = crate::teamreg::TeamStore::new().load_for(&bot.key());
            let Some(team) = teams.get(team_idx as usize) else {
                return;
            };
            let Some(role) = team.roles.get(role_idx as usize) else {
                return;
            };
            if role.chat_id.is_empty() {
                w.set_status_is_error(true);
                w.set_status_line(
                    format!(
                        "角色「{}」未建成角色群（部分失败），无法跳转",
                        role.role_name
                    )
                    .into(),
                );
                return;
            }
            let link = match bot.kind.as_str() {
                "dingtalk" => format!(
                    "dingtalk://dingtalkclient/action/openchat?chatid={}",
                    role.chat_id
                ),
                _ => format!(
                    "https://applink.feishu.cn/client/chat/open?chatId={}",
                    role.chat_id
                ),
            };
            crate::log!(
                "[gui] 跳转角色群 bot={} chat={}",
                bot.key(),
                crate::agent::truncate(&role.chat_id, 12)
            );
            w.invoke_open_url(link.into());
        });
    }
    // #147 解散团队：红色强确认（与虚拟 Bot 解散同款纪律）→ 发 TeamDissolve
    {
        let sw = settings.as_weak();
        let work = work.clone();
        let team_action = team_action.clone();
        let confirm = vb_confirm.as_weak();
        settings.on_team_dissolve(move |team_idx| {
            let Some(w) = sw.upgrade() else { return };
            let sel = w.get_selected();
            let b = work.borrow();
            let Some(bot) = b.get(sel as usize) else { return };
            let teams = crate::teamreg::TeamStore::new().load_for(&bot.key());
            let Some(team) = teams.get(team_idx as usize) else { return };
            *team_action.borrow_mut() = Some(TeamAction::Dissolve {
                bot_key: bot.key(),
                team_name: team.team_name.clone(),
            });
            if let Some(c) = confirm.upgrade() {
                c.set_title_text("解散团队（不可恢复）".into());
                c.set_message(
                    format!(
                        "将解散团队「{}」（{} 个角色）：移除全部角色登记并归档聊天记录（平台群保留、不再受 ABB 管理）。此操作不可恢复，确认继续？",
                        team.team_name,
                        team.roles.len()
                    )
                    .into(),
                );
                c.set_confirm_text("解散团队".into());
                c.set_cancel_text("取消".into());
                c.set_danger(true); // 红色强确认
                show_window_and_focus(&c);
            }
        });
    }
    // #147 任命弹窗：确认 → 发 TeamAppoint（留空 = 恢复待任命）；取消 → 隐藏
    {
        let dlg = appoint_dialog.as_weak();
        let ctx = appoint_ctx.clone();
        let tx = tx.clone();
        appoint_dialog.on_confirm_clicked(move || {
            let Some(d) = dlg.upgrade() else { return };
            let Some((bot_key, team_name, role_name)) = ctx.borrow().clone() else {
                return;
            };
            let member = d.get_member_input().trim().to_string();
            let _ = tx.send(UiCmd::TeamAppoint {
                bot_key,
                team_name,
                role_name,
                member,
            });
            let _ = d.hide();
        });
    }
    {
        let dlg = appoint_dialog.as_weak();
        appoint_dialog.on_cancel_clicked(move || {
            if let Some(d) = dlg.upgrade() {
                let _ = d.hide();
            }
        });
    }
    // 生成方案（#141 真实链路）：发 UiCmd::TeamGenerate，后台 LLM 生成，结果经 team_rx 回填。
    {
        let td = team_dialog.as_weak();
        let tx = tx.clone();
        let team_ctx = team_ctx.clone();
        team_dialog.on_generate_clicked(move || {
            let Some(d) = td.upgrade() else { return };
            let target = d.get_target_input().trim().to_string();
            if target.is_empty() {
                return; // 目标为空：无操作（输入框留提示）
            }
            let Some((idx, bot_key)) = team_ctx.borrow().clone() else {
                return;
            };
            d.set_busy(true);
            let _ = tx.send(UiCmd::TeamGenerate {
                idx,
                bot_key,
                target,
            });
        });
    }
    // 确认创建（#141 真实链路）：携已生成的方案 JSON 发 UiCmd::TeamCreate，
    // 后台逐角色建群 + 登记（幂等），结果经 team_rx 回填。
    {
        let td = team_dialog.as_weak();
        let tx = tx.clone();
        let team_plan = team_plan.clone();
        let team_ctx = team_ctx.clone();
        team_dialog.on_confirm_clicked(move || {
            let Some(d) = td.upgrade() else { return };
            let Some((idx, bot_key)) = team_ctx.borrow().clone() else {
                return;
            };
            let Some(plan) = team_plan.borrow().clone() else {
                return;
            };
            d.set_busy(true);
            let _ = tx.send(UiCmd::TeamCreate { idx, bot_key, plan });
        });
    }
    // 修改 → 回目标输入
    {
        let td = team_dialog.as_weak();
        team_dialog.on_back_clicked(move || {
            if let Some(d) = td.upgrade() {
                d.set_mode(0);
            }
        });
    }
    // 关闭（busy 时禁用，同虚拟 Bot 弹窗纪律）
    {
        let td = team_dialog.as_weak();
        team_dialog.on_close_clicked(move || {
            if let Some(d) = td.upgrade() {
                if !d.get_busy() {
                    let _ = d.hide();
                }
            }
        });
    }

    // 手动刷新：验证登记群存在性（平台解散兜底 + 历史归档），结果走状态行。
    // 独立 block：不依赖上面的 block 变量（work/tx 已被 on_virtual_bot_create 闭包 move），
    // 直接 clone 最外层作用域变量 + weak settings。
    {
        let tx = tx.clone();
        let work = work.clone();
        let sw = settings.as_weak();
        settings.on_virtual_bot_refresh(move || {
            let Some(w) = sw.upgrade() else { return };
            let sel = w.get_selected();
            let b = work.borrow();
            if let Some(bot) = b.get(sel as usize) {
                if bot.kind == "wechat" {
                    return;
                }
                // 同步中：禁用刷新按钮防连点（结果回来在 Done 里复位）
                w.set_vb_syncing(true);
                let _ = tx.send(UiCmd::VirtualBotVerify {
                    bot_key: bot.key(),
                    kind: bot.kind.clone(),
                    app_id: bot.app_id.clone(),
                    app_secret: bot.app_secret.clone(),
                });
            }
        });
    }
    {
        let tx = tx.clone();
        let sw = settings.as_weak();
        let work = work.clone();
        let ctx = vb_ctx.clone();
        let edit = vb_edit.clone();
        let dlg = vb_dialog.as_weak();
        let twork = wk.templates.clone();
        settings.on_virtual_bot_edit(move |row| {
            let Some(w) = sw.upgrade() else {
                return;
            };
            let sel = w.get_selected();
            let b = work.borrow();
            let Some(bot) = b.get(sel as usize) else {
                return;
            };
            let regs = VirtualBotStore::new().load_for(&bot.key());
            let Some(reg) = regs.get(row as usize) else {
                return;
            };
            *ctx.borrow_mut() = Some((sel, bot.key(), bot.kind.clone()));
            // #125：打开即进入 Pending——群名/提示词不预填登记旧值（避免打开残留旧名、
            // 保存回退平台群名），显示「正在拉取…」；平台真实群资料由 Fetched 回填。
            *edit.borrow_mut() = VbEditState {
                phase: VbFetchPhase::Pending,
                chat_id: reg.chat_id.clone(),
                fallback_name: reg.role_name.clone(),
                name_dirty: false,
                prompt_dirty: false,
            };
            if let Some(d) = dlg.upgrade() {
                vb_open_dialog(&d, bot, 3, Some((reg, "", "")), &twork);
                vb_hint(&d, "正在拉取群资料…", false);
                show_window_and_focus(&d);
            }
            let _ = tx.send(UiCmd::VirtualBotFetchInfo {
                kind: bot.kind.clone(),
                app_id: bot.app_id.clone(),
                app_secret: bot.app_secret.clone(),
                chat_id: reg.chat_id.clone(),
            });
        });
    }
    {
        let sw = settings.as_weak();
        let work = work.clone();
        let action = vb_action.clone();
        let confirm = vb_confirm.as_weak();
        // #91 虚拟 Bot 免 @ 开关：切换 mention_modes[chat_id]（事实源单一，热读即时生效）。
        settings.on_virtual_bot_mention_toggle({
            let sw = sw.clone();
            let work = work.clone();
            let dirty = dirty.clone();
            move |row| {
                crate::log!("[gui] ⋯ 切换虚拟 Bot 免@开关 row={row}");
                let Some(w) = sw.upgrade() else { return };
                let sel = w.get_selected();
                let mut b = work.borrow_mut();
                let Some(bot) = b.get_mut(sel as usize) else {
                    return;
                };
                let regs = VirtualBotStore::new().load_for(&bot.key());
                let Some(reg) = regs.get(row as usize) else {
                    return;
                };
                // toggle：off → 移除条目（恢复需要 @）；其它 → 置 off（免 @）
                let is_off = bot.mention_modes.get(&reg.chat_id).map(String::as_str) == Some("off");
                if is_off {
                    bot.mention_modes.remove(&reg.chat_id);
                } else {
                    bot.mention_modes.insert(reg.chat_id.clone(), "off".into());
                }
                dirty.set(true);
                drop(b);
                refresh_vb_rows(&w, &work);
            }
        });
        settings.on_virtual_bot_deregister(move |row| {
            crate::log!("[gui] ⋯ 点击「取消登记」 row={row}");
            let Some(w) = sw.upgrade() else { return };
            let sel = w.get_selected();
            let b = work.borrow();
            let Some(bot) = b.get(sel as usize) else { return };
            let regs = VirtualBotStore::new().load_for(&bot.key());
            let Some(reg) = regs.get(row as usize) else { return };
            *action.borrow_mut() = Some(VbAction::Deregister {
                bot_key: bot.key(),
                chat_id: reg.chat_id.clone(),
            });
            if let Some(c) = confirm.upgrade() {
                c.set_title_text("取消登记".into());
                c.set_message(
                    format!(
                        "仅取消 ABB 登记，平台群「{}」保留。群内 @ 机器人将不再注入角色，deliver @{} 寻址也会失效。",
                        reg.role_name, reg.role_name
                    )
                    .into(),
                );
                c.set_confirm_text("取消登记".into());
                c.set_cancel_text("再想想".into());
                c.set_danger(false); // 轻确认
                show_window_and_focus(&c);
            }
        });
    }
    {
        let sw = settings.as_weak();
        let work = work.clone();
        let action = vb_action.clone();
        let confirm = vb_confirm.as_weak();
        settings.on_virtual_bot_disband(move |row| {
            crate::log!("[gui] ⋯ 点击「解散群」 row={row}");
            let Some(w) = sw.upgrade() else { return };
            let sel = w.get_selected();
            let b = work.borrow();
            let Some(bot) = b.get(sel as usize) else { return };
            let regs = VirtualBotStore::new().load_for(&bot.key());
            let Some(reg) = regs.get(row as usize) else { return };
            *action.borrow_mut() = Some(VbAction::Disband {
                bot_key: bot.key(),
                kind: bot.kind.clone(),
                app_id: bot.app_id.clone(),
                app_secret: bot.app_secret.clone(),
                chat_id: reg.chat_id.clone(),
                // owner 用于权限不足时发授权指引（白名单取首个，None=未配置）
                owner: crate::config::first_owner_id(&bot.owner_open_id),
            });
            if let Some(c) = confirm.upgrade() {
                c.set_title_text("解散群（不可恢复）".into());
                c.set_message(
                    format!(
                        "将解散平台群「{}」并删除群内全部消息，同时移除 ABB 登记。此操作不可恢复，确认继续？",
                        reg.role_name
                    )
                    .into(),
                );
                c.set_confirm_text("解散群".into());
                c.set_cancel_text("取消".into());
                c.set_danger(true); // 红色强确认
                show_window_and_focus(&c);
            }
        });
    }
    {
        let tx = tx.clone();
        let work = work.clone();
        let sw = settings.as_weak();
        let dirty = dirty.clone();
        settings.on_save_clicked({
            let wk = wk.clone();
            move || {
                dirty.set(false);
                if let Some(w) = sw.upgrade() {
                    let (c, dropped) = snapshot_config(&work, &wk);
                    let _ = tx.send(UiCmd::Save(Box::new(c)));
                    // 保存后窗口保持打开（用户要求）：给个绿色确认，方便继续编辑或手动关闭。
                    w.set_status_is_error(false);
                    let mut msg =
                        "✅ 已保存。窗口可继续编辑，不用了点「关闭」或红点关闭。".to_string();
                    if dropped > 0 {
                        msg.push_str(&format!("（丢弃 {dropped} 个未命名供应商）"));
                    }
                    w.set_status_line(msg.into());
                }
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
    // 打开外部链接（依赖文档/申请 token 等「点按钮 → 默认浏览器打开 url」）
    settings.on_open_url(|url| {
        platform::open_url(url.as_str());
    });
    // ── #74 历史记录页控件 ──
    // 提醒开关：写工作副本 + 标记 dirty（随保存写 config；保存走既有重启链路）
    settings.on_set_notify_enabled({
        let sw = settings.as_weak();
        let work = wk.notify.clone();
        let dirty = dirty.clone();
        move |on| {
            *work.borrow_mut() = on;
            dirty.set(true);
            if let Some(w) = sw.upgrade() {
                w.set_notify_enabled(on); // 回写属性：页面切换重建控件时保持选中态
            }
        }
    });
    // 保留期下拉（下标 0=7 天 1=30 天 2=90 天）：写工作副本 + 标记 dirty
    settings.on_set_history_retention({
        let sw = settings.as_weak();
        let work = wk.history_retention.clone();
        let dirty = dirty.clone();
        move |idx| {
            let days = match idx {
                0 => 7,
                2 => 90,
                _ => 30,
            };
            *work.borrow_mut() = days;
            dirty.set(true);
            if let Some(w) = sw.upgrade() {
                w.set_history_retention_days(days as i32);
            }
        }
    });
    // 会话归纳清理开关（#78，默认关）：写工作副本 + 标记 dirty
    settings.on_set_session_gc_enabled({
        let sw = settings.as_weak();
        let work = wk.session_gc.clone();
        let dirty = dirty.clone();
        move |on| {
            *work.borrow_mut() = on;
            dirty.set(true);
            if let Some(w) = sw.upgrade() {
                w.set_session_gc_enabled(on); // 回写属性：页面切换重建控件时保持选中态
            }
        }
    });
    // 过期天数下拉（下标 0=3 天 1=7 天 2=14 天 3=30 天）：写工作副本 + 标记 dirty
    settings.on_set_session_gc_days({
        let sw = settings.as_weak();
        let work = wk.session_gc_days.clone();
        let dirty = dirty.clone();
        move |idx| {
            let days = match idx {
                0 => 3,
                2 => 14,
                3 => 30,
                _ => 7,
            };
            *work.borrow_mut() = days;
            dirty.set(true);
            if let Some(w) = sw.upgrade() {
                w.set_session_gc_days(days as i32);
            }
        }
    });
    // 历史页选中会话：记快照 + 立即载入该会话消息流（轮询只做刷新，首查在这里）
    {
        let sm = sessions_model.clone();
        let sel = selected_chat.clone();
        let msgs = msgs_model.clone();
        let sw = settings.as_weak();
        settings.on_history_selection_changed(move |idx| {
            let row = sm.row_data(idx as usize);
            let Some(row) = row else { return };
            // 从会话行反查 ChatStats（标题要 count_total/last_ts；轮询 tick 会持续刷新）
            let stats = crate::msgstore::MsgStore::production()
                .chat_stats()
                .into_iter()
                .find(|s| s.bot_key == row.bot_key.as_str() && s.chat_id == row.chat_id.as_str());
            let Some(s) = stats else { return };
            let cfg = Config::load().unwrap_or_default();
            let rows =
                crate::msgstore::MsgStore::production().list_chat(&s.bot_key, &s.chat_id, 500);
            if let Some(w) = sw.upgrade() {
                sync_history_msgs(&msgs, &w, &s, &rows, &cfg);
            }
            *sel.borrow_mut() = Some(s);
        });
    }
    // 「清除全部历史」→ 二次确认弹窗（复用 UnsavedDialog 独立实例）
    {
        let dw = clear_dialog.as_weak();
        settings.on_clear_history(move || {
            if let Some(d) = dw.upgrade() {
                show_window_and_focus(&d);
            }
        });
    }
    {
        let dw = clear_dialog.as_weak();
        clear_dialog.on_discard_close(move || {
            if let Some(d) = dw.upgrade() {
                let _ = d.hide(); // 取消：设置窗还开着，不动 dock
            }
        });
    }
    {
        let dw = clear_dialog.as_weak();
        let hm = sessions_model.clone();
        let hs = selected_chat.clone();
        let hm_msgs = msgs_model.clone();
        clear_dialog.on_save_close(move || {
            // 确认清除：GUI 只读连接不能写消息库 → 落命令文件，service 的 history-gc
            // 消费执行（清空 messages.sqlite + unread.json）。这里先乐观清空列表，
            // 服务端确认后下个 2s tick 自然对齐。
            write_command_file("msg-clear.command");
            hm.set_vec(Vec::new());
            hm_msgs.set_vec(Vec::new());
            *hs.borrow_mut() = None;
            if let Some(d) = dw.upgrade() {
                let _ = d.hide();
            }
        });
    }
    // ── #74 提醒弹窗：点条目 → 打开设置窗历史页；「知道了」/5s 自动收起 ──
    {
        let nw = notifications.as_weak();
        let sw = settings.as_weak();
        let showing = notif_showing.clone();
        notifications.on_open_history(move || {
            if let Some(w) = sw.upgrade() {
                w.set_current_page(4);
                show_window_and_focus(&w);
            }
            // 弹窗内容即已读（展示时已落 msg-read.command）：这里只收起，不 hide_dock
            // ——设置窗已打开（收尾的 dock 不变式归它）
            hide_notifications_window(&nw, &sw, &showing);
        });
        notifications.on_dismiss({
            let nw = notifications.as_weak();
            let sw = settings.as_weak();
            let showing = notif_showing.clone();
            move || hide_notifications_window(&nw, &sw, &showing)
        });
    }
    // 「去授权」：跳到对应系统权限的设置面板（仅 macOS 有；面板 URL 与 deps.rs detect_permissions 一致）
    settings.on_open_perm_settings(|id| {
        let url = match id.as_str() {
            "full-disk" => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles"
            }
            "accessibility" => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            }
            "screen" => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
            }
            "automation" => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Automation"
            }
            "camera" => "x-apple.systempreferences:com.apple.preference.security?Privacy_Camera",
            "microphone" => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
            }
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
                w.set_status_line(
                    "⏳ 逐项弹系统授权框（屏幕录制→摄像头→麦克风），请点「允许」…".into(),
                );
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
                // #105：git 版本过低也按缺失提示（与 missing_dep_ids 同口径）；
                // python3 信息项不计入缺失
                let missing: Vec<&str> = all
                    .iter()
                    .filter(|d| d.id != "python3" && (!d.found || (d.id == "git" && !d.version_ok)))
                    .map(|d| d.label)
                    .collect();
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
        // #60 的一键装回调也要用 tx/sw——先克隆再进第一个闭包（move 语义）
        let sw2 = sw.clone();
        let tx2 = tx.clone();
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
        // #60 一键安装全部缺失组件：防连点 + 空清单提示 + 发后台任务；
        // 逐项进度与汇总经 DepEvt 回主线程（tick 处处理）。
        settings.on_install_all_missing(move || {
            if let Some(w) = sw2.upgrade() {
                if !w.get_dep_busy().is_empty() {
                    return; // 已有安装在进行（单项或一键）
                }
                let missing = crate::deps::missing_dep_ids(&crate::deps::detect_all());
                if missing.is_empty() {
                    w.set_status_is_error(false);
                    w.set_status_line("✅ 全部依赖均已安装".into());
                    return;
                }
                w.set_dep_busy(format!("全部缺失组件（共 {} 项）", missing.len()).into());
                w.set_status_is_error(false);
                // 审查 Minor：node 不一定在缺失清单里（只缺 lark-cli 等时没有 node 步）——
                // 文案按实际清单条件化
                let head = if missing.iter().any(|id| id == "node") {
                    "先装 Node.js…"
                } else if crate::deps::find_in_path("npm").is_none() {
                    "先补装 Node.js（npm 缺失）…"
                } else {
                    "按缺失顺序安装…"
                };
                w.set_status_line(
                    format!("⏳ 一键安装开始：共 {} 项，{head}", missing.len()).into(),
                );
            }
            let _ = tx2.send(UiCmd::InstallAllMissing);
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

    {
        let work = work.clone();
        let sw = settings.as_weak();
        settings.on_access_option_toggled(move |_i| {
            let Some(w) = sw.upgrade() else { return };
            // #118：对话权限固定「仅授权用户」，点击仅触发重算回正（无公开档可写）
            refresh_exclusive_checks(&w, &work);
        });
    }
    {
        let work = work.clone();
        let sw = settings.as_weak();
        settings.on_ding_access_option_toggled(move |_i| {
            let Some(w) = sw.upgrade() else { return };
            // #118：同上——钉钉对话权限同样固定「仅授权用户」
            refresh_exclusive_checks(&w, &work);
        });
    }

    // 「取消授权」：从该 bot 授权者列表移除某用户（config 落盘 + 刷新列表）。
    {
        let work = work.clone();
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
                bot.granted_infos
                    .get(granted_idx as usize)
                    .map(|i| i.open_id.clone())
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
        let pwork = wk.providers.clone();
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
        // Jev 决策模型配置（授权者/受限会话门禁）：per-field 回写，与供应商同纪律。
        let jwork = wk.jev.clone();
        let dirty2 = dirty.clone();
        settings.on_jev_field_edited(move |field, value| {
            dirty2.set(true);
            let mut j = jwork.borrow_mut();
            match field.as_str() {
                "api_key" => j.api_key = value.to_string(), // 密钥不 trim
                "base_url" => j.base_url = value.trim().to_string(),
                "model" => j.model = value.trim().to_string(),
                _ => {}
            }
        });
    }
    {
        // 切中别的供应商：重建 model 强制编辑区刷新（同 bot 的 ComboBox current-value 陈旧坑）
        let pwork = wk.providers.clone();
        let pmodel = providers_model.clone();
        let dwork = wk.default_provider.clone();
        settings.on_provider_selection_changed(move |_idx| {
            let pv = pwork.borrow();
            let d = dwork.borrow();
            sync_providers_model(&pmodel, &pv, &d);
        });
    }
    {
        let pwork = wk.providers.clone();
        let pmodel = providers_model.clone();
        let dwork = wk.default_provider.clone();
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
        let pwork = wk.providers.clone();
        let pmodel = providers_model.clone();
        let dwork = wk.default_provider.clone();
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
        let pwork = wk.providers.clone();
        let pmodel = providers_model.clone();
        let dwork = wk.default_provider.clone();
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
        let pwork = wk.providers.clone();
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
        let restored =
            load_with_draft(&settings, &dirty, &work, &bots_model, &providers_model, &wk);
        // 已静默恢复草稿时保留恢复提示，别被「请先添加」覆盖
        if !restored {
            settings.set_status_is_error(false);
            settings.set_status_line("请先添加一个飞书/微信机器人".into());
        }
        show_window_and_focus(&settings);
    }

    // ── 系统级对话：气泡点击 → 对话框；发消息 → 后台 syschat 回合 ──────────────
    // 气泡拖动位置持久化：拖动会产生成串 Moved 事件，400ms 去抖后才写盘（避免每帧写文件）。
    // **坐标系**：winit 的 `Moved` 给**物理**像素，而 BubblePos/set_position 全用**逻辑**像素
    // （Retina 2x 下不换算会让存盘位置每次重启翻倍——评审 R3）。故这里先换逻辑再缓存。
    let bubble_pos_pending: Rc<Cell<Option<crate::syschat::BubblePos>>> = Rc::new(Cell::new(None));
    // A（卡 `abb-v11-bubble-follow-lag`）：跟随泵。事件路径（对话窗 Moved/Resized）与 macOS
    // 拖动期间的 16ms common-modes 定时器共用它；句柄在下面用 `spawn_local` 异步取得。
    let follow_pump: Rc<FollowPump> = Rc::new(FollowPump::default());
    #[cfg(target_os = "macos")]
    {
        use slint::winit_030::WinitWindowAccessor;
        let p = follow_pump.clone();
        let dw = syschat_win.as_weak();
        let bw = bubble.as_weak();
        // `winit_window()` 是异步的（窗口创建完成才 resolve）；拿到后：
        // ① 存句柄（事件路径改走句柄版本）；② 起 common-modes 定时器。
        // 定时器必须保活到进程结束（drop 即 invalidate）——用 `mem::forget`（与既有 `saver`
        // / 主定时器同款泄漏处理，见那两处注释）。
        let _ = slint::spawn_local(async move {
            let (Some(d), Some(b)) = (dw.upgrade(), bw.upgrade()) else {
                return;
            };
            let (Ok(dialog), Ok(bubble_w)) = (
                d.window().winit_window().await,
                b.window().winit_window().await,
            ) else {
                return; // 非 winit 后端（例如 SLINT_BACKEND=winit-software 以外的测试后端）：
                        // 保持事件路径的 Slint 版本即可
            };
            *p.handles.borrow_mut() = Some(follow_macos::Handles {
                dialog,
                bubble: bubble_w,
            });
            let p2 = p.clone();
            let timer = crate::platform::common_mode_timer(
                Duration::from_millis(FOLLOW_TICK_MS),
                move || {
                    let _ = p2.tick_handles();
                },
            );
            std::mem::forget(timer);
        });
    }
    {
        use slint::winit_030::{winit::event::WindowEvent, EventResult, WinitWindowAccessor};
        let pending = bubble_pos_pending.clone();
        // D4（卡 abb-syschat-d4-bubble-follow）：气泡自己的 `Moved` **只**做两件事 ——
        // ① 清按下态（原生移动会吞掉 release，否则 #8 的按下反馈会残留在气泡上）；
        // ② 记下位置待去抖存盘。
        //
        // 它**不再**反向驱动对话窗：D 批 #9 的「拖气泡 ⇒ 对话窗跟着动」已作废（owner 口径改为
        // 气泡始终贴对话窗四角），改由下面的去抖存盘把气泡吸回对话窗最近角。
        // 若保留旧写法，就会出现「气泡 Moved → 窗动 → 窗 Moved → 气泡动」的反馈环。
        let bw = bubble.as_weak();
        bubble.window().on_winit_window_event(move |w, ev| {
            if let WindowEvent::Moved(pos) = ev {
                pending.set(Some(crate::syschat::BubblePos::from_physical(
                    pos.x,
                    pos.y,
                    w.scale_factor(),
                )));
                if let Some(b) = bw.upgrade() {
                    b.set_pressed(false);
                }
            }
            // Windows：圆角需要 SetWindowRgn 硬裁剪（与设置窗同一半径）；其它平台是空操作。
            handle_platform_window_event(w, ev);
            EventResult::Propagate
        });
        let pending = bubble_pos_pending.clone();
        let saver_bw = bubble.as_weak();
        let saver_cw = syschat_win.as_weak();
        let saver = slint::Timer::default();
        saver.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(400),
            move || {
                let Some(raw) = pending.take() else { return };
                // D4：落盘前先把气泡吸到对话窗最近角 —— 这就是「拖气泡也吸附」的落点：
                // 拖动期间**不**吸附（否则与原生拖动抢窗口位置），松手后 Moved 流停止 ⇒
                // 本轮去抖到期 ⇒ 吸附 + 存盘（存的是吸附后的位置，下次开窗就锚在它旁边）。
                let mut to_save = raw;
                if let (Some(b), Some(d)) = (saver_bw.upgrade(), saver_cw.upgrade()) {
                    if d.window().is_visible() {
                        snap_bubble_to_dialog(&b, &d);
                        if let Some(r) = logical_window_rect(&b.window()) {
                            to_save = crate::syschat::BubblePos { x: r.0, y: r.1 };
                        }
                    }
                }
                crate::syschat::save_bubble_pos(to_save);
            },
        );
        // Timer 需保活到事件循环结束（drop 会取消；与 show_window_and_focus 同款泄漏处理）。
        std::mem::forget(saver);
    }
    // 请求后台下载当前会话里未缓存的 http(s) 媒体（图片/音视频）。
    let request_media = {
        let tx = tx.clone();
        move || {
            for u in pending_http_media() {
                let _ = tx.send(UiCmd::FetchMedia { url: u });
            }
        }
    };
    // 「打开系统对话窗」的唯一收口：装载历史 + 刷新下拉 + 置前显示 + 锚到气泡旁 + 拉媒体。
    // **两个入口共用**：点气泡、点 Dock（v10 稿 v8 第 4 条：Dock 点击只开对话窗，不再弹设置窗）。
    // 注：D5（不进 Dock）之后进程永远不落 Dock 图标 ⇒ 「点 Dock」这个入口实际不可达；
    // 接线留着不删（一旦重新引入 Dock 图标就又是对的，且它本身无害）。
    let open_syschat: Rc<dyn Fn()> = {
        let cw = syschat_win.as_weak();
        let model = syschat_model.clone();
        let sessions = sc_sessions.clone();
        let models = sc_models.clone();
        let rm = request_media.clone();
        let bw = bubble.as_weak();
        Rc::new(move || {
            let Some(w) = cw.upgrade() else { return };
            load_syschat_into(&w, &model);
            refresh_syschat_sel(&w, &sessions, &models);
            show_window_and_focus(&w);
            if let Some(b) = bw.upgrade() {
                // 开窗锚定：把对话窗摆到气泡旁（间距 10px，靠屏边翻边）。**只此一次** ——
                // 之后连续几何只剩「对话窗 ⇒ 气泡」（单一事实源见 `snap_bubble_to_dialog`）。
                if let Some((x, y)) = anchored_dialog_pos(b.window(), w.window()) {
                    w.window()
                        .set_position(slint::LogicalPosition::new(x as f32, y as f32));
                }
                // 开窗也校准一次气泡（贴到对话窗最近角）。若上面的 set_position 已经触发了
                // `Moved`，两次吸附都是幂等的（`snap_is_fixed_point_of_open_anchor` 守着开窗不动点）。
                snap_bubble_to_dialog(&b, &w);
                // v10（稿 v8 第 1 条）：气泡**常驻置顶**——开窗不再藏气泡。
                // 旧 #13（「开窗 ⇒ 藏气泡」）已作废；这里保留一次收口调用只是让开窗也把气泡
                // 拉回 config 真值（比如启动时窗口还没 realize 的帧）。
                refresh_bubble_visibility(&b);
            }
            rm();
        })
    };
    // 入口一：点气泡 → 开对话窗。
    {
        let open = open_syschat.clone();
        bubble.on_open_chat(move || open());
    }
    // 入口二：点 Dock 图标 → 只开对话窗（v10 稿 v8 第 4 条）。
    // 背景：no-frame 窗后系统标题栏窗口的默认 reopen 行为失效（2026-08-18 回归修复），
    // 但当时接到的是设置窗；owner 复查改口径为「只开对话窗」。
    #[cfg(target_os = "macos")]
    {
        let open = open_syschat.clone();
        crate::platform::install_dock_reopen(Box::new(move || open()));
    }
    // 新建会话（保留历史，旧会话可在「会话」下拉里切回）。
    {
        let cw = syschat_win.as_weak();
        let model = syschat_model.clone();
        let sessions = sc_sessions.clone();
        let models = sc_models.clone();
        let pump = sc_pump.clone();
        syschat_win.on_new_session(move || {
            let Some(w) = cw.upgrade() else { return };
            // 卡 d10：排队项属于旧会话，换会话不得跟着过去。
            pump.borrow_mut().clear();
            match crate::syschat::new_session(None) {
                Ok(_) => {
                    refresh_syschat_sel(&w, &sessions, &models);
                    load_syschat_into(&w, &model);
                    w.set_status_is_error(false);
                    w.set_status_line("已新建会话（旧会话在「会话」下拉里可切回）。".into());
                }
                Err(e) => {
                    w.set_status_is_error(true);
                    w.set_status_line(format!("新建会话失败：{e}").into());
                }
            }
        });
    }
    // 切换会话（历史保留）。
    {
        let cw = syschat_win.as_weak();
        let model = syschat_model.clone();
        let sessions = sc_sessions.clone();
        let models = sc_models.clone();
        let rm = request_media.clone();
        let pump = sc_pump.clone();
        syschat_win.on_session_pick(move |idx| {
            if idx < 0 {
                return;
            }
            let id = sessions.borrow().get(idx as usize).map(|s| s.id.clone());
            if let Some(id) = id {
                let Some(w) = cw.upgrade() else { return };
                // 卡 d10：排队项属于旧会话，切会话不得跟着过去。
                pump.borrow_mut().clear();
                match crate::syschat::switch_session(&id) {
                    Ok(()) => {
                        refresh_syschat_sel(&w, &sessions, &models);
                        load_syschat_into(&w, &model);
                        rm();
                    }
                    Err(e) => {
                        w.set_status_is_error(true);
                        w.set_status_line(format!("切换会话失败：{e}").into());
                    }
                }
            }
        });
    }
    // 切换模型（**按会话**；从下一条消息起生效，不改历史）。
    {
        let cw = syschat_win.as_weak();
        let sessions = sc_sessions.clone();
        let models = sc_models.clone();
        syschat_win.on_model_pick(move |idx| {
            if idx < 0 {
                return;
            }
            let name = models.borrow().get(idx as usize).map(|(n, _)| n.clone());
            if let Some(name) = name {
                let Some(w) = cw.upgrade() else { return };
                let cur = crate::syschat::current_session();
                match crate::syschat::set_session_provider(&cur, &name) {
                    Ok(()) => {
                        refresh_syschat_sel(&w, &sessions, &models);
                        w.set_status_is_error(false);
                        w.set_status_line(
                            format!("本会话模型已切到「{name}」（从下一条消息起生效）").into(),
                        );
                    }
                    Err(e) => {
                        w.set_status_is_error(true);
                        w.set_status_line(format!("切换模型失败：{e}").into());
                    }
                }
            }
        });
    }
    // 发送：用户消息立即上屏；空闲 ⇒ 起一轮，生成中 ⇒ 插入在途回合或排队（卡 d10）。
    //
    // 落点由纯逻辑 [`crate::syschat::SendPump`] 判定（单测钉住），UI 只负责把三种落点
    // 区分显示（已插入 / 已排队）并驱动对应的实际动作。输入框在生成中**保持可用**
    // （见 ui/app.slint 的 enabled/accepted），所以这里**不再 busy 直接 return**：
    // 空消息与纯附件由 `submit` 拒绝（防误发），防连点由“发送后清 draft”保证同一文本
    // 不会因一次击键被提交两次。
    //
    // 卡 d9：把这段主体提成 `submit_sc`——它是**唯一**的提交入口，手动发送（`on_send`）
    // 与定时到点（下方 2s 主定时器）共用，避免两条路径行为漂移（定时那条也走完整的
    // “上屏 + 落点判定 + 状态行”）。
    let submit_sc: Rc<dyn Fn(String)> = {
        let cw = syschat_win.as_weak();
        let model = syschat_model.clone();
        let atts = sc_atts.clone();
        let pending = sc_pending.clone();
        let pump = sc_pump.clone();
        let inject = sc_inject.clone();
        let start_turn = start_sc_turn.clone();
        Rc::new(move |text: String| {
            let text = text.trim().to_string();
            let Some(w) = cw.upgrade() else { return };
            // 本轮的待发附件（路径随 prompt 进 agent）；发送后清空待发列表——
            // 不删已落盘文件（同一附件可再次附加，且已发出的消息可能已引用它）。
            let atts_now: Vec<crate::syschat::Attachment> = atts.borrow().clone();
            let Some(route) = pump.borrow_mut().submit(&text, atts_now.clone()) else {
                return; // 空消息 / 纯附件：产品口径不发
            };
            // 三种落点都先把用户行上屏 + 清空输入区（用户要知道“我说过了”）。
            for r in rows_for("user", &text) {
                model.push(r);
            }
            w.set_draft("".into());
            atts.borrow_mut().clear();
            refresh_syschat_atts(&w, &atts, &pending);
            match route {
                crate::syschat::SendRoute::Immediate => {
                    start_turn(text, atts_now);
                }
                crate::syschat::SendRoute::Steer => {
                    // 支持 steer：写进在途回合（不进本地队列）。
                    let ok = inject
                        .borrow()
                        .as_ref()
                        .map(|tx| tx.send(crate::syschat::steer_inbound(&text)).is_ok())
                        .unwrap_or(false);
                    if ok {
                        w.set_status_is_error(false);
                        w.set_status_line("已插入本轮对话（怹会在本轮里看到）。".into());
                    } else {
                        // 竞态：回合刚收尾、注入端点已失效。补进队列防静默丢消息。
                        let n = pump.borrow_mut().enqueue_fallback(&text, atts_now.clone());
                        w.set_status_is_error(false);
                        w.set_status_line(
                            format!("已排队（本轮刚结束，将随队依次发送）· 队列 {n} 条").into(),
                        );
                    }
                }
                crate::syschat::SendRoute::Queue => {
                    let n = pump.borrow().pending_len();
                    w.set_status_is_error(false);
                    w.set_status_line(format!("已排队（本轮结束后发送）· 队列 {n} 条").into());
                }
            }
            w.set_scroll_epoch(w.get_scroll_epoch().wrapping_add(1));
        })
    };
    {
        let submit = submit_sc.clone();
        syschat_win.on_send(move |text| submit(text.to_string()));
    }
    // ⏹ 停止：取消在途回合（UI 线程直接触发外部取消令牌；回合收尾由 Stopped 事件处理）。
    {
        let cancel_slot = sc_cancel.clone();
        syschat_win.on_stop_streaming(move || {
            if let Some(t) = cancel_slot.borrow().as_ref() {
                t.cancel();
            }
        });
    }
    // 思考块展开/收起（E2 需求 #11）：点标题切换该行 `think_expanded` 并重设该行。
    // 默认折叠；**流式进行中**的思考块由 `live` 驱动展开（打字机可见），进正文/收尾后
    // 由下一次重建按 `live=false` 收起（重建会重算行，不跨重建携带手动态——已在
    // `sync_stream_rows` 注释里写明这个已知边界）。
    {
        let model = syschat_model.clone();
        let cw = syschat_win.as_weak();
        syschat_win.on_toggle_think(move |idx| {
            let i = idx.max(0) as usize;
            if let Some(mut r) = model.row_data(i) {
                if r.kind == "think" {
                    r.think_expanded = !r.think_expanded;
                    model.set_row_data(i, r);
                    if let Some(w) = cw.upgrade() {
                        w.set_scroll_epoch(w.get_scroll_epoch().wrapping_add(1));
                    }
                }
            }
        });
    }
    // 清空历史（对话框按钮）。
    {
        let cw = syschat_win.as_weak();
        let model = syschat_model.clone();
        let pump = sc_pump.clone();
        syschat_win.on_reset_history(move || {
            // 卡 d10：清空会话 ⇒ 排队项也一并清（否则会在空会话里冒出旧排队）。
            pump.borrow_mut().clear();
            match crate::syschat::clear_history() {
                Ok(()) => {
                    model.set_vec(Vec::new());
                    if let Some(w) = cw.upgrade() {
                        w.set_status_is_error(false);
                        w.set_status_line("已清空对话历史。".into());
                    }
                }
                Err(e) => {
                    if let Some(w) = cw.upgrade() {
                        w.set_status_is_error(true);
                        w.set_status_line(format!("清空失败：{e}").into());
                    }
                }
            }
        });
    }
    // 输入区占位按钮（只剩 ✂ 截图）：**不做成死按钮**——按下走既有状态行通道给一句「尚未实现」。
    // 画在 `ui/app.slint` 的 `.fnbar` 里，回调只带一个功能名。（😊 表情与 ⋯ 更多在卡 d9 已接
    // 真功能，见下方两段；截图需 macOS 屏幕录制 TCC 授权 + 原生采集，另立卡。）
    {
        let cw = syschat_win.as_weak();
        syschat_win.on_composer_todo(move |name| {
            if let Some(w) = cw.upgrade() {
                w.set_status_is_error(false);
                w.set_status_line(format!("「{name}」尚未实现（占位按钮）。").into());
            }
        });
    }
    // v11（卡 `abb-v11-panels-align`）：会话面板里的「🗄 归档…」。本卡只要求**面板里有这一项
    // 且可点**；归档本体是另一张卡（`abb-v11-session-archive`）——这里如实回一句状态，
    // 不做假动作（不假装已归档、也不删任何文件）。
    {
        let cw = syschat_win.as_weak();
        syschat_win.on_session_archive(move || {
            if let Some(w) = cw.upgrade() {
                w.set_status_is_error(false);
                w.set_status_line(
                    "「归档」尚未实现（归档本体见卡 abb-v11-session-archive）。".into(),
                );
            }
        });
    }
    // ── 卡 d9：😊 表情面板 ──────────────────────────────────────────────
    // 分组/翻页内容全在 `crate::emoji`（纯函数带单测）；这里只把结果写进窗口。
    // 插入口径：Slint 的 `TextInput` **没有光标 API**（只有 `text` 整串），所以选一个 emoji
    // 一律**追加到草稿末尾**（本卡口径）；选完**不关面板**，方便连续插多个。
    {
        let cw = syschat_win.as_weak();
        let cat = sc_emoji_cat.clone();
        let page = sc_emoji_page.clone();
        syschat_win.on_emoji_opened(move || {
            // 每次打开回到默认分组首页（避免上次翻到末页后再次打开是空的）。
            cat.set(0);
            page.set(0);
            if let Some(w) = cw.upgrade() {
                refresh_emoji_panel(&w, &cat, &page);
            }
        });
    }
    {
        let cw = syschat_win.as_weak();
        let cat = sc_emoji_cat.clone();
        let page = sc_emoji_page.clone();
        syschat_win.on_emoji_cat(move |i| {
            cat.set(crate::emoji::clamp_cat(i));
            page.set(0);
            if let Some(w) = cw.upgrade() {
                refresh_emoji_panel(&w, &cat, &page);
            }
        });
    }
    {
        let cw = syschat_win.as_weak();
        let cat = sc_emoji_cat.clone();
        let page = sc_emoji_page.clone();
        syschat_win.on_emoji_page(move |delta| {
            let c = cat.get();
            page.set(crate::emoji::clamp_page(c, page.get() as i32 + delta));
            if let Some(w) = cw.upgrade() {
                refresh_emoji_panel(&w, &cat, &page);
            }
        });
    }
    {
        let cw = syschat_win.as_weak();
        syschat_win.on_emoji_pick(move |e| {
            if let Some(w) = cw.upgrade() {
                let mut draft = w.get_draft().to_string();
                draft.push_str(e.as_str());
                w.set_draft(draft.into());
            }
        });
    }
    // ── 卡 d9：⋯ 「更多（定时）」菜单 ────────────────────────────────────
    // 菜单打开时刷新（档位文案要按当前钟点决定「今天/明天 18:00」，待发列表也可能被
    // 上次的到点触发改过）。
    {
        let cw = syschat_win.as_weak();
        let store = sc_sched.clone();
        syschat_win.on_more_opened(move || {
            if let Some(w) = cw.upgrade() {
                refresh_sched_menu(&w, &store);
            }
        });
    }
    // 档位 → 排程。口径：
    // - 只排**文本**（草稿）；带待发附件时**拒排**并给可见原因，绝不默默丢掉附件；
    // - 排成功后清空输入框（与「发送」同观感：消息已被接收，只是晚点发）；
    // - 失败（空草稿 / 时间解析失败）写状态行，不静默。
    {
        let cw = syschat_win.as_weak();
        let store = sc_sched.clone();
        let atts = sc_atts.clone();
        syschat_win.on_sched_preset(move |i| {
            let Some(w) = cw.upgrade() else { return };
            let Some(preset) = crate::syschat_sched::Preset::from_index(i) else {
                return;
            };
            let draft = w.get_draft().to_string();
            if draft.trim().is_empty() {
                w.set_status_is_error(true);
                w.set_status_line("定时发送失败：输入框为空。".into());
                return;
            }
            if !atts.borrow().is_empty() {
                w.set_status_is_error(true);
                w.set_status_line("定时发送暂不支持附件：请先移除待发附件，或直接发送。".into());
                return;
            }
            let at = preset.resolve(crate::schedule::DateTime::now());
            match store.add(&draft, &at) {
                Ok(m) => {
                    w.set_draft("".into());
                    w.set_status_is_error(false);
                    w.set_status_line(
                        format!(
                            "已定时发送：{}（{}）",
                            m.at,
                            crate::agent::truncate(&m.text, 20)
                        )
                        .into(),
                    );
                    refresh_sched_menu(&w, &store);
                }
                Err(e) => {
                    w.set_status_is_error(true);
                    w.set_status_line(format!("定时发送失败：{e}").into());
                }
            }
        });
    }
    // 取消单条（菜单里每条右侧的 ✕）：按 index 在当前列表中定位 id；取不到就是 no-op。
    {
        let cw = syschat_win.as_weak();
        let store = sc_sched.clone();
        syschat_win.on_sched_cancel(move |idx| {
            let Some(w) = cw.upgrade() else { return };
            let list = store.list();
            if let Some(m) = list.get(idx.max(0) as usize) {
                store.remove(&m.id);
                w.set_status_is_error(false);
                w.set_status_line(format!("已取消定时发送：{}。", m.at).into());
            }
            refresh_sched_menu(&w, &store);
        });
    }
    // 取消全部。
    {
        let cw = syschat_win.as_weak();
        let store = sc_sched.clone();
        syschat_win.on_sched_cancel_all(move || {
            if let Some(w) = cw.upgrade() {
                let n = store.remove_all();
                w.set_status_is_error(false);
                w.set_status_line(format!("已取消全部定时发送（{n} 条）。").into());
                refresh_sched_menu(&w, &store);
            }
        });
    }
    // ③ ⚙设置（D2 稿需求 #3）：唤起**既有**设置窗。照托盘「设置…」的同一条路径写：
    // 先 `load_with_draft`（草稿比正式配置新时静默恢复为工作底稿）→ 刷新状态行 → show+聚焦。
    // 不自己写一套显示逻辑：草稿恢复/置前/圆角都在这条路上有既有口径。
    {
        let sw = settings.as_weak();
        let work = work.clone();
        let dirty = dirty.clone();
        let model = bots_model.clone();
        let pmodel = providers_model.clone();
        let wk = wk.clone();
        syschat_win.on_open_settings(move || {
            if let Some(w) = sw.upgrade() {
                load_with_draft(&w, &dirty, &work, &model, &pmodel, &wk);
                push_settings_status(&w, &install::status());
                show_window_and_focus(&w);
            }
        });
    }
    // 音视频媒体卡 / 图片回退「打开」：本地文件用系统默认播放器打开，链接用浏览器。
    // 本地路径与图片内联、普通「打开」共用同一基准（[`crate::syschat::resolve_media_path`]）。
    syschat_win.on_open_media(|src| {
        let s = src.as_str();
        if s.starts_with("http://") || s.starts_with("https://") {
            platform::open_url(s);
        } else {
            platform::open_path(&crate::syschat::resolve_media_path(s));
        }
    });
    // 普通链接 / 图片回退：http(s)/mailto 等交浏览器；`file://` 与无 scheme 的本地路径
    // 按同一工作区基准解析后用系统「打开」（否则相对路径链接必打不开）。
    syschat_win.on_open_link(|url| {
        let s = url.as_str();
        let external = !s.starts_with("file://")
            && (s.contains("://") || s.starts_with("mailto:") || s.starts_with("tel:"));
        if external {
            platform::open_url(s);
        } else {
            platform::open_path(&crate::syschat::resolve_media_path(s));
        }
    });
    // 关闭对话框 = 隐藏（托盘应用惯例；气泡仍在，随时可再打开）。
    // 隐藏路径收口在 [`hide_syschat`]（✕ / 红点 / D5 失焦三条路径共用）。
    {
        use slint::winit_030::{
            winit::{event::ElementState, event::WindowEvent, keyboard::Key},
            EventResult, WinitWindowAccessor,
        };
        let cw = syschat_win.as_weak();
        let sw = settings.as_weak();
        let bw_close = bubble.as_weak();
        syschat_win.on_close_window(move || {
            if let Some(w) = cw.upgrade() {
                hide_syschat(&w, &bw_close, &sw);
            }
        });
        let cw2 = syschat_win.as_weak();
        let sw2 = settings.as_weak();
        let bw_close2 = bubble.as_weak();
        // C1：⌘V —— 剪贴板里是**文件**时拦截为附件，否则原样放行（文本粘贴仍交 TextInput）。
        //
        // 为什么走 winit 事件过滤器而不是 Slint 的 `key-pressed`：
        // ① 判定必须**同步**问 `NSPasteboard`（Rust 侧）。Slint 侧要拦截得新增一个带返回值的
        //    跨语言回调，且拦截元素必须位于输入框的祖先链上（要改输入区布局）；
        // ② winit 过滤器在 Slint 的按键路由**之前**执行，`PreventDefault` 干净吞掉这一次按键，
        //    `Propagate` 则完全不影响既有行为（TextInput 的 ⌘V 文本粘贴由此保留）；
        // ③ 本就在 UI 主线程（AppKit 前置条件）。
        // winit 0.30 的 `KeyEvent` 自身不带修饰键 ⇒ 用 `ModifiersChanged` 跟踪 ⌘/Ctrl 的按下态。
        let super_held = Rc::new(Cell::new(false));
        // C2 拖放状态机（去重口径 / 高亮收尾见 `syschat::DropTracker`）：**同一个闭包**里处理，
        // 不得另起一个 `on_winit_window_event`——那是单槽 `set`，会把这条整个顶掉。
        let drop_tracker = Rc::new(RefCell::new(crate::syschat::DropTracker::new()));
        // D5：失焦隐藏的延时器（单槽：start 会覆盖上一次待执行的隐藏）。
        // 它被闭包持有 ⇒ 与窗口同生命周期（不能在闭包外 drop，否则回调立即取消）。
        let blur_timer = slint::Timer::default();
        let land_paste = land_into_pending.clone();
        let land_drop = land_into_pending.clone();
        let drop_ev = drop_tracker.clone();
        // D4：气泡跟随用的弱引用（对话窗 Moved/Resized → 气泡贴角）。
        let follow_bubble = bubble.as_weak();
        let follow_pump_ev = follow_pump.clone();
        syschat_win.window().on_winit_window_event(move |w, ev| {
            // C2 拖放（Finder → 对话窗）：**不需要任何 AppKit 手写代码**——winit 0.30 的 macOS
            // 实现自己就是 `NSDraggingDestination`（`window_delegate.rs:367`），已把系统拖放翻译成
            // `HoveredFile` / `DroppedFile` / `HoveredFileCancelled`（注册见 `:667`）。
            match ev {
                WindowEvent::HoveredFile(_) => {
                    let h = {
                        let mut t = drop_ev.borrow_mut();
                        t.hovered();
                        t.hovering()
                    };
                    if let Some(win) = cw2.upgrade() {
                        win.set_drag_hover(h);
                    }
                    return EventResult::Propagate;
                }
                WindowEvent::HoveredFileCancelled => {
                    drop_ev.borrow_mut().cancelled();
                    if let Some(win) = cw2.upgrade() {
                        win.set_drag_hover(false);
                    }
                    return EventResult::Propagate;
                }
                WindowEvent::DroppedFile(path) => {
                    // 成功 drop 后 winit **不发** `HoveredFileCancelled`，高亮在这里自己收。
                    let should_land = drop_ev.borrow_mut().dropped(path);
                    if let Some(win) = cw2.upgrade() {
                        win.set_drag_hover(false);
                    }
                    if should_land {
                        land_drop(vec![path.clone()]);
                    }
                    return EventResult::Propagate;
                }
                _ => {}
            }
            if let WindowEvent::ModifiersChanged(m) = ev {
                super_held.set(m.state().super_key());
            }
            if let WindowEvent::KeyboardInput { event, .. } = ev {
                if super_held.get()
                    && event.state == ElementState::Pressed
                    && !event.repeat
                    && matches!(&event.logical_key, Key::Character(s) if s.eq_ignore_ascii_case("v"))
                    && cw2.upgrade().map(|w| !w.get_busy()).unwrap_or(false)
                {
                    let paths = platform::pasteboard_file_paths();
                    if !paths.is_empty() {
                        // 落地在 UI 线程同步完成（否则待发列表要跨线程共享）；典型附件很小，
                        // 大可接受（若将来出现「粘大文件卡 UI」的反馈，再改成队列 + 单发定时器）。
                        land_paste(paths);
                        return EventResult::PreventDefault; // 不落给 TextInput ⇒ 不会粘文本
                    }
                    // 剪贴板无文件：继续往下，文本粘贴仍由 Slint TextInput 处理。
                }
            }
            if matches!(ev, WindowEvent::CloseRequested) {
                if let Some(w) = cw2.upgrade() {
                    hide_syschat(&w, &bw_close2, &sw2);
                }
            }
            // D4：对话窗 `Moved` / `Resized`（含最大化、手动缩放）→ 气泡贴到最近的角。
            // 与 ⌘V / 拖放 / 红点关闭 / 失焦隐藏共用一个钩子（`on_winit_window_event` 是单槽 set）。
            // 这是「跟随」**唯一的事件源**（气泡自己的 Moved 不反向驱动对话窗，详见
            // `handle_syschat_geometry_event` / `snap_bubble_to_dialog` 的防环说明）。
            handle_syschat_geometry_event(ev, &follow_pump_ev, &follow_bubble, &cw2);
            // D5：**失焦即隐藏**（接线在 [`handle_syschat_focus_event`]，那里有判据/延时的完整理由）。
            // ⚠️ 必须挂在这个**已有**的钩子上：`on_winit_window_event` 是单槽 `set`，
            // 另起一个会把 ⌘V / Finder 拖放 / 红点关闭整条顶掉（且不会报错）。
            handle_syschat_focus_event(ev, &blur_timer, &cw2, &bw_close2, &sw2);
            // Windows：圆角 SetWindowRgn 硬裁剪（与设置窗同半径）；其它平台空操作。
            handle_platform_window_event(w, ev);
            EventResult::Propagate
        });
    }

    // ── 系统级对话流式事件：专用 50ms 定时器（E1/E2）──
    //
    // 为什么不用 2s 主定时器：打字机需要分片尽快上屏，2s 一拍只能看到「一跳一跳」；
    // 50ms 的 `try_recv` 几乎无开销（空轮询就是一次原子检查）。
    //
    // E1/E2 口径：
    // - 分片（正文 `Text` / 思考 `Thought`）都进同一个分段累积器，按 kind 分段；
    // - **客户端打字机**（`syschat::Typewriter`）每拍把已收到的文本逐段推上屏——
    //   实测 `abb-agent` 在 `MessageEnd` 整条发正文（一个回合常常只有 1～2 片），
    //   不做这层补字就会「一次性蹦出」；
    // - `Reply` 不立刻收尾：先让打字机把已收到文本推完（`StreamEnd::Reply`）
    //   再整段重建（以 `final_text` 为唯一权威）；`Stopped` 已 `flush`，当拍收尾。
    {
        let syschat_model_t = syschat_model.clone();
        let win_w = syschat_win.as_weak();
        let bubble_w = bubble.as_weak();
        let rm = request_media.clone();
        let stream_state = sc_stream.clone();
        let stream_start = sc_stream_start.clone();
        let stream_end = sc_stream_end.clone();
        let tw_text = sc_tw_text.clone();
        let tw_think = sc_tw_think.clone();
        let cancel_slot = sc_cancel.clone();
        // 卡 d10：能力 watch（每拍同步进泵）+ 串行泵 + 开下一轮的入口。
        let pump = sc_pump.clone();
        let cap_slot = sc_cap.clone();
        let start_turn = start_sc_turn.clone();
        let clock = std::time::Instant::now();
        let syschat_timer = slint::Timer::default();
        syschat_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(50),
            move || {
                let now = clock.elapsed().as_secs_f64();
                // 卡 d10：把本轮 agent 的 steer 能力同步进泵（initialize 后从 Unknown 落锤）。
                // 每拍读一次 watch 只是原子读，不引入额外开销。
                if let Some(rx) = cap_slot.borrow().as_ref() {
                    let cap = *rx.borrow();
                    pump.borrow_mut().set_support(cap);
                }
                let mut dirty = false;
                while let Ok(evt) = syschat_rx.try_recv() {
                    match evt {
                        SysChatEvt::Delta { kind, text } => {
                            let mut b = stream_state.borrow_mut();
                            let changed = b.push(kind, &text, now);
                            drop(b);
                            if !changed {
                                continue; // 空 delta：无变化，不重渲染
                            }
                            match kind {
                                crate::runtime::acp::ChunkKind::Text => {
                                    tw_text.borrow_mut().push(&text)
                                }
                                crate::runtime::acp::ChunkKind::Thought => {
                                    tw_think.borrow_mut().push(&text)
                                }
                            }
                            dirty = true;
                        }
                        SysChatEvt::Reply(text) => {
                            // 去重口径的权威侧：先把累积器正文换成最终文本（思考段保留），
                            // 打字机目标同换；收尾等它推完（本拍末尾判定）。
                            let mut b = stream_state.borrow_mut();
                            b.finish(&text, now);
                            drop(b);
                            tw_text.borrow_mut().set_target(&text);
                            *stream_end.borrow_mut() = Some(StreamEnd::Reply(text));
                        }
                        SysChatEvt::Stopped => {
                            // 停止：保留已生成部分（思考与正文各自保留）——`flush` 把已收到的都显示出来，
                            // 不留「收到了但没上屏」的字；随后当拍收尾。
                            stream_state.borrow_mut().stop();
                            tw_text.borrow_mut().flush();
                            tw_think.borrow_mut().flush();
                            *stream_end.borrow_mut() = Some(StreamEnd::Stopped);
                            dirty = true;
                        }
                        SysChatEvt::Error(e) => {
                            // 失败：已生成部分保留（打字机 `flush` 收口；**不动锚点**，免得带内容的下一次
                            // 重建在末尾重复锚出一条），只报错并收 busy。
                            tw_text.borrow_mut().flush();
                            tw_think.borrow_mut().flush();
                            *stream_end.borrow_mut() = None;
                            *cancel_slot.borrow_mut() = None;
                            if let Some(w) = win_w.upgrade() {
                                w.set_busy(false);
                                w.set_status_is_error(true);
                                w.set_status_line(format!("失败：{e}").into());
                            }
                            if let Some(b) = bubble_w.upgrade() {
                                b.set_busy(false);
                            }
                            // 卡 d10：本轮失败 ⇒ 同样排空队列（一次只跑一轮，依次发）。
                            if let Some(next) = pump.borrow_mut().finish() {
                                start_turn(next.text, next.atts);
                            }
                        }
                        SysChatEvt::MediaReady => {
                            // 后台下好一个 http(s) 媒体：重载当前会话，图片即可内联显示。
                            if let Some(w) = win_w.upgrade() {
                                load_syschat_into(&w, &syschat_model_t);
                                // 评审 B1：整体重载 model 后流式气泡的行区间**必须**复位——本事件
                                // 不受 busy 门控，可能正好落在下一轮流式进行中（陈旧起点 ⇒ 重复气泡）。
                                // 复位后下一次 delta 会以累积全文重新锚一条，自愈。
                                stream_start.set(usize::MAX);
                            }
                        }
                    }
                }
                // 每拍都推进打字机（不依赖是否有新分片）：这是「逐段上屏」的动力。
                if tw_text.borrow_mut().tick() {
                    dirty = true;
                }
                if tw_think.borrow_mut().tick() {
                    dirty = true;
                }
                // 结局判定：Reply 等两路打字机都推完；Stopped 已 flush，立即收尾。
                let end = stream_end.borrow().clone();
                if let Some(end) = end {
                    let drained = tw_text.borrow().done() && tw_think.borrow().done();
                    if drained || end == StreamEnd::Stopped {
                        // 收尾：以最终文本（Reply）或已 flush 的显示文本（Stopped）重建同一段。
                        let final_text = match &end {
                            StreamEnd::Reply(t) => t.clone(),
                            StreamEnd::Stopped => tw_text.borrow().shown().to_string(),
                        };
                        // 停止态由累积器记录（`stop()` 已设）：决定状态行文案与无锚点补行口径。
                        let stopped = stream_state.borrow().is_stopped();
                        // 评审 B1：先判锚点失效，否则会用陈旧起点**删掉真实历史行**（串位）。
                        if crate::syschat::stream_anchor_stale(
                            stream_start.get(),
                            syschat_model_t.row_count(),
                        ) {
                            stream_start.set(usize::MAX);
                        }
                        let s0 = stream_start.get();
                        let (has_thought, elapsed) = {
                            let b = stream_state.borrow();
                            (
                                b.has_thought(),
                                crate::syschat::format_elapsed(b.thought_secs(now)),
                            )
                        };
                        let thought = tw_think.borrow().shown().to_string();
                        let rows = stream_rows(&final_text, has_thought, &thought, &elapsed, false);
                        if s0 != usize::MAX {
                            sync_stream_rows(&syschat_model_t, s0, rows);
                        } else if !stopped && (!final_text.is_empty() || has_thought) {
                            // 回合没有任何已锚定分片就结束（极端）：直接补一条，别丢回复。
                            for r in rows {
                                syschat_model_t.push(r);
                            }
                        }
                        stream_start.set(usize::MAX);
                        *stream_end.borrow_mut() = None;
                        *cancel_slot.borrow_mut() = None;
                        rm();
                        if let Some(w) = win_w.upgrade() {
                            w.set_busy(false);
                            w.set_status_is_error(false);
                            if !stopped {
                                w.set_status_line("与怹直接对话（系统级 · 整机权限）".into());
                            } else {
                                w.set_status_line("已停止。".into());
                            }
                            w.set_scroll_epoch(w.get_scroll_epoch().wrapping_add(1));
                        }
                        if let Some(b) = bubble_w.upgrade() {
                            b.set_busy(false);
                        }
                        // 卡 d10：本轮结束（完成/停止）⇒ 排空队列，依次开下一轮。
                        // 停止也走这里（finish 是唯一出队口）：已入队的消息不因 ⏹ 而丢，
                        // 与卡口径一致（要清队必须显式 clear，如清空会话时）。
                        if let Some(next) = pump.borrow_mut().finish() {
                            start_turn(next.text, next.atts);
                        }
                        dirty = false;
                    } else {
                        // 还没推完：本拍仍要按打字机已显示的前缀重建。
                        dirty = true;
                    }
                }
                if dirty {
                    // 流式进行中：重建已显示前缀（含思考块/正文块）。
                    if crate::syschat::stream_anchor_stale(
                        stream_start.get(),
                        syschat_model_t.row_count(),
                    ) {
                        stream_start.set(usize::MAX);
                    }
                    if stream_start.get() == usize::MAX {
                        stream_start.set(syschat_model_t.row_count());
                    }
                    let (has_thought, elapsed, live) = {
                        let b = stream_state.borrow();
                        let live = b.is_active()
                            && b.segments().last().map(|s| s.kind)
                                == Some(crate::runtime::acp::ChunkKind::Thought);
                        (
                            b.has_thought(),
                            crate::syschat::format_elapsed(b.thought_secs(now)),
                            live,
                        )
                    };
                    let thought = tw_think.borrow().shown().to_string();
                    let text = tw_text.borrow().shown().to_string();
                    let rows = stream_rows(&text, has_thought, &thought, &elapsed, live);
                    sync_stream_rows(&syschat_model_t, stream_start.get(), rows);
                    if let Some(w) = win_w.upgrade() {
                        w.set_scroll_epoch(w.get_scroll_epoch().wrapping_add(1));
                    }
                }
            },
        );
        // Timer 需保活到事件循环结束（drop 会取消；与主定时器同款泄漏处理）。
        std::mem::forget(syschat_timer);
    }

    // ── 主线程定时器：① 看门 ② 刷新托盘 ③ 抽 bot_rx/wx_rx 回填 ──
    let timer = slint::Timer::default();
    {
        let settings_weak = settings.as_weak();
        let vb_dialog_weak = vb_dialog.as_weak();
        let team_dialog_weak = team_dialog.as_weak(); // #141 团队弹窗
        let vb_edit_t = vb_edit.clone();
        let qr_weak = qr_dialog.as_weak();
        let work = work.clone();
        let model = bots_model.clone();
        let tray_hold = tray;
        let tray_weak = tray_hold.as_weak();
        let dirty = dirty.clone();
        // #74 未读提醒：弹窗句柄 + 防重弹标记 + 5s 自动收起定时器（每次弹出 restart）
        let notif_weak = notifications.as_weak();
        let notif_showing = notif_showing.clone();
        let notif_timer = notif_timer.clone();
        // P8c：后台线程只在 `svc_toast` 里放一句话，这里负责弹（见其定义处注释）。
        let svc_toast = svc_toast.clone();
        // 虚拟 Bot 确认弹窗 weak（#75：解散/取消登记结果回填——成功关窗/失败可见）
        let vb_confirm_weak = vb_confirm.as_weak();
        // 确认弹窗待执行操作（失败重试时保留；成功才 take 清空）
        let vb_action_t = vb_action.clone();
        // #74 历史记录页 model + 选中快照（历史页打开时每 tick 刷新）
        let history_sessions = sessions_model.clone();
        let history_msgs = msgs_model.clone();
        let history_selected = selected_chat.clone();
        // 气泡 weak（气泡位置/忙态回填）
        let bubble_weak = bubble.as_weak();
        // 气泡位置待施加标记（见启动处注释）
        let bubble_pos_apply_t = bubble_pos_apply.clone();
        // 卡 d9：定时发送到点——复用托盘进程**既有的** 2s 主定时器（不新增线程/不新增依赖）。
        // 到点项由 `take_due` 取出并**同时揭除**（防重复触发），再走与手动发送同一个
        // `submit_sc` 入口（上屏 + 落点判定 + 状态行）；发出失败由 submit_sc 自身的状态行可见。
        let sched_t = sc_sched.clone();
        let submit_t = submit_sc.clone();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(2),
            {
                let wk = wk.clone();
                move || {
                    let _keep = &tray_hold;
                    // 卡 d9：先把到点的定时消息发出去（分钟粒度；2s 一拍 = 最多晚 2s）。
                    for m in sched_t.take_due(crate::schedule::DateTime::now()) {
                        crate::log!("[syschat] 定时发送触发：{}（{} 字）", m.at, m.text.chars().count());
                        submit_t(m.text);
                    }
                    // P8c：启动/停止/重启服务失败 → 弹一次可见提示（与未读提醒同一套 toast + 5s 收起）
                    let pending_toast = svc_toast.lock().ok().and_then(|mut s| s.take());
                    if let Some(msg) = pending_toast {
                        if let Some(n) = notif_weak.upgrade() {
                            n.set_items(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
                                NotifyRow {
                                    sender: "ABB".into(),
                                    bot: "服务".into(),
                                    preview: msg.into(),
                                    time: "".into(),
                                },
                            ]))));
                            show_notifications_window(&n);
                            notif_showing.set(true);
                            let nw2 = notif_weak.clone();
                            let sw2 = settings_weak.clone();
                            let sh2 = notif_showing.clone();
                            notif_timer.start(
                                slint::TimerMode::SingleShot,
                                Duration::from_secs(5),
                                move || hide_notifications_window(&nw2, &sw2, &sh2),
                            );
                        }
                    }
                    // 每 tick 只查一次进程状态（install::status 内部 fork ps，别重复查）
                    let st = install::status();
                    // 看门：意图=运行但进程不在 → 崩溃，重拉（用户手动停止会清 desired，不覆盖）
                    if install::is_desired() && !st.running {
                        crate::log!("[watchdog] service 意外退出，自动重拉");
                        // 反评 R29 P3：自动重拉失败也不能零留痕（这是全仓最后一处 `let _ =` 丢弃 Err）。
                        // 2026-09-30（abb-mac-svc-supervised-start-20260930）：托管形态下这次
                        // 「拉起来」会 spawn 外部命令，而本 tick 跑在 **UI 线程** —— 实测存在命令
                        // 长时间不返回的形态（EX_CONFIG 的 job 上 `launchctl kickstart` >20s 挂住，
                        // 见 platform.rs 的 LAUNCHCTL_TIMEOUT），同步做就会「bridge 起不来」连带
                        // 「托盘冻死」（评审 R1 的 B1）。故两边都保留：放后台线程 + 失败留痕；
                        // 起没起来由下一拍读 pid 文件复验（install::status）。
                        std::thread::spawn(|| {
                            if let Err(e) = install::svc_start() {
                                crate::log!("[watchdog] 自动重拉失败：{e:#}");
                            }
                        });
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
                        let (draft, _dropped) = snapshot_config(&work, &wk);
                        if let Err(e) = draft.save_draft() {
                            crate::log!("[gui] 草稿自动保存失败: {e:#}");
                        }
                    }
                // 依赖安装结果：清 dep-busy，刷新检测状态，报结果
                while let Ok(evt) = dep_rx.try_recv() {
                    if let Some(w) = settings_weak.upgrade() {
                        match evt {
                            DepEvt::Done { dep_id, result } => {
                                w.set_dep_busy("".into());
                                push_deps_to_window(&w);
                                match result {
                                    Ok(_tail) => {
                                        w.set_dep_detail("".into());
                                        w.set_status_is_error(false);
                                        // 安装输出（npm/brew）冗长不上状态行，只回完成态
                                        w.set_status_line(format!("✅ {dep_id} 安装完成").into());
                                    }
                                    Err(e) => {
                                        // 分类 + 引导进详情区（普通用户可操作）；自动切到
                                        // 环境配置页——失败即所见，不再让用户自己找错误区。
                                        let f = crate::deps::classify_fail(&dep_id, &e);
                                        // 与 AllDone 分支同格式（审查 M5：两处渲染格式统一）
                                        w.set_dep_detail(format!(
                                            "{}\n【怎么办】{}\n（原始错误：{}）",
                                            f.id, f.advice, f.raw
                                        )
                                        .into());
                                        w.set_status_is_error(true);
                                        w.set_status_line(format!(
                                            "⚠️ {dep_id} 安装失败：{}",
                                            f.advice
                                        )
                                        .into());
                                        w.set_dep_failed_count(1);
                                        w.set_current_page(2);
                                    }
                                }
                            }
                            DepEvt::AllProgress { label, idx, total } => {
                                w.set_dep_busy(format!("{label}（第 {idx}/{total} 项）").into());
                            }
                            DepEvt::AllDone(outcome) => {
                                w.set_dep_busy("".into());
                                push_deps_to_window(&w); // 重检测：卡片/首页计数自动刷新
                                let failed = !outcome.failed.is_empty();
                                if failed {
                                    // 逐项分类 + 引导进详情区（普通用户可操作）；
                                    // 自动切到环境配置页——失败即所见。
                                    let detail = outcome
                                        .failed
                                        .iter()
                                        .map(|(id, e)| {
                                            let f = crate::deps::classify_fail(id, e);
                                            format!("{id}\n【怎么办】{}\n（原始错误：{}）", f.advice, f.raw)
                                        })
                                        .collect::<Vec<_>>()
                                        .join("\n\n");
                                    w.set_dep_detail(detail.into());
                                    w.set_status_is_error(true);
                                    w.set_status_line(format!(
                                        "⚠️ 一键安装完成：成功 {} 项，失败 {} 项（点「重试」或按错误区指引处理）",
                                        outcome.ok.len(),
                                        outcome.failed.len()
                                    )
                                    .into());
                                    w.set_dep_failed_count(outcome.failed.len() as i32);
                                    w.set_current_page(2);
                                } else {
                                    w.set_dep_detail("".into());
                                    w.set_status_is_error(false);
                                    w.set_dep_failed_count(0);
                                    let line = crate::deps::format_all_summary(&outcome);
                                    w.set_status_line(line.into());
                                }
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
                // #141 一键创建团队结果：生成回填方案预览；建群回填创建清单
                while let Ok(evt) = team_rx.try_recv() {
                    match evt {
                        TeamEvt::Generate { result } => {
                            let team_plan = team_plan.clone();
                            if let Some(d) = team_dialog_weak.upgrade() {
                                d.set_busy(false);
                                match result {
                                    Ok(plan_json) => {
                                        *team_plan.borrow_mut() = Some(plan_json.clone());
                                        if let Ok(plan) =
                                            serde_json::from_str::<crate::teambuilder::TeamPlan>(
                                                &plan_json,
                                            )
                                        {
                                            d.set_team_name(plan.team_name.clone().into());
                                            d.set_flow(
                                                plan.collab.clone().unwrap_or_default().into(),
                                            );
                                            let rows: Vec<TeamRoleRow> = team_plan_rows(&plan)
                                                .into_iter()
                                                .map(|(rn, _member, duty)| TeamRoleRow {
                                                    role_name: rn.into(),
                                                    member: String::new().into(),
                                                    duty: duty.into(),
                                                })
                                                .collect();
                                            d.set_roles(slint::ModelRc::from(Rc::new(
                                                slint::VecModel::from(rows),
                                            )));
                                            d.set_mode(1);
                                        }
                                    }
                                    Err(e) => {
                                        // 生成失败：留在目标输入可重试（错误显示在 flow 行）
                                        d.set_team_name("生成失败".into());
                                        d.set_flow(e.into());
                                        d.set_mode(0);
                                    }
                                }
                            }
                        }
                        TeamEvt::Create { result } => {
                            if let Some(d) = team_dialog_weak.upgrade() {
                                d.set_busy(false);
                                match result {
                                    Ok(rows) => {
                                        let results: Vec<TeamResultRow> = rows
                                            .into_iter()
                                            .map(|(rn, _member, ok, detail)| TeamResultRow {
                                                text: team_create_line(&rn, &detail).into(),
                                                ok,
                                            })
                                            .collect();
                                        d.set_results(slint::ModelRc::from(Rc::new(
                                            slint::VecModel::from(results),
                                        )));
                                        d.set_mode(2);
                                    }
                                    Err(e) => {
                                        // 建群整体失败：留在预览可重试
                                        d.set_flow(e.into());
                                        d.set_mode(1);
                                    }
                                }
                            }
                            // #147：建群完成 → 刷新团队列表（GUI 与聊天入口同源，热读即同步）
                            if let Some(w) = settings_weak.upgrade() {
                                refresh_team_rows(&w, &work);
                                refresh_vb_rows(&w, &work);
                            }
                        }
                        // #147 团队管理（任命/解散）结果：状态行展示；解散后同时刷新登记列表
                        TeamEvt::Manage { result } => {
                            if let Some(w) = settings_weak.upgrade() {
                                match &result {
                                    Ok(msg) => {
                                        w.set_status_is_error(false);
                                        w.set_status_line(msg.clone().into());
                                    }
                                    Err(e) => {
                                        w.set_status_is_error(true);
                                        w.set_status_line(e.clone().into());
                                    }
                                }
                                refresh_team_rows(&w, &work);
                                refresh_vb_rows(&w, &work);
                            }
                            // 解散团队确认弹窗回填（与虚拟 Bot Done 同款纪律：成功才关、
                            // 失败保留 action 可重试；任命走 AppointDialog 不涉及这里）
                            if let Some(c) = vb_confirm_weak.upgrade() {
                                if c.get_busy() {
                                    match &result {
                                        Ok(msg) => {
                                            c.set_busy(false);
                                            c.set_failed(false);
                                            c.set_message(msg.clone().into());
                                            c.set_confirm_text("知道了".into());
                                            team_action.borrow_mut().take();
                                        }
                                        Err(e) => {
                                            c.set_busy(false);
                                            c.set_failed(true);
                                            c.set_message(e.clone().into());
                                            c.set_confirm_text("重试".into());
                                            // action 保留：再点「重试」重发同一解散
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // 虚拟 Bot 操作结果（#75）：进度 → 弹窗；结束 → 弹窗逐项汇总 + 刷新登记列表
                while let Ok(evt) = vb_rx.try_recv() {
                    match evt {
                        VirtualBotEvt::Progress { done, total } => {
                            if let Some(d) = vb_dialog_weak.upgrade() {
                                d.set_busy(true);
                                d.set_progress_text(
                                    format!("创建中 {}/{}", done + 1, total).into(),
                                );
                            }
                        }
                        VirtualBotEvt::Done { results } => {
                            if let Some(d) = vb_dialog_weak.upgrade() {
                                d.set_busy(false);
                                d.set_progress_text("".into());
                                let mut lines: Vec<slint::SharedString> =
                                    Vec::with_capacity(results.len());
                                for (name, r) in &results {
                                    match r {
                                        Ok(s) => lines.push(format!("✅ {name}：{s}").into()),
                                        Err(e) => lines.push(format!("❌ {name}：{e}").into()),
                                    }
                                }
                                d.set_results(slint::ModelRc::from(Rc::new(
                                    slint::VecModel::from(lines),
                                )));
                            }
                            // 确认弹窗执行中（解散/取消登记）→ 回填结果（8-20 用户反馈）：
                            // 成功 → 清 action + 弹窗显示成功（点「知道了」手动关）；
                            // 失败 → **保留 action** + 主按钮变「重试」（再点重发同一操作），
                            // 取消按钮禁用——成功才能正常关闭（红点 X 是放弃出口）。
                            if let Some(c) = vb_confirm_weak.upgrade() {
                                if c.get_busy() {
                                    c.set_busy(false);
                                    let all_ok = results.iter().all(|(_, r)| r.is_ok());
                                    let lines: Vec<String> = results
                                        .iter()
                                        .map(|(n, r)| match r {
                                            Ok(s) => format!("✅ {n}：{s}"),
                                            Err(e) => format!("❌ {n}：{e}"),
                                        })
                                        .collect();
                                    if all_ok {
                                        vb_action_t.borrow_mut().take();
                                        c.set_failed(false);
                                        c.set_title_text("操作成功".into());
                                        c.set_confirm_text("知道了".into());
                                        c.set_cancel_text("关闭".into());
                                    } else {
                                        c.set_failed(true);
                                        c.set_title_text("操作失败".into());
                                        c.set_confirm_text("重试".into());
                                        c.set_cancel_text("取消".into());
                                    }
                                    c.set_message(lines.join("\n").into());
                                }
                            }
                            if let Some(w) = settings_weak.upgrade() {
                                // 刷新同步结束：复位按钮（刷新期间 vb-syncing=true）
                                w.set_vb_syncing(false);
                                let ok = results
                                    .iter()
                                    .filter(|(_, r)| r.is_ok())
                                    .count();
                                let fail = results.len() - ok;
                                w.set_status_is_error(fail > 0);
                                w.set_status_line(
                                    format!("虚拟 Bot：成功 {ok} 项，失败 {fail} 项").into(),
                                );
                                // 登记表已变（建群/登记/取消/解散都写文件）→ 刷新列表
                                refresh_vb_rows(&w, &work);
                                // #147 团队列表同步刷新（取消登记/解散群可能清角色 chat_id）
                                refresh_team_rows(&w, &work);
                            }
                        }
                        VirtualBotEvt::Fetched {
                            chat_id,
                            name,
                            desc,
                            error,
                        } => {
                            // 编辑预填：群资料异步拉回 → 回填弹窗（仍在编辑该群时）
                            if let Some(d) = vb_dialog_weak.upgrade() {
                                if d.get_mode() == 3 && d.get_edit_chat_id() == chat_id.as_str() {
                                    let mut st = vb_edit_t.borrow_mut();
                                    if st.chat_id != chat_id {
                                        return; // 弹窗已切换目标，迟到的旧拉取作废
                                    }
                                    let (name_fb, prompt_fb) =
                                        vb_edit_apply_fetched(&mut st, &name, &desc, error.as_deref());
                                    if let Some(err) = error {
                                        // #125 Failed：恢复登记旧名供核对 + 强提示
                                        // （保存被拦截，须用户显式改过群名才放行）
                                        if let Some(n) = name_fb {
                                            d.set_name_input(n.into());
                                        }
                                        d.set_name_count(d.get_name_input().chars().count() as i32);
                                        vb_hint(
                                            &d,
                                            &format!(
                                                "读取群资料失败：{err}。当前为登记旧名，直接保存会把平台群名改回旧名并清空群介绍——请核对修改后再保存"
                                            ),
                                            true,
                                        );
                                    } else {
                                        if let Some(n) = name_fb {
                                            d.set_name_input(n.into());
                                        }
                                        if let Some(p) = prompt_fb {
                                            d.set_prompt_input(p.into());
                                        }
                                        d.set_name_count(d.get_name_input().chars().count() as i32);
                                        d.set_prompt_count(d.get_prompt_input().chars().count() as i32);
                                        vb_hint(&d, "", false);
                                    }
                                }
                            }
                        }
                        VirtualBotEvt::PromptGenerated { text, error } => {
                            // 「✨ 生成」结果回填：成功写 prompt-input + 更新计数；
                            // 失败 hint 展示错误。恢复 busy（生成期间禁用创建/保存）
                            if let Some(d) = vb_dialog_weak.upgrade() {
                                d.set_busy(false);
                                match text {
                                    Some(t) => {
                                        d.set_prompt_input(t.into());
                                        d.set_prompt_count(d.get_prompt_input().chars().count() as i32);
                                        vb_hint(&d, "已生成提示词（可编辑后保存）", false);
                                    }
                                    None => vb_hint(
                                        &d,
                                        &format!("生成失败：{}", error.unwrap_or_default()),
                                        true,
                                    ),
                                }
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
                                drop(b);
                                w.set_status_line("".into());
                                // 名称写进了 work，但编辑框的单元素 model 不自动跟随，须重建
                                refresh_editors(&w, &work);
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
                                    "✅ 微信登录成功！点「保存」写入配置。".into(),
                                );
                                // 占位名被自动改成 wxN：编辑框的单元素 model 不自动跟随，须重建，
                                // 否则名称框残留 botN 诱导用户重名覆盖自动名
                                refresh_editors(&w, &work);
                                show_window_and_focus(&w);
                            }
                        }
                        WxEvt::Failed(e) => {
                            if let Some(d) = qr_weak.upgrade() {
                                // 子窗口关闭不动 dock（8-20 用户反馈）
                                let _ = d.hide();
                            }
                            if let Some(w) = settings_weak.upgrade() {
                                w.set_status_line(format!("微信登录失败：{e}").into());
                            }
                        }
                    }
                }
                // 气泡首次默认位：事件循环活跃后才能拿到主屏尺寸，按**逻辑像素**算（右上角）。
                // 气泡位置（首个 tick，事件循环活跃后）：优先进上次保存的位置，没有则按**逻辑屏宽**算
                // 右上默认位。启动前 set_position 会被 macOS 窗口级联覆盖，所以放到这里统一施加。
                if bubble_pos_apply_t.get() {
                    if let Some(b) = bubble_weak.upgrade() {
                        if b.window().is_visible() {
                            use slint::winit_030::WinitWindowAccessor;
                            let pos = crate::syschat::load_bubble_pos().or_else(|| {
                                b.window()
                                    .with_winit_window(|w| {
                                        w.primary_monitor().map(|m| {
                                            let sz = m.size().to_logical::<f32>(m.scale_factor());
                                            // 默认位锚「可视气泡右上角」：窗口比图标大
                                            // （内缩 BUBBLE_INSET），换算时要扣掉。
                                            crate::syschat::BubblePos {
                                                x: sz.width as i32
                                                    - 16
                                                    - BUBBLE_VISUAL_SIZE
                                                    - BUBBLE_INSET,
                                                y: 120 - BUBBLE_INSET,
                                            }
                                        })
                                    })
                                    .flatten()
                            });
                            if let Some(p) = pos {
                                b.window().set_position(slint::LogicalPosition::new(
                                    p.x as f32,
                                    p.y as f32,
                                ));
                                bubble_pos_apply_t.set(false);
                            }
                        }
                    }
                }
                // #74 历史记录页刷新（仅该页打开时）：会话列表整体替换；选中会话的
                // 消息流随 tick 重查（新消息进入即上屏，标题条数同步更新）。
                if let Some(w) = settings_weak.upgrade() {
                    if w.get_current_page() == 4 {
                        let stats = crate::msgstore::MsgStore::production().chat_stats();
                        let cfg = Config::load().unwrap_or_default();
                        sync_chat_sessions_model(&history_sessions, &stats, &cfg);
                        if let Some(s) = history_selected.borrow().as_ref() {
                            let rows = crate::msgstore::MsgStore::production()
                                .list_chat(&s.bot_key, &s.chat_id, 500);
                            sync_history_msgs(&history_msgs, &w, s, &rows, &cfg);
                        }
                    }
                }
                // #74 未读提醒（与托盘刷新同 tick）：读 unread.json →
                // 有未读 → 托盘红点；提醒开关开、服务在跑且弹窗未在显示 → 弹窗展示
                // 最近几条（弹出即已读：落 msg-read.command 由 service 消费清空，消红点）。
                // 服务停着时不弹：此时 msg-read.command 无人消费，弹窗会每 tick 反复弹出；
                // 红点保留（unread.json 是数据事实），服务重启后首个 tick 消费命令清掉。
                if let Some(items) = crate::unread::UnreadStore::production().snapshot() {
                    let cfg = Config::load().unwrap_or_default();
                    if let Some(t) = tray_weak.upgrade() {
                        t.set_unread_count(if cfg.notify_enabled {
                            items.len() as i32
                        } else {
                            0 // 提醒关：不显红点（历史记录仍照常落库）
                        });
                    }
                    if !items.is_empty()
                        && cfg.notify_enabled
                        && st.running
                        && !notif_showing.get()
                    {
                        if let Some(n) = notif_weak.upgrade() {
                            let rows = notify_rows(&items, &cfg);
                            n.set_items(slint::ModelRc::from(Rc::new(slint::VecModel::from(
                                rows,
                            ))));
                            show_notifications_window(&n);
                            notif_showing.set(true);
                            // 弹出即已读：命令文件由 service 消费（GUI 不直写 unread.json，
                            // 保证 service 是唯一写方）
                            write_command_file("msg-read.command");
                            // 5s 自动收起（toast 惯例）：单次定时器每次弹出 restart
                            let nw = notif_weak.clone();
                            let sw = settings_weak.clone();
                            let showing = notif_showing.clone();
                            notif_timer.start(
                                slint::TimerMode::SingleShot,
                                Duration::from_secs(5),
                                move || hide_notifications_window(&nw, &sw, &showing),
                            );
                        }
                    }
                }
            }},
        );
    }

    slint::run_event_loop()?;
    Ok(())
}

/// 「测试连接」的生效端点：`base_url` 留空时回落到类型预置（openrouter/deepseek），
/// 两者都空 → `None`（调用方转成用户可见提示）。返回前去掉尾部 `/`，避免拼出 `//models`。
fn test_provider_base_url(p: &ProviderConfig) -> Option<&str> {
    let raw = if p.base_url.is_empty() {
        crate::config::provider_kind_default_base_url(&p.kind)
    } else {
        p.base_url.as_str()
    };
    let trimmed = raw.trim_end_matches('/');
    (!trimmed.is_empty()).then_some(trimmed)
}

/// 「测试连接」用的协议族：openrouter/deepseek 是 OpenAI 兼容预置，与 openai-chat
/// 走同一条 `GET {base}/models` + Bearer 路径；未知类型单独返回以便报错。
fn test_provider_family(kind: &str) -> &'static str {
    match kind {
        "anthropic" => "anthropic",
        "openai-chat" | "openai-responses" | "openrouter" | "deepseek" => "openai",
        _ => "unknown",
    }
}

/// 测试供应商连通性（best-effort，10s 超时）。返回用户可读结果，绝不含 api_key。
/// OpenAI 兼容系（含 openrouter/deepseek 预置）：GET {base}/models 带 Bearer；
/// anthropic：POST {base}/v1/messages 最小体。
async fn test_provider(p: &ProviderConfig) -> std::result::Result<String, String> {
    let Some(base) = test_provider_base_url(p) else {
        return Err("Base URL 为空（该类型无预置端点，请先填写）".into());
    };
    if p.api_key.is_empty() {
        return Err("API Key 为空（测试前请先在输入框填密钥）".into());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let label = format!("{}（{}）", p.name, p.kind);
    match test_provider_family(&p.kind) {
        "openai" => {
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
                Err(format!(
                    "{label} 返回 HTTP {code}（能连上但响应异常，检查 Base URL/模型）"
                ))
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
                Ok(format!(
                    "✅ {label} 可达（HTTP {code}：认证通过，检查模型名「{model}」）"
                ))
            } else {
                Err(format!("{label} 返回 HTTP {code}"))
            }
        }
        _ => Err(format!("未知供应商类型：{}", p.kind)),
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

// D2：无头渲染举证台（纯测试模块，`#[path]` 指到单独的 .rs，避免把 ui.rs 再撑长）
#[cfg(test)]
#[path = "ui_headless_render_test.rs"]
mod headless_render;

#[cfg(test)]
mod tests {

    // ── A（卡 `abb-v11-bubble-follow-lag`）：跟随泵的纯逻辑 ──────────────────

    /// 动作判据：只有「可见 + 对话窗矩形变了」才动气泡。
    ///
    /// 判别力（两个输入各管一条，变异后必红）：
    /// - 去掉 `visible &&` ⇒ `..._hidden_never_snaps` 红；
    /// - 去掉 `prev_dialog != Some(cur_dialog)` ⇒ `..._same_rect_is_noop` 红（泵会每 16ms 把气泡
    ///   拖回角，用户拖气泡时会被抢走位置）。
    #[test]
    fn follow_snaps_only_when_dialog_geometry_changed_and_visible() {
        let a = (100, 100, 520, 560);
        let b = (160, 100, 520, 560);
        assert!(follow_should_snap(Some(a), b, true), "对话窗移动了：应跟随");
        assert!(follow_should_snap(None, a, true), "首次（无基线）：应跟随");
        assert!(
            !follow_should_snap(Some(a), a, true),
            "对话窗矩形未变：不动作（否则会与「拖气泡」抢窗口位置）"
        );
        assert!(
            !follow_should_snap(None, a, false),
            "对话窗隐藏：不跟随（气泡自由摆放）"
        );
        assert!(
            !follow_should_snap(Some(a), b, false),
            "对话窗隐藏：即使矩形变了也不跟随"
        );
    }

    /// 工作区缓存判据：中心点仍在缓存工作区内 ⇒ 命中，否则重算（跨屏）。
    ///
    /// 判别力：把判据改成「恒 true」⇒ `..._recomputes_when_window_crosses_screen` 红
    ///（跨屏后一直沿用旧显示器的工作区，气泡被夹到错误边界）。
    #[test]
    fn work_area_cache_hits_inside_and_recomputes_across_screen() {
        let left = (0, 0, 1920, 1080);
        let right = (1920, 0, 1920, 1080);
        // 窗在左屏中心附近：缓存命中。
        assert!(work_area_cache_hit(left, (800, 400, 520, 560)));
        // 窗口刚跨到右屏（中心已在右屏）：缓存失效 ⇒ 重算。
        assert!(!work_area_cache_hit(left, (1700, 400, 520, 560)));
        // 边界：中心点落在缓存右边界上算**不命中**（右屏从该点开始，取新显示器更对）。
        assert!(!work_area_cache_hit(left, (1660, 400, 520, 560)));
        assert!(work_area_cache_hit(right, (1700, 400, 520, 560)));
    }

    /// `FollowState::work_area`：命中不调 `resolve`（省 `current_monitor()`），失效才调一次并刷新缓存。
    #[test]
    fn follow_state_reuses_cached_work_area_until_screen_changes() {
        let left = (0, 0, 1920, 1080);
        let right = (1920, 0, 1920, 1080);
        let mut st = FollowState::default();
        let calls = std::cell::Cell::new(0usize);
        let bump = |v| {
            calls.set(calls.get() + 1);
            Some(v)
        };
        let first = st.work_area((800, 400, 520, 560), || bump(left));
        assert_eq!(first, Some(left));
        assert_eq!(calls.get(), 1, "首次应重算一次");
        let again = st.work_area((820, 400, 520, 560), || bump(left));
        assert_eq!(again, Some(left));
        assert_eq!(calls.get(), 1, "同一显示器内应走缓存，不再查显示器");
        let crossed = st.work_area((1700, 400, 520, 560), || bump(right));
        assert_eq!(crossed, Some(right));
        assert_eq!(calls.get(), 2, "跨显示器应重算一次");
    }

    /// 几何落点：已就位返回 `None`（不调 `set_position`），未就位返回目标窗口位。
    /// 并守住「开窗锚定 → 吸附」的不动点（D4 的 `snap_is_fixed_point_of_open_anchor` 同口径）。
    #[test]
    fn bubble_follow_target_is_none_when_already_snapped() {
        let work = (0, 0, 1920, 1080);
        let dialog = (800, 400, 520, 560);
        // 气泡窗口位 = 可视区外侧再减 4px 透明边；上角对齐窗上缘 ⇒ 可视 y = dialog.y、窗口 y = dialog.y − 4。
        let bubble = (800 - 10 - 36 - 4, 400 - 4, 44, 44);
        assert_eq!(bubble_follow_target(dialog, bubble, work), None, "已就位");
        let moved = (900, 400, 520, 560);
        assert_eq!(
            bubble_follow_target(moved, bubble, work),
            Some((900 - 10 - 36 - 4, 400 - 4)),
            "对话窗右移 100px：气泡也应右移 100px（增量=1:1）"
        );
    }

    /// D7：媒体/文件路径的**唯一基准** —— 图片内联、媒体卡打开、普通「打开」/链接点击
    /// 三处都必须调用 `crate::syschat::resolve_media_path`，不得各写一份（尤其不得回退到
    /// 进程 CWD：`PathBuf::from(src)` / `Path::new(s)`）。
    ///
    /// 变异点：把任一处改回 `std::path::PathBuf::from(src)` ⇒ 计数不足 / 断言红。
    #[test]
    fn media_paths_share_single_workspace_base() {
        let src = crate::platform::src_lf(include_str!("ui.rs"));
        let prod = &src[..src.find("mod tests {").expect("ui.rs 应有测试模块")];
        let calls = prod.matches("crate::syschat::resolve_media_path(").count();
        assert!(
            calls >= 3,
            "图片加载/媒体卡/普通打开 三处应共用 resolve_media_path，实际 {calls} 处"
        );
        assert!(
            !prod.contains("PathBuf::from(src)"),
            "load_block_image 不得再按进程 CWD 解析相对路径"
        );
        assert!(
            !prod.contains("open_path(std::path::Path::new(s))"),
            "媒体打开不得再按进程 CWD 解析相对路径"
        );
    }

    /// v10（稿 v8 第 1 条）「气泡常驻置顶」：显隐只看托盘开关，**不再看对话框是否可见**。
    ///
    /// 判别力（负向）：
    /// 1. 把 `bubble_visible` 改成 `enabled && !dialog_visible`（旧 #13 口径）⇒ 行为断言必红；
    /// 2. 把对话框可见性加回收口签名 / 调用点 ⇒ 结构守卫必红。
    #[test]
    fn bubble_visibility_has_no_dialog_input() {
        // 行为：开关开 ⇒ 显示；关 ⇒ 不显示（没有第三个输入）。
        assert!(bubble_visible(true));
        assert!(!bubble_visible(false));
        // 结构：只扫**生产代码**（`mod tests` 之前）——测试自己的字面量不能当证据。
        let src = crate::platform::src_lf(include_str!("ui.rs"));
        let prod = &src[..src.find("mod tests {").expect("ui.rs 应有测试模块")];
        assert!(
            prod.contains("fn refresh_bubble_visibility(bubble: &AnchorBubble)"),
            "气泡显隐收口应只按 config（签名里不带对话框可见性）"
        );
        assert!(
            !prod.contains("refresh_bubble_visibility(&b, "),
            "气泡显隐调用点还在传第二个参数（旧 #13 口径）"
        );
        assert!(
            !prod.contains("dialog_visible") && !prod.contains("dialog-visibility"),
            "生产代码里还残留「对话框可见性」这条判据"
        );
    }

    /// D5 失焦隐藏的判据（行为侧）：**只有「窗口可见 + 焦点离开整个进程」才隐藏**。
    ///
    /// 判别力（负向，两个输入各管一条）：
    /// - 把判据退化成 `visible`（只看窗口焦点，不看进程）⇒ `..._ignores_focus_that_stays_in_process` 红；
    /// - 退化成 `!app_active`（不看可见性）⇒ `..._skips_hidden_window` 红。
    #[test]
    fn blur_hide_ignores_focus_that_stays_in_process() {
        // 焦点还在本进程（气泡/设置窗/原生文件面板抢走 key）：不隐藏。
        assert!(!should_hide_on_blur(true, true));
        // 焦点离开进程（点了别的 app / Cmd+Tab）：隐藏。
        assert!(should_hide_on_blur(true, false));
    }

    #[test]
    fn blur_hide_skips_hidden_window() {
        // 窗口本来就藏着：不再重复处理（避免「已关窗又收到一次 Focused(false)」时白跑一条隐藏路径）。
        assert!(!should_hide_on_blur(false, false));
        assert!(!should_hide_on_blur(false, true));
    }

    /// D5：「不进 Dock」必须是**结构上**的——`activate_app` 不得再改激活策略，
    /// 且两个窗口都不许把自绘缩放热区写死成 `true`。
    ///
    /// 判别力（负向）：
    /// - 把 `macos_activate(true)` 改回 `setActivationPolicy:Regular(0)` ⇒ 本用例红；
    /// - 把任一个 `set_resize_hit_areas(resize_hit_areas_enabled())` 改回 `set_resize_hit_areas(true)`
    ///   ⇒ 本用例红（D6 真机：开着热区的 macOS 上标题栏顶部/两端变成死区）。
    #[test]
    fn resize_hit_areas_follow_platform_rule() {
        let plat = crate::platform::src_lf(include_str!("platform.rs"));
        let act_start = plat
            .find("pub fn activate_app()")
            .expect("platform.rs 应有 activate_app");
        let activate_branch = &plat[act_start
            ..plat
                .find("pub fn app_is_active()")
                .expect("platform.rs 应有 app_is_active")];
        assert!(
            activate_branch.contains("macos_activate(true)"),
            "activate_app 应只做「激活」（macos_activate(true)）"
        );
        // 激活那条分支里不许出现策略切换：一旦切 Regular，Dock 图标就回来了。
        let body = &activate_branch[..activate_branch
            .find("\n}\n")
            .expect("activate_app 应有函数体")];
        assert!(
            !body.contains("set_policy_fn("),
            "activate_app 不应再改激活策略（D5：ABB 永远是 accessory，不进 Dock）"
        );
        // 自绘缩放热区：两个窗口都按平台口径给。
        let src = crate::platform::src_lf(include_str!("ui.rs"));
        let prod = &src[..src.find("mod tests {").expect("ui.rs 应有测试模块")];
        assert!(
            !prod.contains("set_resize_hit_areas(true)"),
            "有窗口把自绘缩放热区写死了 true：macOS 上 drag_resize_window 返回 NotSupported，\
             热区只吃按压不干活（标题栏顶部/两端变死区）"
        );
        assert_eq!(
            prod.matches("set_resize_hit_areas(resize_hit_areas_enabled())")
                .count(),
            2,
            "设置窗与对话窗都应走 resize_hit_areas_enabled()"
        );
    }

    /// D5：失焦隐藏接在**已有的那个** `on_winit_window_event` 里。
    ///
    /// `Window::on_winit_window_event` 是**单槽 `set`**（见该处注释与 `syschat` C1/C2 的教训）：
    /// 另起一个会把前一条（⌘V / Finder 拖放 / 红点关闭）整个顶掉，而且**没有任何报错**。
    /// 判别力：再加一个 `syschat_win.window().on_winit_window_event(` ⇒ 本用例红。
    #[test]
    fn syschat_has_exactly_one_winit_event_hook() {
        let src = crate::platform::src_lf(include_str!("ui.rs"));
        let prod = &src[..src.find("mod tests {").expect("ui.rs 应有测试模块")];
        assert_eq!(
            prod.matches("syschat_win.window().on_winit_window_event(")
                .count(),
            1,
            "对话窗的 winit 事件钩子是单槽 set：只能有一个（D5 的失焦隐藏必须并进那一个）"
        );
        assert!(
            prod.contains(
                "should_hide_on_blur(w.window().is_visible(), platform::app_is_active())"
            ),
            "失焦隐藏应走判据函数（`should_hide_on_blur`）而不是就地写条件"
        );
    }

    /// D4（卡 `abb-syschat-d4-bubble-follow`）：连续几何只允许「对话窗 ⇒ 气泡」一个方向。
    ///
    /// `anchored_dialog_pos`（气泡 ⇒ 窗口）只允许在**开窗**那一处调用一次；气泡自己的 `Moved`
    /// 绝不能再反推窗口位置 —— 否则「气泡 Moved → 窗动 → 窗 Moved → 气泡动」成环（防环措施
    /// 见 `snap_bubble_to_dialog` 注释）。
    ///
    /// 判别力：把 `anchored_dialog_pos(...)` 再放回气泡的 `Moved` 钩子（旧 #9 写法）⇒ 计数变 3（红）；
    /// 把对话窗钩子里的跟随调用删掉 ⇒ 第二条断言红。
    #[test]
    fn bubble_follow_is_wired_one_way_only() {
        let src = crate::platform::src_lf(include_str!("ui.rs"));
        let prod = &src[..src.find("mod tests {").expect("ui.rs 应有测试模块")];
        // 1 处定义 `fn anchored_dialog_pos(` + 1 处调用 = 2。
        assert_eq!(
            prod.matches("anchored_dialog_pos(").count(),
            2,
            "反方向锚定（气泡 ⇒ 窗口）只允许开窗时调用一次，否则跟随成环"
        );
        assert!(
            prod.contains("anchored_dialog_pos(b.window(), w.window())"),
            "开窗锚定必须保留（气泡 ⇒ 窗口的唯一一次）"
        );
        // 跟随只挂在对话窗的几何事件上（唯一事件源），并交给跟随泵（它内部再按
        // 「句柄就绪走句柄版 / 否则退回 Slint 版」分派，A 卡）。
        assert!(
            prod.contains(
                "handle_syschat_geometry_event(ev, &follow_pump_ev, &follow_bubble, &cw2)"
            ),
            "对话窗 Moved/Resized 必须跟随气泡"
        );
        // A（卡 `abb-v11-bubble-follow-lag`）：macOS 拖动期间 winit 不交事件，必须有一条
        // **common-modes 定时器**驱动的跟随通道；否则「拖窗时气泡不动，松手才跳」。
        assert!(
            prod.contains("crate::platform::common_mode_timer(")
                && prod.contains("p2.tick_handles()"),
            "拖动期间的跟随通道必须存在（common-modes 定时器 → tick_handles）"
        );
        // 气泡钩子不得再移动对话窗（旧写法用的是 `d.window()` 这个绑定名）。
        assert!(
            !prod.contains("anchored_dialog_pos(b.window(), d.window())"),
            "气泡 Moved 钩子不得再反推对话窗（旧 #9 口径，D4 已作废）"
        );
    }

    /// ABB 的观感固定为 slint-pixel 的 **soft（柔彩）预设**（owner 2026-09-30 指定「主题选 soft」）。
    ///
    /// 判别力：把 `ui/app.slint` 里那行 `init => { PixelPresets.soft(); }` 删掉、或改成别的预设
    /// （例如库默认的 `classic()`），本用例即红。注意这**只**锁「我们调用的是 soft」这一事实；
    /// `soft()` 自身的 token 值归 slint-pixel 仓库的守卫管（`every_preset_writes_every_token`）。
    #[test]
    fn app_theme_applies_slint_pixel_soft_preset() {
        let raw = crate::platform::src_lf(include_str!("../ui/app.slint"));
        // 只看**代码行**：按行剥掉 `//` 之后的注释再匹配（评审 R1 的 M-c 反证证明，不剥注释时
        // 「把调用注释掉、字符串还在」照样能骗绿 —— 子串检查的经典洞，同族教训见
        // `LESSON_子串式守卫会被同前缀常量顶住须逐项做阳性对照.md`）。
        let code_line = |needle: &str| {
            raw.lines().any(|l| {
                let code = match l.find("//") {
                    // `://` 是 URL 里的斜杠对，不是注释起点
                    Some(i) if !l[..i].ends_with(':') => &l[..i],
                    _ => l,
                };
                code.contains(needle)
            })
        };
        assert!(
            code_line("PixelPresets"),
            "ui/app.slint 必须从 @slint_pixel 导入 PixelPresets"
        );
        assert!(
            code_line("PixelPresets.soft()"),
            "SettingsWindow 的 init 必须套用 soft 预设（owner 指定；换主题请连同本守卫与 Cargo.toml 注释一起改）"
        );
        assert!(
            !code_line("PixelPresets.classic()"),
            "不得退回库默认的 classic（黑白直角）"
        );
    }

    /// #74 会话列表排序：必须按**真实时间戳**，不能按显示字符串。
    ///
    /// 判别力（owner 2026-10-01 实报「历史记录面板错位」，这是查出的一条真缺陷）：
    /// 旧实现 `sort_by_key(|r| Reverse(r.time.to_string()))` 用的是 `fmt_msg_time` 的
    /// `"MM-DD HH:MM"`（函数把年份丢了）。下面两条数据里 `1_767_200_400`
    /// （2026-01-01 01:00 +08）比 `1_767_193_200`（2025-12-31 23:00 +08）**新**，但显示串
    /// `"12-31 23:00" > "01-01 01:00"` ⇒ 字符串倒序会把旧的排第一，本用例即红。
    #[test]
    fn history_sessions_order_by_real_timestamp_not_display_string() {
        fn st(chat_id: &str, last_ts: Option<i64>) -> crate::msgstore::ChatStats {
            crate::msgstore::ChatStats {
                bot_key: "k".to_string(),
                chat_id: chat_id.to_string(),
                count_7d: 0,
                count_total: 1,
                last_ts,
                last_sender: "u".to_string(),
                last_sender_id: "u".to_string(),
                last_text: "t".to_string(),
                chat_type: "group".to_string(),
                chat_name: String::new(),
            }
        }
        let old_year = st("old", Some(1_767_193_200)); // 2025-12-31 23:00 (+08)
        let new_year = st("new", Some(1_767_200_400)); // 2026-01-01 01:00 (+08)
        let never = st("never", None); // 从未有过消息时间
        let stats = vec![old_year, never, new_year];
        let ids: Vec<&str> = order_sessions_by_recency(&stats)
            .into_iter()
            .map(|s| s.chat_id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["new", "old", "never"],
            "按真时间戳倒序，缺失时间戳排最后"
        );
    }

    /// `round_rect_bands` 的形状不变量：**穷举**小尺寸 × 多个半径，断言
    /// ①没有任何反向矩形（`x2 <= x1` / `y2 <= y1`）②不越界 ③带与带首尾相接且互不重叠。
    ///
    /// 背景（独立审查）：窗口比圆角直径还窄时，`xl` 可能超过 `width - xl`，
    /// `CreateRectRgn` 收到反向矩形会把它规范化成「从右往里」的一条，形状与绘制弧不符。
    /// 设置窗 `min-width: 880px` 让这条今天够不到，但函数是通用的，必须锁住。
    #[test]
    fn round_rect_bands_never_invert_or_overflow() {
        for width in 1..=40 {
            for height in 1..=40 {
                for radius in [1.0_f64, 7.5, 10.0, 12.5, 15.0] {
                    let bands = round_rect_bands(width, height, radius);
                    assert!(
                        !bands.is_empty(),
                        "w={width} h={height} r={radius} 不该为空"
                    );
                    let mut expect_y = 0;
                    for (l, t, r, b) in &bands {
                        assert!(
                            l < r,
                            "反向矩形 l={l} r={r}（w={width} h={height} r={radius}）"
                        );
                        assert!(
                            t < b,
                            "反向矩形 t={t} b={b}（w={width} h={height} r={radius}）"
                        );
                        assert!(*l >= 0 && *r <= width, "越界 x：l={l} r={r} width={width}");
                        assert!(
                            *t >= 0 && *b <= height,
                            "越界 y：t={t} b={b} height={height}"
                        );
                        assert_eq!(
                            *t, expect_y,
                            "带必须逐行相接、不重叠（w={width} h={height}）"
                        );
                        expect_y = *b;
                    }
                    assert_eq!(expect_y, height, "带必须铺满整高（w={width} h={height}）");
                }
            }
        }
    }

    /// 直边区必须是整宽（`xl == 0`）：圆角只该发生在上下各 r 行内。
    #[test]
    fn round_rect_bands_has_full_width_straight_edges() {
        let (w, h, r) = (800, 600, 15.0_f64);
        let bands = round_rect_bands(w, h, r);
        // 行 r 到 h-r-1 是直边区：这些行只能出现在 xl == 0 的带里
        for (l, t, rr, b) in &bands {
            let overlaps_straight = *b > r as i32 && (*t as f64) < (h as f64 - r);
            if overlaps_straight {
                // 直边区的带必须是整宽（否则圆心附近还留着内缩）
                assert!(t < b);
                if (*t as f64) >= r && (*b as f64) <= h as f64 - r {
                    assert_eq!(*l, 0, "直边区带必须整宽：l={l} t={t}");
                    assert_eq!(*rr, w, "直边区带必须整宽：r={rr} t={t}");
                }
            }
        }
        // 顶行与底行必须内缩（否则圆角没生效）
        let top = bands.first().copied().unwrap();
        let bottom = bands.last().copied().unwrap();
        assert!(top.0 > 0, "顶行必须内缩：{top:?}");
        assert!(bottom.0 > 0, "底行必须内缩：{bottom:?}");
        assert_eq!(top.0, bottom.0, "上下圆角必须对称：{top:?} vs {bottom:?}");
    }

    /// 上下对称 + 内缩逐行单调不减（圆角弧的固有性质），并且小数半径（1.25×10=12.5、1.5×10=15）
    /// 不能 panic、不能产生非整数误差导致的抖动。
    #[test]
    fn round_rect_bands_is_symmetric_and_monotone_for_fractional_radius() {
        for radius in [10.0_f64, 12.5, 15.0] {
            let (w, h) = (400, 300);
            let mut insets = vec![0_i32; h as usize];
            for (l, t, _r, b) in round_rect_bands(w, h, radius) {
                for y in t..b {
                    insets[y as usize] = l;
                }
            }
            for y in 0..(radius.ceil() as usize).min(h as usize) {
                let mirror = h as usize - 1 - y;
                assert_eq!(
                    insets[y], insets[mirror],
                    "上下不对称 radius={radius} y={y}"
                );
                if y > 0 {
                    assert!(
                        insets[y] <= insets[y - 1],
                        "圆角内缩必须逐行单调不减 radius={radius} y={y}"
                    );
                }
            }
            // 弧之外的直边必须是 0
            assert_eq!(insets[(h / 2) as usize], 0, "直边必须内缩为 0");
        }
    }

    /// 相邻同宽行必须真的并成一条带（否则每次重算会退化成 O(height) 次 GDI 调用）。
    #[test]
    fn round_rect_bands_merges_adjacent_rows() {
        let (w, h, r) = (800, 600, 15.0_f64);
        let bands = round_rect_bands(w, h, r);
        assert!(
            bands.len() <= (4.0 * r) as usize + 4,
            "带数应约等于 4r，实际 {}（合并失效？）",
            bands.len()
        );
        // 同一带内不能有相邻可合并的断点：任意相邻带的宽度必须不同
        for pair in bands.windows(2) {
            let (l1, _, r1, b1) = pair[0];
            let (l2, _, r2, _) = pair[1];
            assert!(b1 == pair[1].1, "带必须逐行相接");
            assert!(l1 != l2 || r1 != r2, "相邻同宽带应已合并：{pair:?}");
        }
    }

    /// **渲染侧 oracle**（独立审查要求）：Slint 默认 femtovg 用的是三次贝塞尔
    /// `Path::rounded_rect` + 展平，**不是**理想圆——所以不能用「与理想圆重合」当验收。
    /// 这里直接用同一版 femtovg 当 oracle，逐行拿到「绘制路径首个被覆盖的像素」，
    /// 断言我们的圆角区域满足**安全方向**：区域绝不比实际绘制路径更窄（更窄就会把
    /// 描边切掉，正是用户报的那个 bug），同时外扩不超过 1px（避免出现明显黑边）。
    ///
    /// 已知残留（如实记录，不粉饰）：理想圆公式与贝塞尔展平在个别行差 1px（例如
    /// scale=2.0/r=20 有 4 行、scale=1.0/r=10 有 2 行），方向是**区域更宽**，因此不会切
    /// 描边；要彻底逐像素重合需要直接拿展平后的路径生成区域，属后续优化。
    #[cfg(test)]
    fn femto_oracle_first_pixels(r: f32, dpi: f32, w: i32, h: i32) -> Vec<i32> {
        use femtovg::{renderer::Void, Canvas, FillRule, Path};
        let mut canvas = Canvas::new(Void).expect("femtovg canvas");
        canvas.set_size(w as u32, h as u32, dpi);
        let mut path = Path::new();
        path.rounded_rect(0.0, 0.0, w as f32, h as f32, r);
        (0..r.ceil() as i32)
            .map(|y| {
                (0..(2.0 * r.ceil()) as i32 + 2)
                    .find(|&x| {
                        canvas.contains_point(
                            &path,
                            x as f32 + 0.5,
                            y as f32 + 0.5,
                            FillRule::NonZero,
                        )
                    })
                    .unwrap_or(-1)
            })
            .collect()
    }

    /// 区域的安全性：**永不比绘制路径窄**（否则切描边），且外扩 ≤1px。
    #[test]
    fn round_rect_bands_never_narrower_than_femtovg_path() {
        let (w, h) = (1671, 1067);
        for scale in [1.0_f64, 1.25, 1.5, 1.75, 2.0] {
            let r = 10.0 * scale; // .slint 侧重逻辑 10px
            let dpi = scale.ceil() as f32;
            let oracle = femto_oracle_first_pixels(r as f32, dpi, w, h);
            let mut insets = vec![0_i32; h as usize];
            for (l, t, _r, b) in round_rect_bands(w, h, r) {
                for y in t..b {
                    insets[y as usize] = l;
                }
            }
            for (y, want) in oracle.iter().enumerate() {
                if *want < 0 {
                    continue;
                }
                let got = insets[y];
                assert!(
                    got <= *want,
                    "scale={scale} y={y}: 区域 xl={got} 比绘制路径首像素 {want} 更窄 → 会切描边"
                );
                assert!(
                    *want - got <= 1,
                    "scale={scale} y={y}: 区域外扩 {}px（>1px 会露出明显黑边）",
                    *want - got
                );
            }
        }
    }

    /// 极小窗口（宽高 < 圆角直径）必须退化成良定义的整窗矩形，不得出现反向矩形。
    #[test]
    fn round_rect_bands_degenerates_for_tiny_window() {
        assert_eq!(round_rect_bands(1, 1, 10.0), vec![(0, 0, 1, 1)]);
        assert_eq!(round_rect_bands(4, 3, 10.0), vec![(0, 0, 4, 3)]);
        // 宽度够、高度极小：也必须是良定义矩形
        let bands = round_rect_bands(400, 2, 15.0);
        assert!(bands
            .iter()
            .all(|(l, t, r, b)| l < r && t < b && *r <= 400 && *b <= 2));
    }

    /// #300：预置类型（openrouter/deepseek）留空 base_url 时，「测试连接」也要用预置端点；
    /// 显式填的覆盖预置并去掉尾部斜杠；无预置且留空 → None。
    #[test]
    fn provider_test_base_url_uses_preset_when_blank() {
        let mut p = crate::config::ProviderConfig {
            kind: "openrouter".into(),
            ..Default::default()
        };
        assert_eq!(
            test_provider_base_url(&p),
            Some("https://openrouter.ai/api/v1")
        );
        p.kind = "deepseek".into();
        assert_eq!(
            test_provider_base_url(&p),
            Some("https://api.deepseek.com/v1")
        );
        // 显式填写覆盖预置，且去尾斜杠
        p.base_url = "https://proxy.example.com/v1/".into();
        assert_eq!(
            test_provider_base_url(&p),
            Some("https://proxy.example.com/v1")
        );
        // 无预置 + 留空 → 调用方报「Base URL 为空」
        p.kind = "openai-chat".into();
        p.base_url = String::new();
        assert_eq!(test_provider_base_url(&p), None);
    }

    /// #300：预置类型必须走 OpenAI 兼容的测试路径（否则「测试连接」报未知类型）。
    #[test]
    fn provider_test_family_covers_presets() {
        for k in ["openai-chat", "openai-responses", "openrouter", "deepseek"] {
            assert_eq!(
                test_provider_family(k),
                "openai",
                "{k} 应走 OpenAI 兼容路径"
            );
        }
        assert_eq!(test_provider_family("anthropic"), "anthropic");
        assert_eq!(test_provider_family("gemini"), "unknown");
    }
    use super::*;

    /// bot 字段翻转必须翻签名（bot 事件循环在 service 启动时构建，入签名字段不是热读）。
    ///
    /// 原用例只钉 `wassette` 一个字段；wassette 移除后改为**逐字段**覆盖 `bots_struct_sig`
    /// 的全部 9 个 bot 字段——否则以后增删字段时，“字段变了但签名不翻 → service 不重启”
    /// 这类回归不会被任何测试发现（2026-10-10 独立评审指出）。
    #[test]
    fn bots_struct_sig_covers_all_bot_fields() {
        let base = || {
            let mut c = Config::default();
            c.bots = vec![crate::config::BotConfig {
                name: "bot1".into(),
                kind: "feishu".into(),
                ..Default::default()
            }];
            c
        };
        let s0 = bots_struct_sig(&base());
        /// 一条字段翻转用例：字段名（失败信息用）+ 就地改写 bot 的闭包。
        type FieldCase = (&'static str, fn(&mut crate::config::BotConfig));
        let cases: [FieldCase; 9] = [
            ("name/key", |b| b.name = "bot2".into()),
            ("enabled", |b| b.enabled = !b.enabled),
            ("kind", |b| b.kind = "dingtalk".into()),
            ("app_id", |b| b.app_id = "cli_x".into()),
            ("app_secret", |b| b.app_secret = "sec".into()),
            ("wx_token", |b| b.wx_token = "tok".into()),
            ("wx_user_id", |b| b.wx_user_id = "wxu".into()),
            ("ding_user_id", |b| b.ding_user_id = "dingu".into()),
            ("ding_robot_code", |b| b.ding_robot_code = "rc".into()),
        ];
        for (field, mutate) in cases {
            let mut c = base();
            mutate(&mut c.bots[0]);
            assert_ne!(
                s0,
                bots_struct_sig(&c),
                "改 {field} 必须翻签名（触发 svc_restart）"
            );
        }
    }

    #[test]
    fn bots_struct_sig_covers_providers() {
        // harness 供应商 env 是启动快照 → 改供应商必须翻签名触发 svc_restart
        //（Windows 实机：补填 API Key 后不重启，codex 持续 Missing env 报错）
        let mut c = Config::default();
        c.providers = vec![crate::config::ProviderConfig {
            name: "ds".into(),
            kind: "openai-chat".into(),
            base_url: "https://api.deepseek.com".into(),
            api_key: "sk-a".into(),
            model: "deepseek-chat".into(),
        }];
        let s1 = bots_struct_sig(&c);
        // ① 改 key → 翻签名
        c.providers[0].api_key = "sk-b".into();
        let s2 = bots_struct_sig(&c);
        assert_ne!(s1, s2, "改 API Key 必须翻签名（触发 service 重启）");
        // ①b 改默认供应商指向 → 也翻签名（harness env 快照不热读，路由变更须重启）
        let mut r = c.clone();
        r.default_provider = "other-vendor".into();
        assert_ne!(
            bots_struct_sig(&c),
            bots_struct_sig(&r),
            "改默认供应商指向必须翻签名"
        );
        // ①c bot 级 provider 指向变更 → 翻签名
        let mut r2 = c.clone();
        r2.bots = vec![crate::config::BotConfig {
            name: "bot1".into(),
            provider: "ds".into(),
            ..Default::default()
        }];
        let mut r3 = r2.clone();
        r3.bots[0].provider = "other".into();
        assert_ne!(
            bots_struct_sig(&r2),
            bots_struct_sig(&r3),
            "bot 改供应商指向必须翻签名"
        );
        // ② 明文 key 绝不出现在签名串里
        assert!(
            !s1.contains("sk-a") && !s2.contains("sk-b"),
            "签名须用 key 摘要"
        );
        // ③ 供应商声明顺序无关（签名排序）
        let mut d = c.clone();
        d.providers.insert(
            0,
            crate::config::ProviderConfig {
                name: "anth".into(),
                kind: "anthropic".into(),
                ..Default::default()
            },
        );
        let mut e = c.clone();
        e.providers.push(d.providers[0].clone());
        assert_eq!(bots_struct_sig(&d), bots_struct_sig(&e), "顺序无关");
    }

    #[test]
    fn team_plan_rows_flattens_roles() {
        // #141：TeamPlan → 预览行（role_name/member/duty），member 空 = 待任命占位
        let plan = crate::teambuilder::TeamPlan {
            team_name: "记账团队".into(),
            roles: vec![
                crate::teambuilder::TeamRole {
                    role_name: "产品经理".into(),
                    member_name: Some("小王".into()),
                    system_prompt: "负责需求".into(),
                },
                crate::teambuilder::TeamRole {
                    role_name: "后端".into(),
                    member_name: None,
                    system_prompt: "负责 API".into(),
                },
            ],
            collab: None,
        };
        let rows = team_plan_rows(&plan);
        assert_eq!(rows.len(), 2);
        // #162：预览行 role_name = 完整三段（项目-角色-姓名）；member 列置空避免重复
        assert_eq!(
            rows[0],
            (
                "记账团队-产品经理-小王".into(),
                String::new(),
                "负责需求".into()
            )
        );
        assert_eq!(
            rows[1],
            (
                "记账团队-后端-待任命".into(),
                String::new(),
                "负责 API".into()
            )
        );
    }

    #[test]
    fn team_create_line_formats_member_and_pending() {
        // #141/#162：创建结果行「角色 → 详情」——role_name 已是三段（含成员），
        // 不再拼（成员）括号
        assert_eq!(
            team_create_line("记账团队-产品经理-小王", "已建"),
            "记账团队-产品经理-小王 → 已建"
        );
        assert_eq!(
            team_create_line("记账团队-测试-待任命", "失败：群名冲突"),
            "记账团队-测试-待任命 → 失败：群名冲突"
        );
    }

    #[test]
    fn single_model_holds_exactly_one_row() {
        // 单元素 model 是「交互断绑后重建实例」绕法的承重原语：行数恒为 1、
        // 值原样进出（SetWindow 层的刷新链路依赖 Slint 窗口，只能人工/E2E 验证）
        let m = single_model(OptionRow {
            name: "".into(),
            checked: true,
        });
        assert_eq!(m.row_count(), 1);
        assert!(m.row_data(0).unwrap().checked);
        let e = single_model(EditorRow {
            value: "cli_xxx".into(),
        });
        assert_eq!(e.row_count(), 1);
        assert_eq!(e.row_data(0).unwrap().value.as_str(), "cli_xxx");
    }

    /// #74 红点合成行为：右上角出现红点（含白描边），左下角像素保持原图不动。
    /// 用真实托盘资产（CARGO_MANIFEST_DIR 相对路径，测试进程 cwd 无关）。
    fn load_green() -> slint::Image {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/tray-green.png");
        slint::Image::load_from_path(&p).expect("托盘资产应可加载")
    }

    #[test]
    fn tray_dot_is_painted_at_top_right_and_preserves_rest() {
        let base = load_green();
        let original = base.to_rgba8().expect("rgba8 解码");
        let (w, h) = (original.width(), original.height());
        let dot = composite_tray_dot(&base)
            .to_rgba8()
            .expect("合成图应可解码");
        assert_eq!((dot.width(), dot.height()), (w, h), "合成不改尺寸");

        // SharedPixelBuffer 无 pixel() 访问器，用 as_slice 按行索引取像素
        let at = |buf: &slint::SharedPixelBuffer<slint::Rgba8Pixel>, x: u32, y: u32| {
            buf.as_slice()[(y * w + x) as usize]
        };
        // 红点中心落在右上角内缩区域（与 composite_tray_dot 的半径/边距公式同源）
        let r = ((w.min(h) / 6).max(2)) as i32;
        let margin = (r / 2).max(1);
        let (cx, cy) = (w as i32 - margin - r, margin + r);
        let p = at(&dot, cx as u32, cy as u32);
        assert!(
            p.r > 0xE0 && p.g < 0x80 && p.b < 0x60,
            "红点中心应为红色系，实际 {p:?}"
        );
        // 白描边：红点最外圈（dist² == r² 的轴上像素）应为白色
        let ring = at(&dot, (cx + r) as u32, cy as u32);
        assert!(
            ring.r > 0xE0 && ring.g > 0xE0 && ring.b > 0xE0,
            "红点外圈应为白描边，实际 {ring:?}"
        );
        // 左下角远离红点：像素必须保持原图（合成不能污染其余区域）
        let bl = at(&dot, 1, h - 2);
        let bl_orig = at(&original, 1, h - 2);
        assert_eq!(
            (bl.r, bl.g, bl.b, bl.a),
            (bl_orig.r, bl_orig.g, bl_orig.b, bl_orig.a)
        );
    }

    // ── #125 编辑弹窗异步预填状态机 ──
    fn st() -> VbEditState {
        VbEditState {
            phase: VbFetchPhase::Pending,
            chat_id: "oc_chat_a".into(),
            fallback_name: "软件工程师-Steven".into(),
            name_dirty: false,
            prompt_dirty: false,
        }
    }

    #[test]
    fn vb_edit_fetch_ok_backfills_and_phase_ok() {
        let mut s = st();
        let (n, p) = vb_edit_apply_fetched(&mut s, "软件工程师-StevenV2", "新的群介绍", None);
        assert_eq!(s.phase, VbFetchPhase::Ok);
        // 平台新名回填（解决「打开残留登记旧名」）
        assert_eq!(n.as_deref(), Some("软件工程师-StevenV2"));
        assert_eq!(p.as_deref(), Some("新的群介绍"));
        // Ok 后保存放行
        assert_eq!(vb_edit_save_blocked(&s, "oc_chat_a"), None);
    }

    #[test]
    fn vb_edit_fetch_ok_does_not_override_user_edits() {
        // 用户已手动改过群名 → 回填不得覆盖（dirty 保护，防竞态）
        let mut s = st();
        s.name_dirty = true;
        let (n, p) = vb_edit_apply_fetched(&mut s, "软件工程师-StevenV2", "新的群介绍", None);
        assert_eq!(n, None, "群名 dirty：不回填");
        assert_eq!(p.as_deref(), Some("新的群介绍"), "提示词未改：仍回填");

        // 提示词 dirty 同理
        let mut s2 = st();
        s2.prompt_dirty = true;
        let (n2, p2) = vb_edit_apply_fetched(&mut s2, "软件工程师-StevenV2", "新的群介绍", None);
        assert_eq!(n2.as_deref(), Some("软件工程师-StevenV2"));
        assert_eq!(p2, None, "提示词 dirty：不回填");
    }

    #[test]
    fn vb_edit_fetch_failed_blocks_save_until_user_edits_name() {
        // 拉取失败：恢复登记旧名显示，且保存被拦截（防把旧名写回平台）
        let mut s = st();
        let (n, p) = vb_edit_apply_fetched(&mut s, "", "", Some("99992356"));
        assert_eq!(s.phase, VbFetchPhase::Failed);
        assert_eq!(n.as_deref(), Some("软件工程师-Steven"), "失败恢复登记旧名");
        assert_eq!(p, None);
        assert!(
            vb_edit_save_blocked(&s, "oc_chat_a").is_some(),
            "Failed 且未改群名：禁止保存"
        );

        // 用户显式改过群名（dirty）→ 视为有意为之，放行
        s.name_dirty = true;
        assert_eq!(vb_edit_save_blocked(&s, "oc_chat_a"), None);

        // 只改提示词不算显式改群名：仍拦截（防把登记旧名 PUT 回平台）
        let mut s3 = st();
        let _ = vb_edit_apply_fetched(&mut s3, "", "", Some("99992356"));
        s3.prompt_dirty = true;
        assert!(
            vb_edit_save_blocked(&s3, "oc_chat_a").is_some(),
            "Failed 且只改提示词：仍禁止保存（须显式改群名）"
        );

        // 显式改过群名 → 放行
        s3.name_dirty = true;
        assert_eq!(vb_edit_save_blocked(&s3, "oc_chat_a"), None);
    }

    #[test]
    fn vb_edit_pending_blocks_save_and_stale_fetch_discarded() {
        // Pending：保存被拦截（还没拉到平台资料）
        let s = st();
        assert_eq!(
            vb_edit_save_blocked(&s, "oc_chat_a").unwrap(),
            "正在拉取群资料，请稍候再保存…"
        );
        // 弹窗已切换目标：chat_id 不匹配 → 拦截 + 迟到拉取作废
        assert!(vb_edit_save_blocked(&s, "oc_chat_b").is_some());
        let mut s2 = st();
        let (n, _) = vb_edit_apply_fetched(&mut s2, "别的群", "", None);
        assert_eq!(s2.chat_id, "oc_chat_a");
        assert_eq!(n.as_deref(), Some("别的群"));
    }

    #[test]
    fn vb_edit_empty_platform_name_not_backfilled() {
        // 平台名空串：不回填（保留空 + Pending 语义由保存拦截兜底）
        let mut s = st();
        let (n, p) = vb_edit_apply_fetched(&mut s, "", "desc", None);
        assert_eq!(n, None);
        assert_eq!(p.as_deref(), Some("desc"));
    }
}
