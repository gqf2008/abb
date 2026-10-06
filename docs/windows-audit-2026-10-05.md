# Windows 深度排查（2026-10-05）

触发：owner「windows 上的问题太多了，深度排查一遍，不希望后面再出现这种很低级的问题了」。
方法：4 路只读审计（进程生命周期 / 安装升级 / 路径编码日志 / 网络与文件锁）+ 实机证据（2.23.104 安装、logs 目录、Setup Log、注册表、进程快照）。

状态：已修 / 待修 / 已核对无问题。每条给出位置、证据、修法与守卫配方。

## 已修

1. 升级后固定 90 秒空窗（安装器 [Run] 在静默安装下从未成功拉起 APP，唯一救活的是兜底脚本里 ping 死等 90 秒）
   证据：update.log 2.23.100 到 104 五连「安装后 90 秒仍无 agent-bridge 进程，兜底拉起」
   修法：改成每 2 秒轮询；APP 已在跑则退出；安装器进程已退出且无 APP 则立刻拉起；硬上界 150 次
2. 兜底看护不读用户意图（用户点过停止也会被每 5 秒复活）
   修法：兜底先判 install::is_desired()，意图为停则不接管
3. 兜底拉 service 用 Stdio::null（Windows 上 NUL 是有效句柄，attach_log_file 判为可用直接 return，日志全蒸发）
   修法：stdout/stderr 接到 logs/bridge.out
4. 启动前的「停」失败挡住「启动」（taskkill 报进程不存在被当成错误，服务永远起不来）
   修法：taskkill 报「进程不存在」视为已停止；停失败只记日志继续启动
5. --wait-lock 拿不到 gui 锁就 exit(0)（升级后既无托盘也无服务）
   修法：失败臂接管 service 看护（无 GUI 兜底，10 分钟有界）
6. 手写打包脚本会用错产物（给 fork 构建带 CARGO_TARGET_DIR，.iss 取到上一版 buzz-agent.exe）
   修法：唯一入口 tools/win-build-installer.ps1，各自默认 target 目录 + 新鲜度断言
7. status() 只按 pid 判活、不校验身份（PID 复用被当 service：假运行 + 误杀无辜进程）
   证据：gui.out 23:43 记 running=true pid=12012，taskkill 同一 pid 报 not found，此后 30 分钟挡住启动
   修法：status() = 存活 且 镜像名为 agent-bridge.exe（复用 orphan_mcp::image_path）；配判别力测试 + 接线守卫
8. 发版资产名无断言（产物名错位时客户端把旧版当新版装，形成降级循环）
   修法：CI 断言 app-assets/Output/ABB-Setup-<版本>.exe 必须存在

## 待修（配方就绪，按用户可见后果 x 复发概率排序）

9. 单实例 share_mode(0)：第三方（杀软/索引/备份）短暂打开过锁文件就报「已有实例」，托盘静默不启动
   位置：src/single_instance.rs:125-137
   修法：只把 ERROR_SHARING_VIOLATION(32) 当已有实例，其它错误重试上报；更彻底用 CreateMutexW
10. Run 键只判值存在、不判指向哪个 exe（per-user 迁 per-machine 后旧 LOCALAPPDATA 值被判已开，登录什么都不发生）
   位置：src/platform.rs:1324-1332 / 1386-1391
   修法：新增 parse_run_value(reg query 输出) 纯函数，按值 == current_exe 判定，不符即 Drifted 重写
11. 卸载链完全不收工（无 [UninstallRun]；服务无窗口删不掉、卸载后进程还在跑、HKCU Run 键残留）
   位置：app-assets/ABB.iss:102-142
   修法：卸载钩子 taskkill 两个 exe + reg delete HKCU Run 键；守卫断言这三条
12. per-user 迁 per-machine 无清理（两个卸载项、LOCALAPPDATA 残留 162MB、开始菜单快捷方式指向已不存在的旧 exe）
   位置：ABB.iss:24 / 51-57
   修法：迁移代码清理旧目录 + 其 _is1 卸载键 + 用户开始菜单链接
13. ✅ 已修（main e416242）attach_log_file 认不出 NUL 句柄（判据侧同 3）
   修法：把「句柄可用否」抽纯函数，NUL 句柄判不可用
14. 兜底脚本按镜像名判托盘（服务就是同名 agent-bridge.exe --service，服务活着时永不拉起托盘；最坏结果是没有托盘图标，频道仍由服务活着）
   修法：按命令行是否带 --service 区分（现成 PowerShell 一行）
15. 控制台输出一律按 UTF-8 解码（非 65001 机器注册表 PATH 里的中文目录被解成 U+FFFD，工具判未安装）
   位置：src/deps.rs:202-234
   修法：抽 parse_reg_path(&[u8])，喂 GBK 字节断言条目保留
16. ✅ 已修（main 29c6ef3 代码 + 03b96da 守卫）bridge.out / gui.out 永不轮转（实机 17.5MB / 78275 行，同一文件半个月）
   （2026-10-05 两次尝试的坑：helper 必须放 crate 根；且本仓 main.rs 的日志守卫测试在**嵌套** mod 里，use super::* 看不到根级项，要写 crate::xxx。别再把插入点选在别处 fn 的 #[test] 前——会把它的属性吞掉。）
   修法：仿 task_store.rs:237-259 的轮转单测，与任务日志共用上限常量
17. ✅ 已修（main 6cb3446）软链失败被 let _ = 吞掉还谎报「已补链 N 个」（普通用户 + 未开开发者模式则技能永久缺失）
   位置：src/larkskills.rs:46-53
   修法：注入「建链必失败」的 linker 断言成功计数为 0，或改用无需特权的 junction
18. ✅ 已修（main d92dcb5 第一批 + f1bab12 收尾） 钉钉 Stream 的 sink.send 无超时（半开连接冻死 select，180s 看门狗永不触发）
   位置：src/dingtalk.rs:1052/1058/1099/1114/1130/1141
   修法：参照 src/ws.rs:93-104 包 tokio::time::timeout；源码守卫断言每个 sink.send 外层有超时
19. ✅ 已修（main 169b725）微信游标不持久化（每次重启从空游标开始，升级窗口内消息大概率丢）
   位置：src/service.rs:1259 / 1271 / 1280
   修法：游标落盘 workspaces/<bot>/wx_cursor.json，mock 断言重启后首个请求体游标等于上次保存值
20. ✅ 已修（main c4c2a42）微信把 2xx/5xx + 空 body 当成功空轮询（consec_timeouts 被复位、假绿不自愈）
   位置：src/wechat.rs:553-555
   修法：mock 返回空体，断言 get_updates 返回 Err 而非 Ok
21. ✅ 已修（main a0e8303）微信 outbox 两个写方各持内存快照（Router::fail_text 新建 OutboxStore 与 Bridge.outbox 互相整文件覆盖，积压项静默丢）
   位置：src/outbox.rs:90-135 / src/deliver.rs:571-574 / src/bridge/mod.rs:439
   修法：单测两个同路径 store 各 add 一条，reload 断言两条都在；或源码守卫禁止 deliver.rs 再 new OutboxStore
22. ✅ 已修（main b4c6415）config.json 固定 tmp 名 + 只有进程内锁（GUI 与 service 并发写互踩，设置/授权静默回退）
   位置：src/config.rs:1710-1718 / 1783
   修法：源码守卫禁止 config.rs 出现 json.tmp，必须走 atomic_write_text/atomic_write_sensitive
23. ✅ 已修（main f06e88c）agent 回复发送失败仍无条件摘掉 pending（断网瞬间的回复永久丢；飞书/钉钉没有 outbox）
   位置：src/bridge/virtualbot.rs:1096-1102
   修法：失败时保留 pending（或转 outbox），下一轮/重启补发
24. ✅ 已修（main 87d13fb）svc_start_verified 验证的是自己刚写的 pid（子进程随后才因抢锁 exit(0) 或 config exit(1)，点启动弹成功而服务已死）
   位置：src/install.rs:207 / 220 / 384
   修法：注入「立即 exit(1)」的假 service 断言返回 Err；或要求跨过稳定窗口后仍存活才算 up
25. ✅ 已修（main 2c30d47 停止只清自己停掉的 pid + 00af32d 两处写入改原子写） pid 文件写者不唯一且非原子 + svc_stop_impl 无条件删 pid 与并发 start 竞态（pid 指向死进程 / 删掉新实例的 pid）
   位置：src/install.rs:208 / 271 / 361
   修法：pid 只允许 service 自写 + 原子 tmp+rename；读写纳入同一把锁

## 已核对无问题

- deliver.rs:137-161：CLI 与 service 的 flock / LockFileEx 真排他，唯一 tmp + rename 正确。
- pending / botstatus / unread 的原子写与「service 单一写方」自洽。
- msgstore WAL、mcp_events::with_lock 未见可证实的 Windows 缺陷。
- agent 子进程 env_clear 白名单缺 PATHEXT / SystemRoot 不影响 cmd 解析（实测 exit=0）。
- 卸载不会误删 ~/.agent-bridge（无 [UninstallDelete]，git log -S 零命中）。

## 旁证待查（2026-10-05 已全部落地）

> 除「升级端到端实跑」需要 owner 拍板（会中断四个 bot 约 10~30 秒）外，本清单所有条目均已修复并配可判别守卫。


26. ✅ 已修（main 929f206 启动即告警 + 058fec7 安全降尊重启：Explorer 降权 + 握手确认才退出，未确认继续运行）——提权传染链（2026-10-05 实测）

    **安全降尊重启的设计（2026-10-05 定稿，实施时必须照此，绝不能让用户「什么都没有」）**：
    1. 高完整性实例启动时，用 `explorer.exe "<exe>" --de-elevated-handoff` 拉起**普通身份**实例
       （Explorer 的令牌是登录用户 ⇒ 天然降权；绝不用 `runas`/提权外壳）；
    2. 新实例拿到 gui 锁后**写一个握手标记**（例如 `logs/deelev-handoff`，内容含 pid）；
    3. 旧（提权）实例**轮询该标记最长 ~10s**：见到自己那份的确认 ⇒ 才 `exit(0)`；
    4. 超时未见确认 ⇒ **继续运行**（保持现状）并记一条响亮日志 —— **宁可不降权，也绝不留下无人看守的后台**；
    5. 守卫：握手判据抽纯函数（标记内容/超时）单测 + 源码守卫断言「未确认前不得退出」。
    另：service 由托盘看守 ⇒ 托盘降权后，服务与 agent 自然跟着普通身份（无需单独处理）。
：应用自己的日志每 5 分钟一条
    `[spawn] 本进程正以管理员权限运行：agent「…\buzz-agent.exe」会继承该权限`；而同日诊断显示
    诊断 shell `elevated=False`、**无任何 ABB 计划任务**、Run 键指向 `C:\Program Files\ABB\agent-bridge.exe`（普通）
    ⇒ 最可能是**某一次托盘被从提权上下文启动**（早期安装器 `[Run]` 或人工/工具启动），此后服务与 agent 全部
    沿父进程继承管理员令牌。
    位置：`src/platform.rs` 的 `is_elevated`（判定侧）+ 托盘/service 启动链（继承侧）。
    修法（结构性）：启动时若发现自己以**高完整性**运行，**主动降尊重启**（用 `explorer.exe` 或
    计划任务 `RunLevel=Limited` 拉起普通实例）并退出当前实例；至少也要在托盘显著提示 + 日志写明
    「当前为管理员实例，agent 会继承管理员」——绝不允许「悄悄以管理员跑」。
    守卫：单测 `is_elevated` 的判据（注入高/普通令牌）；源码守卫断言启动路径里有降尊重启或显著提示。


- 当前托盘/service 以管理员身份运行（bridge.out 00:36:33 记「服务正以管理员权限运行」，UAC EnableLUA=1），agent 继承管理员，与「全部普通用户运行」模型相悖，需单列排查提权传染链。

## 2026-10-06 新增发现（实测）

27. ❌ **安装器 `[Run]` 拉起的实例连 `main()` 都到不了**（三次复现：13:56 死得无声、15:20 起来但卡死且零日志、
    22:42 完全无记录）。`boot.log`（本轮新加的启动取证）在 22:42:23 那次**没有任何行** ⇒ 进程根本没跑到 Rust 代码。
    兜底：`ABB-EnsureRunning` 每 5 分钟自愈（已装 ✓ 且已在 2.23.111 写进代码）。待办：替换 `[Run]`（属升级路径，单独一轮）。
28. ❌ **看护类任务在 Agent 侧疯狂调 LLM 烧满预算**（实测 03:15~03:19 每 1~2 秒一次模型调用，用满 200 回合上限）。
    已修：① 该看护任务改成**纯脚本 + `agent-bridge deliver`**（零 LLM，秒级 ✓）；② 登记期拒绝「预算 ≥ 周期」
    （2.23.112 ✓）；③ 回合上限按任务预算推导（纯函数，已落地，接线待做）。
29. ⚠ **Windows 缺 proc runner**：`task_proc.rs` 全 `#[cfg(unix)]`，Windows 只有空壳 + 两处显式拒绝（Q15）。
    第一步 `ProcGroup` 抽象（Windows `taskkill /T /F`、Unix `kill -9 -<pgid>` + 守卫）已落地；runner 与开闸待做。
    决策：Jev `bool` 判「现在硬上风险大于收益」p=0.82 ⇒ 推迟到专门一轮。
