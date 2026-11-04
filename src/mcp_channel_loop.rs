//! MCP 渠道的**事件循环**（总线 S2d）：把 S2/S2b/S2c 三段串起来。
//!
//! ## 它做的事
//!
//! ```text
//! spawn client → initialize → tools/list + resources/list → negotiate
//!   ↓ 循环
//!   有订阅：等通知 → resources/read → 消息
//!   无订阅：调 poll → 消息
//!   ↓
//!   deliver_batch（去重/LRU）→ mcp_adapter::to_ev → bridge.handle(ev)
//! ```
//!
//! ## 为什么这段单独成文件而不是塞进 `service::run_bot`
//!
//! `run_bot` 里已经有 ACP 句柄、工作区版本管理、回合投递三块，再塞一个
//! 「连外部进程 + 循环 + 协议协商」会让那 40 行变成 200 行且不可单测。
//! 这里只依赖三个**已经测过**的模块（`mcp_client`/`mcp_inbound`/`mcp_adapter`）
//! 加一个投递闭包，于是「循环本身」也能用假 client 单测。
//!
//! ## 状态上报
//!
//! 进循环前报「连接中」，成功取到一轮报「在线」，退出报「清空」。
//! **不能只报「连接中」然后就不管**：那样渠道死了托盘点还看得到「在线」，
//! 而本期最常见的故障就是 MCP 服务退出。
//!
//! ## 重连
//!
//! client 死掉（stdout 关闭）⇒ 退避重连，而不是让 bot 任务退出。
//! 但**连续失败超过上限就放弃**（否则一个永远起不来的配置会无限刷日志）。
//!
//! ## 阻塞风险
//!
//! `mcp_client` 是**同步**的（`std::process` + `read_until`），而本循环在 tokio 里。
//! 直接调用会阻塞 worker 线程 ⇒ 全部放到 `spawn_blocking` 里跑，
//! 用通道把消息送回 async 侧投递。这是本模块最容易写错的地方，有测试钉着。

use std::sync::Arc;

use crate::bridge::Bridge;
use crate::mcp_adapter::to_ev;
use crate::mcp_bus::ChannelMessage;
use crate::mcp_inbound_loop::{deliver_batch, InboundSink, InboundState};

/// 重连退避的起始与上限。
pub const BACKOFF_START: std::time::Duration = std::time::Duration::from_secs(2);
/// 退避封顶。**问过 Jev**：`cap_15` p=0.54 / `keep_60` p=0.45
/// （conf=0.31——**它自己也不确定，基本是抛硬币**）。选 15s 的实际理由：
/// 封顶即「服务恢复后最多等这么久才重新取消息」，IM 场景 60s 太迟；
/// 代价只是长故障期间每分钟 4 次请求而不是 1 次（子进程本地调用，可忽略）。
pub const BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(15);

/// 超过多少次连续失败后，从「指数退避」切到「稳定慢重试」。
///
/// **2026-11-03 owner 决定：永不放弃。**
/// 原设计是「连续 10 次就退出渠道」，理由是「一个永远起不来的配置无限重连会刷爆日志」。
/// 但那个理由只对**日志量**成立，代价却是「MCP 服务半夜崩了，渠道静默死到人发现为止」——
/// 而 IM 是随时会来消息的东西，静默死比刷日志严重得多。
///
/// 现在的设计：10 次以内仍按指数退避（**快速暴露配置错误**：约 2 分钟内就能从日志看出不对），
/// 超过 10 次转 [`RETRY_SLOW`] 固定间隔、**永不退出**。
/// 日志量由 `RETRY_SLOW` 控制：每 30s 一条，一天约 2880 条——可接受。
pub const MAX_CONSECUTIVE_FAILURES: u32 = 10;

/// 超过 [`MAX_CONSECUTIVE_FAILURES`] 之后的固定重试间隔。
///
/// 30s 是 owner 指定的值。它同时是「日志量上限」与「恢复延迟上限」的折中：
/// 服务恢复后最多等 30s 就重新取到消息，而长期故障时每 30s 才一条日志。
pub const RETRY_SLOW: std::time::Duration = std::time::Duration::from_secs(30);

/// 下一轮退避时长。
///
/// 两段式：
/// - `failures <= MAX_CONSECUTIVE_FAILURES`：指数增长（2s→4s→…→15s 封顶），
///   **快速暴露配置错误**
/// - 超过之后：固定 [`RETRY_SLOW`]（30s），**永不放弃**
///
/// 抽成纯函数是为了能直接测边界（0 次 / 刚好到阈值 / 远超阈值），不必等真实时间。
pub fn backoff(failures: u32) -> std::time::Duration {
    if failures == 0 {
        return BACKOFF_START;
    }
    if failures > MAX_CONSECUTIVE_FAILURES {
        return RETRY_SLOW;
    }
    let factor = 1u32 << failures.min(5); // 2^5=32，够了
    let d = BACKOFF_START.saturating_mul(factor);
    d.min(BACKOFF_MAX)
}

/// **永不放弃**（owner 决定，2026-11-03）。
///
/// 保留这个函数而不是删掉调用点，是为了让「放弃」这个决定只有**一处**——
/// 将来若要重新引入上限（比如加个 `max_retry_minutes` 配置），改这里就够。
pub fn should_give_up(_consecutive: u32) -> bool {
    false
}

/// 把消息投给 `Bridge` 的 sink。
///
/// 单独抽出来（而不是在循环里内联）是为了能测「翻译失败怎么办」：
/// `to_ev` 失败的消息**不入桥**，但要记日志——不能静默吞。
pub struct BridgeSink {
    bridge: Arc<Bridge>,
}

/// 准入判定的**纯函数**形态（`BridgeSink::admit` 只是从 bot 配置取值后转发）。
///
/// 抽出来是为了让判据顺序能被单测钉死——顺序错了不会报错，只会让
/// 「新建 bot 收不到消息」或「陌生人能白嫖 token」悄悄发生。
///
/// 三条判据（**前两条是并列的 `return true`，彼此顺序无关**——实测互换算过，
/// 行为等价。真正敏感的是它们必须都在第三条之前）：
///
/// 1. **空名单 ⇒ 放行**：那是 fail-closed 的「全部受限但都能对话」档，
///    不是「全部丢弃」——否则新建 bot 没填 owner 就完全收不到消息。
/// 2. `allow_ungranted` ⇒ 放行：用户显式要求放开。
/// 3. 名单命中 ⇒ 放行；否则丢弃。匹配**逐字节精确**（与 `to_ev` 判 owner 同口径），
///    且空 `sender_id` 永不算命中（名单里写了空串也不行）——否则一个「忘了带
///    sender_id」的渠道会静默全开。
///
/// 「顺序无关」这句是自己实测过的：把 1、2 对调后四条测试全绿——那是对的，
/// 因为它们都返回 `true`。写注释时差点把一个**等价的**顺序约束写成硬约束，
/// 那会让后来的人以为改了会坏。
fn admit_by(owner_ids: &[String], allow_ungranted: bool, sender_id: &str) -> bool {
    if owner_ids.is_empty() {
        return true;
    }
    if allow_ungranted {
        return true;
    }
    !sender_id.is_empty() && owner_ids.iter().any(|o| o == sender_id)
}

impl BridgeSink {
    /// 准入判定：这条消息该不该继续处理。
    ///
    /// 与 `config::Config::access_allows` 同语义，但**不看平台 kind**
    /// （MCP 渠道没有 kind 概念，名单来自 `mcp_channel.owner_ids`）。
    fn admit(&self, ev: &crate::bridge::Ev) -> bool {
        match self.bridge.bot.mcp_channel.as_ref() {
            // 没配渠道的 bot 根本不会走到这里（`messenger::build` 已挡），
            // 保守放行让后续逻辑报出真正的错误。
            None => true,
            Some(ch) => admit_by(&ch.owner_ids, ch.allow_ungranted, &ev.sender_id),
        }
    }

    /// 把**内联**引用内容（微信/钉钉形态）填进 `ev.quoted`。
    ///
    /// 附件按 `100 + i` 段向渠道要——它们就缓存在渠道进程里（入站时记下的），
    /// 与本条消息自己的 `0..n` 段分开，不会串。
    ///
    /// 与 `Bridge::fill_quoted` 的分工：那个走「按 id 反查」（飞书），这个走
    /// 「内容已经在手」（微信/钉钉）。两条路都填 `ev.quoted`，下游 `handle` 无感。
    async fn fill_inline_quoted(
        &self,
        ev: &mut crate::bridge::Ev,
        q: &crate::mcp_bus::ChannelQuoted,
    ) {
        let mut quoted = crate::messenger::QuotedContent {
            text: q.text.trim().to_string(),
            attachments: Vec::new(),
        };
        let bot_key = self.bridge.bot.key();
        for i in 0..q.attachment_count {
            // 下载失败**不丢整条引用**：文本可能是有用的，附件单独缺。
            if let Some(meta) = self
                .bridge
                .msgr
                .download_attachment(
                    &bot_key,
                    &ev.mid,
                    crate::mcp_bus::QUOTED_SEQ_BASE + i,
                    &crate::attachments::AttachmentDesc::Mcp,
                )
                .await
            {
                quoted.attachments.push(meta);
            }
        }
        if quoted.text.is_empty() && quoted.attachments.is_empty() {
            // 内联引用声明了但什么都没拿到 ⇒ 不设，免得下游出现空引用块。
            return;
        }
        crate::log!(
            "[mcp] 内联引用 mid={} 文本长度={} 附件数={}",
            crate::agent::truncate(&ev.mid, 12),
            quoted.text.chars().count(),
            quoted.attachments.len()
        );
        ev.quoted = quoted;
    }

    /// 尝试把这条消息当授权码消费。返回 `true` = 已处理（调用方必须**就此返回**）。
    ///
    /// 三步：查展示名（best-effort）→ 消费 → 按结果回执。
    /// 只有 `NotFound`（不匹配任何待消费码）才算「不是授权码」，其余两种都要回话。
    async fn try_consume_owner_code(&self, ev: &mut crate::bridge::Ev) -> bool {
        let text = ev.text.trim().to_string();
        if text.is_empty() {
            return false;
        }
        use crate::config::OwnerCodeResult as R;
        // 查名放消费之前：失败不阻塞授权（名字只是展示用，落了 id 也能认）。
        let name = self.bridge.msgr.user_display_name(&ev.sender_id).await;
        let r = crate::config::Config::consume_owner_code(
            &self.bridge.bot.key(),
            &text,
            &ev.sender_id,
            name.as_deref().unwrap_or(""),
        );
        let reply = match r {
            R::Admitted => "✅ 授权成功，你现在可以在这个 bot 里对话了。",
            R::Expired => "❌ 授权码已过期，请联系管理员重新生成。",
            // 不匹配任何待消费码 ⇒ 当普通消息，不打扰。
            R::NotFound => return false,
        };
        if let Err(e) = self.bridge.msgr.send_text(&ev.chat_id, reply).await {
            crate::log!("[mcp] 授权码回执发送失败 chat={}：{e:#}", ev.chat_id);
        }
        crate::log!(
            "[mcp] 授权码已处理（bot={} sender={} result={r:?}）",
            self.bridge.bot.key(),
            ev.sender_id
        );
        true
    }

    pub fn new(bridge: Arc<Bridge>) -> Self {
        Self { bridge }
    }

    /// 本 bot 的 owner 名单（`mcp_channel.owner_ids` 的构造时快照）。
    ///
    /// 取 `bridge.bot` 而不是每次热读 config.json：渠道循环持有的本来就是启动快照
    /// （`run_bot` 传进来的那个 bot），用同一份才能与「拉起哪个程序」保持口径一致。
    /// 名单改动要生效就得重启 service——所以 `bots_struct_sig` 把名单也入了签名。
    fn owner_ids(&self) -> &[String] {
        self.bridge
            .bot
            .mcp_channel
            .as_ref()
            .map(|c| c.owner_ids.as_slice())
            .unwrap_or(&[])
    }
}

#[async_trait::async_trait]
impl InboundSink for BridgeSink {
    async fn deliver(&self, msg: &ChannelMessage) -> Result<(), String> {
        let ev = match to_ev(msg, self.owner_ids()) {
            Ok(ev) => ev,
            Err(e) => {
                // 翻译失败 ⇒ 这条进不了桥。**记日志但不让整批失败**：
                // 一条畸形消息不该阻断同批其它消息。
                crate::log!(
                    "[mcp] 消息无法进桥（chat={} mid={}）：{e}",
                    msg.chat_id,
                    msg.message_id
                );
                return Err(e);
            }
        };
        // 准入闸（**与内置渠道退役前一致**）：不在 `owner_ids` 里的发送者，
        // 其消息直接丢弃、不回复。
        //
        // 退役前 `access_allows` 就是 `!owner_ids.is_empty() && is_owner_allowed(...)`。
        // MCP 路径保留同一语义，但可以用 `allow_ungranted` 显式放开
        // （放开后是「受限会话」——不能执行命令、不能读写文件，但能对话）。
        //
        // 三条判据的顺序不能换：
        // - **空名单先放行**：那是 fail-closed 的「全部受限但都能对话」档，
        //   不是「全部丢弃」——否则新建 bot 没填 owner 就完全收不到消息；
        // - `allow_ungranted` 次之：用户显式要求放开；
        // - 最后才按名单丢。
        if !self.admit(&ev) {
            crate::log!(
                "[mcp] 丢弃未授权消息 bot={} sender={}（不在 owner_ids 且未开 allow_ungranted）",
                self.bridge.bot.key(),
                ev.sender_id
            );
            return Ok(());
        }

        // 附件下载：**在这里做，不在 `to_ev` 里**。
        //
        // `to_ev` 是纯函数（无 IO、可单测），而下载要等渠道 server 回包。
        // 让纯函数去下载会把「翻译」和「IO」绑死，单测就得起子进程。
        //
        // 顺序与内置渠道一致（`bridge/mod.rs` 的飞书路径也是先下载再建 Ev）：
        // 下载失败**不丢消息**——`download_attachment` 返回的 meta 自带 `note`，
        // agent 会如实告诉用户「附件拿不到」。
        let mut ev =
            download_attachments(self.bridge.msgr.as_ref(), &self.bridge.bot.key(), ev).await;
        // 引用/回复：**两种形态，内联优先**。
        //
        // 1. `msg.quoted`（微信/钉钉）：内容随入站帧到达，没有原消息 id。
        // 2. `parent_id`（飞书）：按 id 反查。
        //
        // 为什么必须先判内联：微信/钉钉**根本没有 parent_id**，走反查路径会在
        // `fill_quoted` 第一行就 return，引用内容静默丢失（用户在群里引用一条消息
        // 问「这个啥意思」，agent 看不到被引用的是什么）。
        //
        // 位置在 `handle` 之前、本条附件下载之后：引用附件用 `100 + i` 的 seq 段，
        // 与本条消息的 `0..n` 不冲突。
        if let Some(q) = msg.quoted.as_ref() {
            self.fill_inline_quoted(&mut ev, q).await;
        } else {
            self.bridge.fill_quoted(&mut ev, &msg.parent_id).await;
        }

        // 授权码消费（**必须是 `handle` 之前**）：命中就回执并吞掉这条消息，
        // 不让它进 agent——一个授权码不该触发一轮对话。
        //
        // 与内置渠道的差别：不判 `is_p2p`。MCP 路径的 `chat_type` 恒为 `"mcp"`，
        // 而渠道 server 已经把「没 @ bot 的群消息」过滤掉了；且
        // `consume_owner_code` 对**不匹配的文本返回 `NotFound`**（不是报错），
        // 所以「不判 p2p」不会让群里的随便一句话被当成授权码。
        if self.try_consume_owner_code(&mut ev).await {
            return Ok(());
        }

        // 未读提醒（GUI 小红点 + 托盘通知）。
        //
        // **不判 chat_type**（2026-11-04 Jev `always_notify` p=0.94/conf=0.92）：
        // 内置渠道时代这里判 `chat_type == "p2p"`（只提醒私聊），但 MCP 路径的
        // chat_type 恒为 `"mcp"`——那是有意的（填 `p2p` 会让它参与主会话选举）。
        //
        // 也不需要新判据：**渠道 server 已经做过过滤**，没 @ bot 的群消息根本不会
        // 进到总线。所以进到这里的每条消息，事实上都是「对 bot 说的」。
        //
        // 顺序：放在 `handle` 之前——`handle` 里可能长时间跑 agent，先记提醒才能让
        // 用户立刻看到「有人找你」；失败只记日志，不阻断消息处理。
        self.bridge.unread.report(
            &self.bridge.bot.key(),
            &ev.sender_id,
            &msg.sender_name,
            &crate::agent::truncate(&ev.text, 40),
            ev.ts,
        );

        self.bridge.handle(ev).await;
        Ok(())
    }
}

/// 把 `Ev` 里「只有占位、没有本地路径」的附件真的下载下来。
///
/// 不下载的直接后果（本轮修的）：用户发图，agent 只看到「有一个附件」，
/// **读不到内容**，于是答非所问——而 ABB 不报任何错。
///
/// 抽成自由函数（不挂在 `BridgeSink` 上）纯粹为了**可测**：`BridgeSink` 要构造
/// 整个 `Bridge`（会话、agent、运行时目录），而这段逻辑只依赖 `Messenger`。
pub async fn download_attachments(
    msgr: &dyn crate::messenger::Messenger,
    bot_key: &str,
    mut ev: crate::bridge::Ev,
) -> crate::bridge::Ev {
    if ev.attachments.is_empty() {
        return ev;
    }
    for (i, meta) in ev.attachments.iter_mut().enumerate() {
        // 已经有本地路径的（渠道直接给了落盘位置）不再下载。
        if !meta.path.trim().is_empty() {
            continue;
        }
        // `seq` 必须与渠道侧 `attachments` 数组同序——入站解析时两侧按同一
        // 顺序生成，这里用下标即对得上（跨包 e2e 覆盖了这条）。
        let desc = crate::attachments::AttachmentDesc::Mcp;
        match msgr.download_attachment(bot_key, &ev.mid, i, &desc).await {
            Some(got) => *meta = got,
            None => {
                // 渠道没声明 `download_attachment`（契约 §3.4 可选能力）：
                // 保留占位并在 note 里说清，**不静默**。
                if meta.note.trim().is_empty() {
                    meta.note = format!(
                        "渠道 `{bot_key}` 未声明 download_attachment 能力 ⇒ 附件内容取不到"
                    );
                }
            }
        }
    }
    ev
}

/// 一次拉取的抽象：让循环本身能单测（真实现走 `mcp_inbound`）。
///
/// 用 trait 而不是直接调 `mcp_inbound`：那段要真子进程，测不了「循环的重连逻辑」。
#[async_trait::async_trait]
///
/// 只需要 `Send` 不需要 `Sync`：真实现内部持有 `StdioChannelClient`，而它含有
/// `mpsc::Receiver`（**不是 `Sync`**）。这不是妥协——`fetch(&mut self)` 本来就是
/// 独占语义，多线程共享它没有意义。
pub trait InboundSource: Send {
    /// 取一批消息。`Err` 表示这一轮失败（调用方据此退避/计数）。
    async fn fetch(&mut self) -> Result<Vec<ChannelMessage>, String>;
}

/// MCP 渠道的**主循环**（S2d 接线点）。
///
/// 起 client → 握手 → 能力协商 → 循环取消息投给 Bridge。
/// client 死掉就退避重连；连续失败超上限则退出（不无限刷日志）。
pub async fn run(
    bot_key: &str,
    name: &str,
    program: &str,
    args: &[String],
    envs: &[(String, String)],
    bridge: std::sync::Arc<Bridge>,
    stop: tokio_util::sync::CancellationToken,
) {
    let sink = BridgeSink::new(bridge);
    let mut consecutive: u32 = 0;
    loop {
        if stop.is_cancelled() {
            return;
        }
        crate::botstatus::report(bot_key, "mcp", name, "连接中");

        // 建连 + 协商（同步，放阻塞线程）。
        let name_owned = name.to_string();
        let program_owned = program.to_string();
        let args_owned: Vec<String> = args.to_vec();
        let envs_owned: Vec<(String, String)> = envs.to_vec();
        let built = tokio::task::spawn_blocking(move || {
            build_source(&name_owned, &program_owned, &args_owned, &envs_owned)
        })
        .await;

        let mut source = match built {
            Ok(Ok(src)) => {
                consecutive = 0;
                crate::botstatus::report(bot_key, "mcp", name, "在线");
                crate::log!("[mcp:{bot_key}] 渠道 `{name}` 已连上");
                src
            }
            Ok(Err(e)) => {
                consecutive += 1;
                crate::log!("[mcp:{bot_key}] 连接渠道 `{name}` 失败（第 {consecutive} 次）：{e}");
                // **当前恒为 false**（owner 决定永不放弃，2026-11-03）。
                // 保留这个分支是因为「要不要放弃」这个决定只该有一处；
                // 真要重新引入上限（比如加 max_retry_minutes 配置）时改 should_give_up 即可。
                if should_give_up(consecutive) {
                    crate::log!(
                        "[mcp:{bot_key}] 连续失败 {consecutive} 次，放弃重连（检查 mcp_channel 配置）"
                    );
                    crate::botstatus::report(bot_key, "mcp", name, "连接失败");
                    return;
                }
                crate::botstatus::report(bot_key, "mcp", name, "重连中");
                if wait_or_cancel(&stop, backoff(consecutive)).await {
                    return;
                }
                continue;
            }
            Err(e) => {
                crate::log!("[mcp:{bot_key}] 建连任务 join 失败：{e}");
                return;
            }
        };

        // 取消息循环。一次 fetch 失败不退出：可能是临时故障，退避后接着跑。
        let mut state = crate::mcp_inbound_loop::InboundState::new(4096);
        loop {
            if stop.is_cancelled() {
                return;
            }
            match tick(&mut state, source.as_mut(), &sink).await {
                Ok(report) => {
                    if report.delivered > 0 {
                        consecutive = 0;
                        crate::botstatus::report(bot_key, "mcp", name, "在线");
                    }
                    let delay = crate::mcp_inbound_loop::next_delay(&report);
                    if wait_or_cancel(&stop, delay).await {
                        return;
                    }
                }
                Err(e) => {
                    consecutive += 1;
                    crate::log!("[mcp:{bot_key}] 取消息失败（第 {consecutive} 次）：{e}");
                    crate::botstatus::report(bot_key, "mcp", name, "重连中");
                    // 同上：当前恒为 false，永不退出。
                    if should_give_up(consecutive) {
                        crate::log!("[mcp:{bot_key}] 连续失败 {consecutive} 次，退出渠道");
                        return;
                    }
                    if wait_or_cancel(&stop, backoff(consecutive)).await {
                        return;
                    }
                    // 断开重连：丢掉当前 source，回外层重新建连。
                    break;
                }
            }
        }
    }
}

/// 等一段时间；被取消则返回 `true`（调用方据此退出）。
async fn wait_or_cancel(
    stop: &tokio_util::sync::CancellationToken,
    d: std::time::Duration,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => false,
        _ = stop.cancelled() => true,
    }
}

/// 建连 + 协商入站方式（同步；由调用方放进 `spawn_blocking`）。
/// 测试可见的建连入口（端到端全链路用）。
///
/// 与生产 [`run`] 走同一个 `build_source`，避免"测试走另一条路"的假绿。
#[cfg(test)]
pub fn build_source_for_test(
    name: &str,
    program: &str,
    args: &[String],
    envs: &[(String, String)],
) -> Result<Box<dyn InboundSource>, String> {
    build_source(name, program, args, envs)
}

fn build_source(
    name: &str,
    program: &str,
    args: &[String],
    envs: &[(String, String)],
) -> Result<Box<dyn InboundSource>, String> {
    let mut client = crate::mcp_client::StdioChannelClient::spawn(name, program, args, envs)
        .map_err(|e| format!("起不来渠道服务：{e}"))?;
    // 握手失败不致命（tools/list 才是真判据），与 `McpChannelMessenger::connect` 同口径。
    let _ = client.call(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "abb", "version": env!("CARGO_PKG_VERSION")},
        }),
    );
    let tools = client
        .call("tools/list", serde_json::json!({}))
        .map_err(|e| format!("tools/list 失败：{e}"))?;
    let resources = client
        .call("resources/list", serde_json::json!({}))
        .unwrap_or(serde_json::Value::Null);
    let caps = crate::mcp_bus::capabilities_from(&tools, &resources)
        .map_err(|e| format!("能力清单形状不对：{e:?}"))?;
    let inbound = crate::mcp_inbound::negotiate(&mut client, &caps).ok_or_else(|| {
        "既没有可订阅的 inbox 资源，也没有 poll 工具 ⇒ 该渠道收不到消息".to_string()
    })?;
    Ok(match inbound {
        crate::mcp_inbound::ChannelInbound::Subscribe { .. } => {
            Box::new(SubscribeSource::new(client, inbound, DEFAULT_FETCH_LIMIT))
        }
        crate::mcp_inbound::ChannelInbound::Poll => {
            Box::new(PollSource::new(client, DEFAULT_FETCH_LIMIT))
        }
    })
}

/// 单次取消息上限。
pub const DEFAULT_FETCH_LIMIT: usize = 100;

/// 跑一轮：取 → 去重投递。返回本轮报告（失败会体现为 `Err`）。
pub async fn tick(
    state: &mut InboundState,
    source: &mut dyn InboundSource,
    sink: &dyn InboundSink,
) -> Result<crate::mcp_inbound_loop::TickReport, String> {
    let batch = source.fetch().await?;
    Ok(deliver_batch(state, sink, &batch).await)
}

/// 真实现：一条长驻的订阅连接（A 方案）。
///
/// 每次 `fetch` 先排空通知通道，再对涉及的 URI 逐个 `resources/read`。
/// 没通知就返回空批次（**不是错误**：没新消息是常态）。
pub struct SubscribeSource {
    client: Option<crate::mcp_client::StdioChannelClient>,
    inbound: crate::mcp_inbound::ChannelInbound,
    limit: usize,
}

impl SubscribeSource {
    pub fn new(
        client: crate::mcp_client::StdioChannelClient,
        inbound: crate::mcp_inbound::ChannelInbound,
        limit: usize,
    ) -> Self {
        Self {
            client: Some(client),
            inbound,
            limit,
        }
    }
}

#[async_trait::async_trait]
impl InboundSource for SubscribeSource {
    async fn fetch(&mut self) -> Result<Vec<ChannelMessage>, String> {
        // 同步 client 放进 spawn_blocking —— 见模块注释「阻塞风险」。
        // `spawn_blocking` 要求 'static，所以把 client **移出**再移回（`&mut self` 独占，
        // 不会有人并发用）。用 `Option::take` 而不是 `mem::replace` + 占位进程：
        // 占位进程会真的 spawn 一个 /bin/true，白起进程还可能失败。
        let client = self
            .client
            .take()
            .ok_or_else(|| "内部错误：client 已被取走（上一次 fetch 没归还？）".to_string())?;
        let inbound = self.inbound.clone();
        let limit = self.limit;
        let res = tokio::task::spawn_blocking(move || {
            let mut c = client;
            let notes = c.drain_notifications();
            let out = if notes.is_empty() {
                Ok(vec![])
            } else {
                crate::mcp_inbound::messages_from_notifications(&mut c, &inbound, &notes, limit)
            };
            (c, out)
        })
        .await
        .map_err(|e| format!("读资源任务 join 失败：{e}"))?;
        let (client, out) = res;
        // **无论成功失败都归还**：否则下一轮 fetch 直接报「client 被取走」，
        // 一次偶发失败会变成渠道永久失效。
        self.client = Some(client);
        out
    }
}

/// 真实现：轮询（B 方案）。
pub struct PollSource {
    client: Option<crate::mcp_client::StdioChannelClient>,
    state_cursor: Option<String>,
    limit: usize,
}

impl PollSource {
    pub fn new(client: crate::mcp_client::StdioChannelClient, limit: usize) -> Self {
        Self {
            client: Some(client),
            state_cursor: None,
            limit,
        }
    }
}

#[async_trait::async_trait]
impl InboundSource for PollSource {
    async fn fetch(&mut self) -> Result<Vec<ChannelMessage>, String> {
        let client = self
            .client
            .take()
            .ok_or_else(|| "内部错误：client 已被取走".to_string())?;
        let cursor = self.state_cursor.clone();
        let limit = self.limit;
        let res = tokio::task::spawn_blocking(move || {
            let mut c = client;
            let out = crate::mcp_inbound::poll_once(&mut c, cursor.as_deref(), limit);
            (c, out)
        })
        .await
        .map_err(|e| format!("轮询任务 join 失败：{e}"))?;
        let (client, out) = res;
        self.client = Some(client);
        let batch = out?;
        // 游标只在成功后推进（S2b 的口径）——`out?` 已经把失败挡在上面了。
        self.state_cursor = batch.next_cursor;
        Ok(batch.messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_inbound_loop::TickReport;
    use std::sync::Mutex;

    // ── 准入闸：不在 owner 名单的消息该不该丢 ─────────────────────
    //
    // 这一批测 `BridgeSink::admit` 的**四条判据及其顺序**。顺序错了不会报错，
    // 只会让「新建 bot 收不到消息」或「陌生人能白嫖 token」悄悄发生。

    /// 判定入口：直接调纯函数（不建 Bridge——它读的就是那两个字段）。
    fn admit_with(owner_ids: &[&str], allow_ungranted: bool, sender: &str) -> bool {
        let ids: Vec<String> = owner_ids.iter().map(|s| s.to_string()).collect();
        admit_by(&ids, allow_ungranted, sender)
    }

    /// **空名单 ⇒ 放行**（fail-closed 的「全部受限但都能对话」档）。
    ///
    /// 这条最容易写反：直觉会认为「没有名单 = 谁都不许用」，但那会让**新建 bot
    /// 还没填 owner 时完全收不到消息**，用户只会看到「机器人坏了」。
    #[test]
    fn empty_owner_list_admits_everyone() {
        assert!(admit_with(&[], false, "ou_stranger"), "空名单必须放行");
        assert!(admit_with(&[], false, ""), "空名单下空 sender 也放行");
    }

    /// 名单命中 ⇒ 放行；未命中 ⇒ 丢弃。
    #[test]
    fn listed_sender_admits_others_drop() {
        assert!(admit_with(&["ou_boss"], false, "ou_boss"), "名单内必须放行");
        assert!(
            !admit_with(&["ou_boss"], false, "ou_other"),
            "名单外必须丢弃"
        );
    }

    /// `allow_ungranted: true` ⇒ 名单外也放行（用户显式要求）。
    #[test]
    fn allow_ungranted_opens_the_gate() {
        assert!(
            admit_with(&["ou_boss"], true, "ou_other"),
            "显式放开后名单外也要放行"
        );
    }

    /// **空 `sender_id` 永不算命中**，哪怕名单里写了空串。
    ///
    /// 否则一个「忘了带 sender_id」的渠道会静默全开。
    #[test]
    fn empty_sender_is_never_admitted_by_list() {
        assert!(
            !admit_with(&["ou_boss", ""], false, ""),
            "空 sender 不能因为名单里有空串就被放行"
        );
    }

    // ── 附件下载接线：不下载 = 用户发图 agent 读不到内容 ─────────────
    //
    // 这一批测的是**行为**，不是解析：`download_attachments` 有没有真的去调
    // messenger、有没有把结果写回 `Ev`、没能力时有没有留下说明。

    /// 假 messenger：只实现 `download_attachment`，记录被问过的 `(mid, seq)`。
    ///
    /// `capable=false` 时返回 `None`——那正是「渠道没声明这个可选能力」的形状
    /// （trait 默认实现就返回 `None`）。
    struct AttachMsgr {
        capable: bool,
        asked: std::sync::Mutex<Vec<(String, usize)>>,
    }

    impl AttachMsgr {
        fn new(capable: bool) -> Self {
            Self {
                capable,
                asked: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::messenger::Messenger for AttachMsgr {
        async fn send_text(&self, _c: &str, _t: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn send_attachment(
            &self,
            _c: &str,
            _m: &crate::attachments::AttachmentMeta,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn download_attachment(
            &self,
            _bot_key: &str,
            mid: &str,
            seq: usize,
            _desc: &crate::attachments::AttachmentDesc,
        ) -> Option<crate::attachments::AttachmentMeta> {
            self.asked.lock().unwrap().push((mid.to_string(), seq));
            if !self.capable {
                return None;
            }
            Some(crate::attachments::AttachmentMeta {
                kind: "image".into(),
                source: "mcp".into(),
                file_name: format!("{mid}-{seq}.png"),
                mime: "image/png".into(),
                size: 7,
                path: format!("/tmp/{mid}-{seq}.png"),
                sha256: String::new(),
                note: String::new(),
            })
        }
    }

    fn ev_with(n: usize) -> crate::bridge::Ev {
        crate::bridge::Ev {
            mid: "m1".into(),
            chat_id: "c1".into(),
            chat_type: "mcp".into(),
            chat_name: String::new(),
            thread_id: String::new(),
            sender_id: "u1".into(),
            text: "看图".into(),
            ts: 1,
            quoted: Default::default(),
            role: crate::config::SenderRole::Granted,
            attachments: (0..n)
                .map(|i| crate::attachments::AttachmentMeta {
                    kind: "image".into(),
                    source: "mcp".into(),
                    file_name: format!("a{i}.png"),
                    mime: String::new(),
                    size: 0,
                    // **空路径**：正是「渠道只给了占位」的形状。
                    path: String::new(),
                    sha256: String::new(),
                    note: String::new(),
                })
                .collect(),
        }
    }

    /// 渠道支持 ⇒ 每个附件都下载、路径写回、`seq` 是下标。
    ///
    /// **这是本轮修的主缺陷**：修之前 `mcp_adapter` 直接造占位 `AttachmentMeta`，
    /// 从不调用 `download_attachment` ⇒ agent 只看到「有一个附件」，读不到内容，
    /// 而 ABB 不报任何错。
    #[tokio::test]
    async fn supported_channel_downloads_every_attachment() {
        let m = AttachMsgr::new(true);
        let ev = download_attachments(&m, "bot", ev_with(3)).await;

        let asked = m.asked.lock().unwrap().clone();
        assert_eq!(
            asked,
            vec![
                ("m1".to_string(), 0),
                ("m1".to_string(), 1),
                ("m1".to_string(), 2)
            ],
            "必须按 seq=0,1,2 逐个问，且用消息 id"
        );
        assert_eq!(ev.attachments.len(), 3);
        for (i, a) in ev.attachments.iter().enumerate() {
            assert_eq!(
                a.path,
                format!("/tmp/m1-{i}.png"),
                "第 {i} 个要拿到自己的路径"
            );
            assert_eq!(a.size, 7, "下载后的元数据要覆盖占位");
        }
    }

    /// 渠道不支持 ⇒ **保留占位并写清原因**，不静默、不丢消息。
    #[tokio::test]
    async fn unsupported_channel_leaves_an_explanatory_note() {
        let m = AttachMsgr::new(false);
        let ev = download_attachments(&m, "bot", ev_with(1)).await;

        assert_eq!(ev.attachments.len(), 1, "消息里的附件条目不能消失");
        assert!(ev.attachments[0].path.is_empty());
        let note = &ev.attachments[0].note;
        assert!(
            note.contains("download_attachment") && note.contains("bot"),
            "note 要点名缺的能力和渠道：{note:?}"
        );
    }

    /// 已经有本地路径的**不再下载**（渠道直接给了落盘位置）。
    ///
    /// 这条防的是「无谓的第二次下载」：重复下载既慢又可能把文件覆盖成另一份。
    #[tokio::test]
    async fn attachment_with_existing_path_is_not_re_downloaded() {
        let m = AttachMsgr::new(true);
        let mut ev = ev_with(2);
        ev.attachments[0].path = "/already/there.png".into();

        let ev = download_attachments(&m, "bot", ev).await;

        let asked = m.asked.lock().unwrap().clone();
        assert_eq!(asked, vec![("m1".to_string(), 1)], "只该问没路径的那个");
        assert_eq!(
            ev.attachments[0].path, "/already/there.png",
            "不覆盖已有路径"
        );
        assert_eq!(ev.attachments[1].path, "/tmp/m1-1.png");
    }

    /// 没有附件 ⇒ 一次都不问（别为无附件的消息白起 IO）。
    #[tokio::test]
    async fn no_attachments_means_no_calls() {
        let m = AttachMsgr::new(true);
        let ev = download_attachments(&m, "bot", ev_with(0)).await;
        assert!(m.asked.lock().unwrap().is_empty());
        assert!(ev.attachments.is_empty());
    }

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff(0), BACKOFF_START);
        assert_eq!(backoff(1), BACKOFF_START * 2);
        assert_eq!(backoff(2), BACKOFF_START * 4);
        // 指数段封顶：5 次以上不再增长，到 15s
        assert_eq!(backoff(5), BACKOFF_MAX);
        // **阈值处仍是指数段**（`> MAX` 才切慢档，`== MAX` 不切）
        assert_eq!(backoff(MAX_CONSECUTIVE_FAILURES), BACKOFF_MAX);
    }

    /// 阈值之后**每个**失败次数都落在同一个值上：`RETRY_SLOW`（30s）。
    ///
    /// 这是「永不放弃 + 稳定重试」的**数值**保证：服务恢复后最多等 30s 就重新取到消息，
    /// 长期故障时也还是每 30s 一条日志（不会随失败次数继续拉长，也不会溢出）。
    /// 逐个值扫而不是只点三四个样本：删掉 `backoff` 里那个 `> MAX_CONSECUTIVE_FAILURES`
    /// 的早返回分支时，指数段会把间隔算成 15s 封顶——**必须**在第一个越界值上就咬住。
    #[test]
    fn backoff_after_many_failures_is_stable_30s() {
        // 刚过阈值
        assert_eq!(backoff(MAX_CONSECUTIVE_FAILURES + 1), RETRY_SLOW);
        assert_eq!(backoff(MAX_CONSECUTIVE_FAILURES + 2), RETRY_SLOW);
        // 远超阈值
        assert_eq!(backoff(100), RETRY_SLOW);
        // 极端值不溢出、不回落到指数段
        assert_eq!(backoff(u32::MAX), RETRY_SLOW);
        // 阈值之后**全部**等长（含 500 这种远超样本）
        for n in (MAX_CONSECUTIVE_FAILURES + 1)..=500 {
            assert_eq!(
                backoff(n),
                RETRY_SLOW,
                "连续失败 {n} 次后的间隔必须稳定在 {RETRY_SLOW:?}"
            );
        }
        // 慢档要真的更慢，否则「10 次后转慢」这个策略没有意义
        assert!(RETRY_SLOW > BACKOFF_MAX, "慢档必须真的更慢");
    }

    /// **任何**连续失败次数都不放弃（owner 决定 2026-11-03）。
    ///
    /// 这条钉的是「进程死了渠道不会静默失效」：旧策略在 10 次处返回 `true`，
    /// 于是一次夜间崩溃就把渠道永久关掉。逐个值扫 0..=300 再补上极端值——
    /// 把 `should_give_up` 改回 `consecutive >= 10` 时，10 这个点会立刻红。
    #[test]
    fn consecutive_failures_never_trigger_give_up() {
        for n in (0..=300).chain([1000, MAX_CONSECUTIVE_FAILURES, u32::MAX]) {
            assert!(
                !should_give_up(n),
                "连续失败 {n} 次也不得放弃——owner 决定：永不退出，每 30s 重试"
            );
        }
    }

    fn msg(id: &str) -> ChannelMessage {
        ChannelMessage {
            chat_id: "c".into(),
            message_id: id.into(),
            sender_id: String::new(),
            sender_name: String::new(),
            text: "hi".into(),
            ts: 1,
            attachments: vec![],
            parent_id: String::new(),
            quoted: None,
        }
    }

    /// 可配置返回：第 n 次失败 / 返回哪些消息。
    struct FakeSource {
        /// 依次返回：`Some(msgs)` = 成功，`None` = 失败。
        script: Mutex<Vec<Option<Vec<ChannelMessage>>>>,
    }

    impl FakeSource {
        fn new(script: Vec<Option<Vec<ChannelMessage>>>) -> Self {
            Self {
                script: Mutex::new(script),
            }
        }
    }

    #[async_trait::async_trait]
    impl InboundSource for FakeSource {
        async fn fetch(&mut self) -> Result<Vec<ChannelMessage>, String> {
            let mut s = self.script.lock().unwrap();
            if s.is_empty() {
                return Ok(vec![]);
            }
            match s.remove(0) {
                Some(m) => Ok(m),
                None => Err("拉取失败".into()),
            }
        }
    }

    struct CountSink {
        got: Mutex<Vec<String>>,
    }

    impl CountSink {
        fn new() -> Self {
            Self {
                got: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl InboundSink for CountSink {
        async fn deliver(&self, m: &ChannelMessage) -> Result<(), String> {
            self.got.lock().unwrap().push(m.message_id.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn tick_delivers_what_source_returns() {
        let mut st = InboundState::new(10);
        let mut src = FakeSource::new(vec![Some(vec![msg("1"), msg("2")])]);
        let sink = CountSink::new();
        let r = tick(&mut st, &mut src, &sink).await.unwrap();
        assert_eq!(r.delivered, 2);
    }

    /// **拉取失败要冒泡成 Err**，不能当成「这轮没消息」——
    /// 后者会让循环永远以为渠道正常。
    #[tokio::test]
    async fn tick_propagates_fetch_failure() {
        let mut st = InboundState::new(10);
        let mut src = FakeSource::new(vec![None]);
        let sink = CountSink::new();
        let err = tick(&mut st, &mut src, &sink).await.unwrap_err();
        assert!(err.contains("拉取失败"));
    }

    /// 失败的那一轮**不该影响下一轮**：状态要能继续用。
    #[tokio::test]
    async fn failure_round_does_not_poison_state() {
        let mut st = InboundState::new(10);
        let mut src = FakeSource::new(vec![None, Some(vec![msg("1")])]);
        let sink = CountSink::new();
        assert!(tick(&mut st, &mut src, &sink).await.is_err());
        let r = tick(&mut st, &mut src, &sink).await.unwrap();
        assert_eq!(r.delivered, 1);
    }

    /// **source 连续报错不会杀死循环**：前两轮 `fetch` 返回 `Err`，第三轮恢复。
    ///
    /// 断言三件事：
    /// 1. 失败轮**都**以 `Err` 冒泡（循环据此计数/退避），不是伪装成「没消息」；
    /// 2. 失败轮之后 source 还能被同一个循环继续用（恢复那轮真的投出去）；
    /// 3. `InboundState`（去重/LRU）**没被污染**——恢复后同一 mid 仍按去重处理。
    ///
    /// 第 3 点才是「静默失效」的判据：若失败轮把状态清掉/占了坑，渠道会看起来在线
    /// 却把消息重复投递或整批丢掉。
    #[tokio::test]
    async fn source_error_does_not_kill_the_loop() {
        let mut st = InboundState::new(10);
        let mut src = FakeSource::new(vec![None, None, Some(vec![msg("1")]), Some(vec![msg("1")])]);
        let sink = CountSink::new();

        // 1、2 轮：失败要冒泡，且状态与 source 都还活着
        for round in 1..=2 {
            let err = tick(&mut st, &mut src, &sink).await.unwrap_err();
            assert!(
                err.contains("拉取失败"),
                "第 {round} 轮失败原因应原样冒泡，实际：{err}"
            );
            assert!(
                sink.got.lock().unwrap().is_empty(),
                "第 {round} 轮失败不该投出任何消息"
            );
        }

        // 第 3 轮：恢复，消息正常投递
        let r = tick(&mut st, &mut src, &sink).await.unwrap();
        assert_eq!(r.delivered, 1, "恢复轮必须把消息投出去");
        assert_eq!(*sink.got.lock().unwrap(), vec!["1".to_string()]);

        // 第 4 轮：同一 mid 仍按去重处理 ⇒ 失败轮没有污染去重状态
        let r = tick(&mut st, &mut src, &sink).await.unwrap();
        assert_eq!(r.delivered, 0);
        assert_eq!(r.deduped, 1, "恢复后去重记录要还在（失败轮不得清状态）");
    }

    /// **越过旧上限（10 次）之后仍然能恢复**：失败 12 轮再成功，照旧投递。
    ///
    /// 这是「进程挂了很久」的回归：旧策略在第 11 次失败时就退出渠道，
    /// 于是 MCP 服务夜间崩溃 ⇒ 渠道静默死到人发现为止。这条从**循环决策**的角度
    /// 钉住恢复能力：0..=12 全都不放弃 + 第 12 次之后仍是 30s 慢重试 +
    /// 第 13 轮恢复投递。
    #[tokio::test]
    async fn recovers_after_more_failures_than_the_old_give_up_threshold() {
        const ROUNDS: u32 = 12;
        let mut scroll: Vec<Option<Vec<ChannelMessage>>> = vec![None; ROUNDS as usize];
        scroll.push(Some(vec![msg("late")]));
        let mut src = FakeSource::new(scroll);
        let mut st = InboundState::new(10);
        let sink = CountSink::new();

        for consecutive in 1..=ROUNDS {
            assert!(tick(&mut st, &mut src, &sink).await.is_err());
            // 每一轮都按循环的口径复查决策——包括恰好越过旧上限的 11、12 轮
            assert!(
                !should_give_up(consecutive),
                "第 {consecutive} 轮失败后不得放弃重连"
            );
            let delay = backoff(consecutive);
            assert!(delay <= RETRY_SLOW, "退避不得超过慢档 {RETRY_SLOW:?}");
            if consecutive > MAX_CONSECUTIVE_FAILURES {
                assert_eq!(delay, RETRY_SLOW, "越过阈值后必须稳定在 30s");
            }
        }

        let r = tick(&mut st, &mut src, &sink).await.unwrap();
        assert_eq!(r.delivered, 1, "越过旧上限的失败之后仍要能恢复投递");
        assert_eq!(*sink.got.lock().unwrap(), vec!["late".to_string()]);
    }

    /// 同一批消息第二轮应全部去重（跨 tick 的状态要保留）。
    #[tokio::test]
    async fn dedupe_persists_across_ticks() {
        let mut st = InboundState::new(10);
        let mut src = FakeSource::new(vec![Some(vec![msg("1")]), Some(vec![msg("1")])]);
        let sink = CountSink::new();
        let r1 = tick(&mut st, &mut src, &sink).await.unwrap();
        let r2 = tick(&mut st, &mut src, &sink).await.unwrap();
        assert_eq!(r1.delivered, 1);
        assert_eq!(r2.delivered, 0);
        assert_eq!(r2.deduped, 1);
    }

    /// 空批次是正常的（没新消息），不该算失败。
    #[tokio::test]
    async fn empty_batch_is_ok_not_error() {
        let mut st = InboundState::new(10);
        let mut src = FakeSource::new(vec![Some(vec![])]);
        let sink = CountSink::new();
        let r: TickReport = tick(&mut st, &mut src, &sink).await.unwrap();
        assert_eq!(r.delivered, 0);
    }
}
