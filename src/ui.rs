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
/// member 为空 = 待任命（UI 显示占位）。
fn team_plan_rows(plan: &crate::teambuilder::TeamPlan) -> Vec<(String, String, String)> {
    plan.roles
        .iter()
        .map(|r| {
            (
                r.role_name.clone(),
                r.member_name.clone().unwrap_or_default(),
                r.system_prompt.clone(),
            )
        })
        .collect()
}

/// #141 单角色创建结果行文案（成功/失败清单展示）：`角色（成员）→ 详情`。
fn team_create_line(role_name: &str, member: &str, detail: &str) -> String {
    format!(
        "{}（{}）→ {}",
        role_name,
        if member.is_empty() {
            "待任命"
        } else {
            member
        },
        detail
    )
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
fn resolve_display(cfg: &Config, bot_key: &str, sender_id: &str) -> (String, String) {
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

/// #74 历史页列表行（消息库行 → 生成类型 HistoryRow）。
fn history_to_row(r: &crate::msgstore::MsgRow, cfg: &Config) -> HistoryRow {
    let bot = cfg.bots.iter().find(|b| b.key() == r.bot_key);
    let kind = bot.map(|b| kind_label(&b.kind)).unwrap_or_default();
    let (bot_name, sender) = resolve_display(cfg, &r.bot_key, &r.sender_id);
    // 审查跟进：assistant 行（bot 回复）发送者列显示 bot 名——回复来自 bot 而非
    // 授权者，否则「授权者名 + 回复」标签语义错位。
    // user 行优先落库时的 sender_name（未授权用户 API 反查的真实名字；8-20 用户
    // 反馈：历史记录显示名字而不是 open_id），回落本地名单/id。
    let sender = if r.direction == "assistant" {
        bot_name.clone()
    } else if !r.sender_name.is_empty() {
        r.sender_name.clone()
    } else {
        sender
    };
    HistoryRow {
        id: r.id as i32,
        bot: bot_name.into(),
        kind: kind.into(),
        sender: sender.into(),
        time: fmt_msg_time(r.ts).into(),
        direction: (if r.direction == "assistant" {
            "回复"
        } else {
            "用户"
        })
        .into(),
        preview: crate::agent::truncate(&r.text, 60).into(),
        text: r.text.clone().into(),
    }
}

/// #74 历史列表 model 整体替换（2s 轮询刷新，ui.rs sync_model 同款手法）。
fn sync_history_model(
    model: &slint::VecModel<HistoryRow>,
    rows: &[crate::msgstore::MsgRow],
    cfg: &Config,
) {
    model.set_vec(
        rows.iter()
            .map(|r| history_to_row(r, cfg))
            .collect::<Vec<_>>(),
    );
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
/// 的角落锚定语义（此处是唯一例外，注释留痕）。顺序同 show_window_and_focus：先激活
/// （dock 图标）再 show——避免「先 show 后激活」的策略重排闪烁；show 后 set_position
/// 发生在 macOS 窗口 map 之前（同步路径），无可见跳动。
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
/// 设置窗还开着（点条目跳历史页）时不动 dock——激活策略已归设置窗；
/// 否则弹窗收起即降回 accessory（hide 先于 hide_dock，同 QrDialog 显隐时序，避免闪烁）。
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
    let sel = w.get_selected();
    *work.borrow_mut() = cfg.bots.clone();
    sync_model(model, &cfg.bots);
    w.set_selected(sel);
    refresh_owner_code_info(w, work);
}

/// 按当前选中 bot 重建三个独立开关（启用/授权者隔离/每日整理）的单元素 model。
/// 与 backend-options 同款绕法：整体替换 model → for 循环重建 PixelCheckBox 实例，
/// 实例全新、checked 绑定全新——绕开 slint「用户交互移除 checked 绑定（内部赋值断绑，
/// 绑 model 行属性或独立 property 都一样）、状态残留到其它 bot」的坑（#80 回归：
/// 点 A 的每日整理，切到 B 显示残留，诱导误操作后连配置一起串）。
fn refresh_toggle_checks(w: &SettingsWindow, work: &RefCell<Vec<BotConfig>>) {
    let bot = work.borrow().get(w.get_selected() as usize).cloned();
    let mk = |v: bool| -> slint::ModelRc<OptionRow> {
        slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![OptionRow {
            name: "".into(),
            checked: v,
        }])))
    };
    w.set_enabled_options(mk(bot.as_ref().map(|b| b.enabled).unwrap_or(false)));
    w.set_restrict_options(mk(bot
        .as_ref()
        .map(|b| b.restrict_granted_agent)
        .unwrap_or(false)));
    w.set_tidy_options(mk(bot.as_ref().map(|b| b.tidy_enabled).unwrap_or(false)));
    // #91 群聊提及默认（bot 级）：true=免 @ 参与
    w.set_mention_options(mk(bot.as_ref().map(|b| b.mention_default).unwrap_or(false)));
}

/// 按当前选中 bot 重建编辑框/下拉的单元素 model（名称/供应商/Owner/App ID/App Secret/
/// 钉钉机器人编码/钉钉 Owner）。输入/选择交互同样移除组件 text/value 绑定（与 checkbox
/// 同源坑），直接绑 model 行属性会在切 bot 后残留上一 bot 的值；整体替换 model →
/// for 循环重建实例，绑定全新。密码框按现有语义回填原值（bot_to_row 已如此）。
fn refresh_editors(w: &SettingsWindow, work: &RefCell<Vec<BotConfig>>) {
    let bot = work.borrow().get(w.get_selected() as usize).cloned();
    let mk = |v: &str| -> slint::ModelRc<EditorRow> {
        slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![EditorRow {
            value: v.into(),
        }])))
    };
    let b = bot.as_ref();
    w.set_name_editor_options(mk(b.map(|b| b.name.as_str()).unwrap_or("")));
    w.set_provider_editor_options(mk(b.map(|b| b.provider.as_str()).unwrap_or("")));
    w.set_owner_editor_options(mk(b.map(|b| b.owner_open_id.as_str()).unwrap_or("")));
    w.set_app_id_editor_options(mk(b.map(|b| b.app_id.as_str()).unwrap_or("")));
    w.set_app_secret_editor_options(mk(b.map(|b| b.app_secret.as_str()).unwrap_or("")));
    w.set_ding_robot_code_editor_options(mk(b.map(|b| b.ding_robot_code.as_str()).unwrap_or("")));
    w.set_ding_owner_ids_editor_options(mk(b.map(|b| b.ding_owner_ids.as_str()).unwrap_or("")));
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
    w.set_backend_options(slint::ModelRc::from(Rc::new(slint::VecModel::from(
        mk_opts(
            &[("claude", "claude"), ("codex", "codex"), ("pi", "pi")],
            be_sel,
        ),
    ))));
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

/// 跑一次依赖检测并把全部 7 项状态回填到设置窗
/// （claude/codex/pi/node/python3/lark-cli/dingtalk-cli）。
fn push_deps_to_window(w: &SettingsWindow) {
    let all = crate::deps::detect_all();
    let get = |id: &str| {
        all.iter()
            .find(|d| d.id == id)
            .map(|d| d.found)
            .unwrap_or(false)
    };
    // #93 codex 三态：codex_ok = 已装且版本满足最低锁定（< MIN_CODEX_VERSION → 需升级）。
    let codex = all.iter().find(|d| d.id == "codex");
    let codex_version = codex.map(|d| d.version.clone()).unwrap_or_default();
    let codex_ok = codex.map(|d| d.found && d.version_ok).unwrap_or(false);
    // #105 git 三态：git_ok = 已装且版本 >= MIN_GIT_VERSION（< 2.30 → 需升级）。
    let git = all.iter().find(|d| d.id == "git");
    let git_version = git.map(|d| d.version.clone()).unwrap_or_default();
    let git_ok = git.map(|d| d.found && d.version_ok).unwrap_or(false);
    // #8 M0：claude/codex/pi 任一未装 → 顶部横幅（首次启动也据此自动弹设置窗引导安装）
    // #93：codex 版本过低同样视为「待处理」——启动引导/横幅继续提示，直到升级到最低锁定版本。
    w.set_missing_agent(!get("claude") || !codex_ok || !get("pi"));
    w.set_claude_installed(get("claude"));
    w.set_codex_installed(get("codex"));
    w.set_codex_version(codex_version.into());
    w.set_codex_ok(codex_ok);
    w.set_pi_installed(get("pi"));
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

pub fn run_gui() -> Result<()> {
    configure_backend()?;

    // 旧「单 bot 平铺」数据 → workspaces/<key>/（幂等）
    if let Ok(c) = Config::load() {
        if let Some(b) = c.bots.first() {
            platform::migrate_legacy_state(&b.key());
        }
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
    });
    // 历史记录列表 model：2s 轮询只读查询消息库 → set_vec 整体替换（ui.rs sync_history_model）
    let history_model: Rc<slint::VecModel<HistoryRow>> = Rc::new(slint::VecModel::default());
    settings.set_history_rows(slint::ModelRc::from(history_model.clone()));
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
        sync_providers_model(providers_model, &c.providers, &c.default_provider);
        w.set_provider_names(slint::ModelRc::from(Rc::new(slint::VecModel::from(
            build_provider_names(&c.providers),
        ))));
        // 后端、Owner、供应商 都是 per-bot（bots[i].backend / .owner_open_id / .provider）
        w.set_selected(if c.bots.is_empty() { -1 } else { 0 });
        w.set_provider_selected(if c.providers.is_empty() { -1 } else { 0 });
        refresh_owner_code_info(w, work);
        refresh_exclusive_checks(w, work);
        refresh_toggle_checks(w, work);
        refresh_editors(w, work);
        w.set_status_line("".into());
        // 依赖检测：claude/codex/node/python3/lark-cli 是否在本机可执行路径上。
        push_deps_to_window(w);
        // 系统权限检测（macOS）：完全磁盘/辅助功能/屏幕录制/自动化。
        push_perms_to_window(w);
        // 虚拟 Bot 登记列表（#75）：按选中 bot 刷新
        refresh_vb_rows(w, work);
        // #147 团队列表：按选中 bot 刷新（热读 teams.json）
        refresh_team_rows(w, work);
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
                                        crate::log!(
                                            "[update] 发现新版本 v{}（当前 v{}）",
                                            rel.version,
                                            crate::updater::CURRENT
                                        );
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
                                    crate::log!(
                                        "[update] 检查失败{}：{e:#}",
                                        if silent { "（静默）" } else { "" }
                                    );
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
                                    crate::log!(
                                        "[update] v{} 安装完成，退出并重启到新版本",
                                        rel.version
                                    );
                                    let _ = slint::invoke_from_event_loop(move || {
                                        install::svc_stop();
                                        let _ = slint::quit_event_loop();
                                    });
                                }
                                Err(e) => {
                                    crate::log!("[update] 安装失败：{e:#}");
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
                            // 走该 bot 生效后端（bot.backend 优先，回落全局默认）
                            let backend = Config::load()
                                .ok()
                                .and_then(|c| {
                                    c.bots
                                        .iter()
                                        .find(|b| b.key() == bot_key)
                                        .map(|b| b.effective_backend(&c.default_backend).to_string())
                                })
                                .unwrap_or_default();
                            let r =
                                crate::agent::generate_role_prompt(crate::agent::Backend::parse(&backend), &name).await;
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
                    // #141 团队生成（真实 LLM 链路）：取该 bot 生效后端，生成方案后经 team_rx 回主线程。
                    UiCmd::TeamGenerate {
                        idx,
                        bot_key,
                        target,
                    } => {
                        let team_tx = team_tx.clone();
                        tokio::spawn(async move {
                            // 走该 bot 生效后端（与 GeneratePrompt 同口径）
                            let backend = Config::load()
                                .ok()
                                .and_then(|c| {
                                    c.bots.iter().find(|b| b.key() == bot_key).map(|b| {
                                        b.effective_backend(&c.default_backend).to_string()
                                    })
                                })
                                .unwrap_or_default();
                            let r = crate::teambuilder::generate_team_plan(
                                crate::agent::Backend::parse(&backend),
                                &target,
                                &[],
                                None,
                            )
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
    // 启动 20s 后静默检查一次更新，之后每 6h 复查（失败静默；有新版本时托盘菜单出现「升级」项）
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
            Duration::from_secs(6 * 3600),
            move || {
                let _ = tx3.send(UiCmd::CheckUpdate { silent: true });
            },
        );
        std::mem::forget(t6);
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
        // #93：codex 版本过低也触发启动引导（升级到最低锁定版本前一直引导）
        let codex_low = deps
            .iter()
            .find(|d| d.id == "codex")
            .map(|d| d.found && !d.version_ok)
            .unwrap_or(false);
        if missing("claude")
            || missing("codex")
            || codex_low
            || missing("pi")
            || std::env::args().any(|a| a == "--show-settings")
        {
            let debug_show = std::env::args().any(|a| a == "--show-settings");
            let work = work.clone();
            let model = bots_model.clone();
            let pmodel = providers_model.clone();
            load_with_draft(&settings, &dirty, &work, &model, &pmodel, &wk);
            push_settings_status(&settings, &install::status());
            // 调试参数（--show-settings）不设误导的「未检测到」状态行
            if !debug_show {
                settings.set_status_line(
                    "⚠️ 未检测到 Claude Code / Codex CLI：请到「环境配置」页安装依赖，否则机器人无法处理消息。"
                        .into(),
                );
                settings.set_status_is_error(true);
            }
            show_window_and_focus(&settings);
        }
    }
    {
        let sw = settings.as_weak();
        let work = work.clone();
        let model = bots_model.clone();
        let pmodel = providers_model.clone();
        let dirty_open = dirty.clone();
        // 供托盘「设置…」与 Dock 点击共用的显示逻辑：草稿恢复 + 显示置前。
        // Dock 图标点击恢复窗口（no-frame 后系统标题栏窗口的默认 reopen 行为失效，
        // 2026-08-18 回归修复）走同款路径。
        let tray_sw = sw.clone();
        #[cfg(target_os = "macos")]
        {
            let dock_sw = settings.as_weak();
            let dock_work = work.clone();
            let dock_model = bots_model.clone();
            let dock_pmodel = providers_model.clone();
            let dock_dirty = dirty.clone();
            crate::platform::install_dock_reopen(Box::new({
                let wk = wk.clone();
                move || {
                    if let Some(w) = dock_sw.upgrade() {
                        load_with_draft(
                            &w,
                            &dock_dirty,
                            &dock_work,
                            &dock_model,
                            &dock_pmodel,
                            &wk,
                        );
                        push_settings_status(&w, &install::status());
                        show_window_and_focus(&w);
                    }
                }
            }));
        }
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
                        "backend" => {
                            bot.backend = value.to_string();
                            // 后端三选一 CheckBox：select-backend 已显式 set 勾选态，这里只回写
                            // work；切 bot 时由 refresh_exclusive_checks 经 backend-ui property
                            // 重新 set 勾选态（绕开用户交互会移除 checked 绑定的 slint 坑）。
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
                // 输入框/下拉断绑后不跟随 model 重建，这里一并重建（kind/backend 切换）
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
            // 重建单元素 model → for 重建实例（交互断绑后实例不跟随，必须重建）
            if let Some(w) = sw.upgrade() {
                w.set_enabled_options(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
                    OptionRow {
                        name: "".into(),
                        checked: enabled,
                    },
                ]))));
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
                w.set_restrict_options(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
                    OptionRow {
                        name: "".into(),
                        checked: enabled,
                    },
                ]))));
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
                w.set_tidy_options(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
                    OptionRow {
                        name: "".into(),
                        checked: enabled,
                    },
                ]))));
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
                w.set_mention_options(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
                    OptionRow {
                        name: "".into(),
                        checked: enabled,
                    },
                ]))));
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
                refresh_owner_code_info(&w, &work);
                refresh_exclusive_checks(&w, &work);
                refresh_toggle_checks(&w, &work);
                refresh_editors(&w, &work);
                // 虚拟 Bot 登记列表按选中 bot 刷新 + 收起 ⋯ 菜单（切 bot 残留展开态会串行）
                refresh_vb_rows(&w, &work);
                w.set_vb_menu_open(-1);
                // #147 团队列表按选中 bot 刷新 + 收起团队 ⋯ 菜单
                refresh_team_rows(&w, &work);
                w.set_team_menu_open(-1);
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
                    let _ = tx.send(UiCmd::Save(c));
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
        let hm = history_model.clone();
        clear_dialog.on_save_close(move || {
            // 确认清除：GUI 只读连接不能写消息库 → 落命令文件，service 的 history-gc
            // 消费执行（清空 messages.sqlite + unread.json）。这里先乐观清空列表，
            // 服务端确认后下个 2s tick 自然对齐。
            write_command_file("msg-clear.command");
            hm.set_vec(Vec::new());
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
            // ——设置窗已打开（dock 激活策略归它管）
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
                let missing: Vec<&str> = all
                    .iter()
                    .filter(|d| !d.found || (d.id == "codex" && !d.version_ok))
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
                // 审查 Minor：node 不一定在缺失清单里（只缺 codex 时没有 node 步）——
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
        // 虚拟 Bot 确认弹窗 weak（#75：解散/取消登记结果回填——成功关窗/失败可见）
        let vb_confirm_weak = vb_confirm.as_weak();
        // 确认弹窗待执行操作（失败重试时保留；成功才 take 清空）
        let vb_action_t = vb_action.clone();
        // #74 历史记录列表 model（历史页打开时每 tick 刷新）
        let history_model = history_model.clone();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(2),
            {
                let wk = wk.clone();
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
                                    Ok(tail) => {                                        w.set_dep_detail("".into());
                                        w.set_status_is_error(false);
                                        // #93：codex 装完的登录引导（run_install 成功返回已附）。
                                        // 其它依赖保持原样文案（npm/brew 输出冗长不直接上状态行）。
                                        if dep_id == "codex" {
                                            w.set_status_line(
                                                crate::agent::truncate(&tail, 200).into(),
                                            );
                                        } else {
                                            w.set_status_line(format!("✅ {dep_id} 安装完成").into());
                                        }
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
                                push_deps_to_window(&w); // 重检测：卡片/横幅/首页计数自动刷新
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
                                    let mut line = crate::deps::format_all_summary(&outcome);
                                    // #93：一键装里带 codex → 追加登录引导（与单项装同文案）
                                    if outcome.ok.iter().any(|id| id == "codex") {
                                        line.push_str(
                                            "；codex 首次使用请运行 `codex login` 或在「模型供应商」页配 API key",
                                        );
                                    }
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
                                                .map(|(rn, member, duty)| TeamRoleRow {
                                                    role_name: rn.into(),
                                                    member: member.into(),
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
                                            .map(|(rn, member, ok, detail)| TeamResultRow {
                                                text: team_create_line(&rn, &member, &detail).into(),
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
                                // 子窗口关闭不动 dock（8-20 用户反馈）
                                let _ = d.hide();
                            }
                            if let Some(w) = settings_weak.upgrade() {
                                w.set_status_line(format!("微信登录失败：{e}").into());
                            }
                        }
                    }
                }
                // #74 历史记录页刷新（仅该页打开时）：只读查询消息库 → 整体替换 model。
                // 与弹窗同 tick：点弹窗条目跳历史页后列表即时可见；清除命令执行后
                // 列表在下个 tick 自然清空。
                if let Some(w) = settings_weak.upgrade() {
                    if w.get_current_page() == 4 {
                        let rows = crate::msgstore::MsgStore::production().list_recent(1000);
                        let cfg = Config::load().unwrap_or_default();
                        sync_history_model(&history_model, &rows, &cfg);
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            rows[0],
            ("产品经理".into(), "小王".into(), "负责需求".into())
        );
        assert_eq!(rows[1], ("后端".into(), String::new(), "负责 API".into()));
    }

    #[test]
    fn team_create_line_formats_member_and_pending() {
        // #141：创建结果行「角色（成员）→ 详情」；member 空 → 待任命
        assert_eq!(
            team_create_line("产品经理", "小王", "已建"),
            "产品经理（小王）→ 已建"
        );
        assert_eq!(
            team_create_line("测试", "", "失败：群名冲突"),
            "测试（待任命）→ 失败：群名冲突"
        );
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
