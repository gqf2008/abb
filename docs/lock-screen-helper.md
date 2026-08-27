# 锁屏控制：root 特权助手 abb-helper（#129）

> 状态：**Stage 1 已落地**（本 PR）。Stage 2 待真机验证项见文末。
> 目标：仿 ToDesk——锁屏状态下 agent 能把用户提供的密码按键注入 loginwindow 完成解锁
> （密码只瞬态转发，不存储/不绕过；解锁永远是用户本人输入密码，agent 只转发按键）。

## 一、架构总览

```
┌─ 用户会话（普通权限）────────────────────────────┐
│  agent-bridge（GUI/service）                    │
│    ├─ permreq::request_lock_permissions()       │  ← 辅助功能/输入监控授权（TCC）
│    ├─ lockctl::install/uninstall/status         │  ← 运维 root 助手（osascript 管理员框）
│    └─ lockctl::unlock(password)                 │  ← 解锁指令（config 开关闸 + 瞬态密码）
└───────────────┬─────────────────────────────────┘
                │ unix socket /var/run/com.sqb.abb-helper.sock
                │ （0600 + chown 安装用户 + peer uid/路径/签名校验）
┌───────────────▼─────────────────────────────────┐
│  abb-helper（root launchd daemon）              │
│    ├─ peer 校验（getpeereid / LOCAL_PEERPID /   │
│    │   proc_pidpath / SecCode designated req）  │
│    ├─ status / unlock 命令分发                   │
│    └─ IOHIDEventSystemClient 按键注入            │  ← loginwindow（root 跨会话）
└─────────────────────────────────────────────────┘
```

- 助手独立二进制 `abb-helper`（`[[bin]]`，与主程序同目录打包、独立签名）。
- 安装/卸载走 `osascript ... with administrator privileges`：**显式弹管理员授权框，
  用户不点同意则不装任何东西**（fail-closed）。
- 无任何网络监听端口；只监听本机 unix socket。

## 二、安装与卸载

| 步骤 | 落点 | 说明 |
|------|------|------|
| 二进制 | `/Library/PrivilegedHelperTools/com.sqb.abb-helper` | Apple 特权助手惯例目录 |
| launchd plist | `/Library/LaunchDaemons/com.sqb.abb-helper.plist` | RunAtLoad + KeepAlive；`LOCKCTL_UID`=安装用户 uid（socket chown 用） |
| socket | `/var/run/com.sqb.abb-helper.sock` | 0600 + chown 安装用户 |
| 日志 | `/var/log/com.sqb.abb-helper.log` | 仅 daemon 级错误；**永不出现密码** |

CLI：`agent-bridge --lockctl status|install|uninstall`（macOS）。

## 三、IPC 协议（本机 unix socket）

- 帧格式：4 字节大端长度 + JSON（请求/响应同构）。
- 请求：
  - `{"cmd":"status"}`
  - `{"cmd":"unlock","password":"<瞬态>","timeout_ms":8000}`
- 响应：`{"ok":true,...}` / `{"ok":false,"error":"..."}`。
- 对等方校验（任何一步失败即断开，fail-closed）：
  1. `getpeereid`：peer uid == `LOCKCTL_UID`（安装用户）；
  2. `LOCAL_PEERPID` → `proc_pidpath`：可执行路径为主程序 `agent-bridge`；
  3. `SecCodeCopyGuestWithAttributes`（kSecGuestAttributePid）→ `SecCodeCheckValidity`：
     - 已签名 → 必须过 `identifier "com.sqb.abb"`（bundle id）；
     - 未签名（本地 dev 构建）→ 路径+uid 兜底（文档明示：生产必须签名）。

## 四、解锁语义（fail-closed）

- 前置闸：`config.lock_screen_control` 默认 **false**；关闭时 agent 侧直接拒绝，且助手
  未安装则不运行（“关闭时完全无特权组件运行”）。
- 密码仅存在于调用方内存：发送后立即覆写清零；**不落盘、不进日志、不参与事件溯源、
  不跨会话投递**（`docs/message-flow-eventsourcing.md` 已注明该通道除外）。
- 单次解锁失败/超时即丢弃，**不重试**（防暴力尝试）。
- 任一字符无法映射（非 US 布局/非 ASCII）→ 整次失败，不注入一半。
- 注入序列：逐字符（shift down → key down/up → shift up）+ 末尾 Return，事件间 10-12ms。

## 五、Stage 2 待真机验证 / 后续项

1. **loginwindow 会话定向**：root + `IOHIDEventSystemClientDispatchEvent` 是否直达锁屏会话
   需真机验证（ToDesk 同路线，issue 已确认可）；若系统会话无法注入，需评估
   launchd `LimitLoadToSessionType=LoginWindow` 或后台登录项方案。
2. **键盘布局**：当前按键映射为 US 布局；非 US 布局需按 `TISCopyCurrentKeyboardLayoutInputSource`
   动态映射。
3. **GUI 集成**：设置页「锁屏控制」开关 + 特权组件健康检查展示（`--lockctl status` 已就绪）。
4. **安装引导**：首次启动引导弹辅助功能授权（`--request-permissions` 已含）＋一键安装助手。
5. **SMAppService/SMJobBless 迁移**：当前用 osascript 管理员脚本；生产可迁移到
   `SMAppService.daemon`（macOS 13+）以获得更干净的安装/卸载语义。
6. **打包**：安装器需把 `abb-helper` 随主程序同目录打包并独立签名（`.app/Contents/MacOS/`）。
