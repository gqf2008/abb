# ABB — Agent Bridge Bar

把 **飞书 / 微信 / 钉钉** 消息桥接到**本地 Claude / Codex** 的菜单栏应用（macOS）+ 无头守护进程（Windows / macOS 双平台）。

单二进制双模式：

- `agent-bridge` —— 托盘控制器（Slint GUI），同时是 service 的看门狗
- `agent-bridge --service` —— 无头桥守护进程（纯 tokio）

## 特性

- **多通道**：飞书（官方长连接 WS）、微信（长轮询）、钉钉（Stream 模式），多 bot 并行
- **后端随 bot 走**：每个 bot 独立选择 `claude` / `codex`，定时任务与聊天用同一后端，不再割裂
- **模型供应商集中配置**：Anthropic 原生 / OpenAI 兼容（chat / responses），支持 per-bot 覆盖；未配置时 claude 回落 CC Switch、codex 走自认证
- **会话记忆**：每个 bot 按后端独立维护会话，支持流式回复、`停止` 打断
- **定时任务**：聊天里用自然语言创建（agent 自动调 `agent-bridge job` CLI），中文 cron，任务跟随 bot 后端
- **工作区隔离**：每个 bot 独立目录 `~/.agent-bridge/workspaces/<bot>/`，agent 只在本工作区读写
- **看门 + 自启**：GUI 拉起/重启 service，崩溃自动重拉（跨平台，不依赖 launchctl）
- **系统权限**：macOS 权限检测（完全磁盘访问 / 辅助功能 / 屏幕录制等）+ 一键请求
- **分发**：macOS Developer ID 签名 + 公证；Windows Inno Setup 安装包

## 下载安装

| 平台 | 安装包 | 说明 |
|---|---|---|
| macOS | `ABB-<版本>-notarized.zip`（Release 资产） | Apple Silicon（aarch64），解压后拖入「应用程序」 |
| Windows | `ABB-Setup-<版本>.exe`（Release 资产） | Inno Setup 安装包 |

macOS 首次启动后，在「系统设置 → 隐私与安全性」按需授权（完全磁盘访问、辅助功能、自动化等）；托盘菜单里也有「请求权限」按钮。

## 快速开始

1. 配置 `~/.agent-bridge/config.json`（0600；也可用托盘「设置」窗编辑，保存后 service 热加载）
2. 启动托盘应用，看门会自动拉起 `--service` 守护进程
3. 在 bot 私聊里发消息即可；定时/周期需求直接说「每天 9 点提醒我…」

示例配置：

```json
{
  "owner_open_id": "ou_xxx（飞书 owner）",
  "default_backend": "claude",
  "default_provider": "anthropic-main",
  "providers": [
    { "name": "anthropic-main", "kind": "anthropic", "base_url": "https://api.anthropic.com", "api_key": "sk-ant-…", "model": "" },
    { "name": "deepseek", "kind": "openai-chat", "base_url": "https://api.deepseek.com", "api_key": "sk-…", "model": "deepseek-v4-flash" }
  ],
  "bots": [
    {
      "name": "庆小丰",
      "kind": "feishu",
      "app_id": "cli_xxx",
      "app_secret": "…",
      "owner_open_id": "ou_xxx",
      "backend": "claude",
      "provider": "anthropic-main"
    },
    {
      "name": "微信龙虾",
      "kind": "wechat",
      "wx_token": "…（扫码登录获得）",
      "wx_user_id": "…（owner 微信标识）",
      "backend": "codex",
      "provider": "deepseek"
    },
    {
      "name": "钉钉助手",
      "kind": "dingtalk",
      "app_id": "AppKey",
      "app_secret": "AppSecret",
      "ding_user_id": "…（允许响应的 staffId，空=响应所有人）",
      "backend": "codex"
    }
  ]
}
```

### 主要字段

| 字段 | 说明 |
|---|---|
| `default_backend` | 全局默认后端：`claude` \| `codex` |
| `bots[].backend` | per-bot 后端，空 = 跟随全局默认 |
| `bots[].kind` | `feishu`（默认）\| `wechat` \| `dingtalk` |
| `bots[].name` | 隔离名（决定工作区目录），空则用 app_id 尾 6 位 |
| `bots[].provider` | 模型供应商名，空 = 跟随 `default_provider` |
| `providers[].kind` | `anthropic` \| `openai-chat` \| `openai-responses` |

微信 bot 的 `wx_token` 通过应用内扫码登录获得（等价凭证，勿外泄）。

## 定时任务

聊天里告诉 agent「每天 9 点半提醒我站会」「每分钟查一次」即可；也可以手动用 CLI：

```bash
agent-bridge job list
agent-bridge job add --cron "30 9 * * *" --prompt "提醒我开站会" [--note "原句"]
agent-bridge job add --once "2026-08-08 10:00" --prompt "发周报"
agent-bridge job del <id前缀>
```

- cron 为 5 段中文 cron：`分 时 日 月 周`（支持 `*`、`,`、`-`、`/`）
- 定时任务执行时**跟随 bot 生效后端**（与聊天一致）
- 任务结果回发到创建时的会话；会话失效自动回落 bot 主会话

## 开发

```bash
cargo build            # 调试构建
cargo run              # 托盘应用
cargo run -- --service # 无头守护进程
cargo test             # 全部单元测试
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

模块结构：`agent`（调 claude/codex）、`bridge`（消息路由/会话/打断）、`wechat` / `feishu` / `dingtalk`（通道）、`schedule`（定时任务）、`sessions`（会话存储）、`service`（守护入口）、`ui/app.slint`（Slint UI）。

运行时数据都在 `~/.agent-bridge/`；`scripts/` 是本地开发脚本，不入库。

## 构建分发

```bash
./scripts/build.sh               # 本机自用：release 编译 + agent-bridge-dev 签名
./scripts/build.sh --notarize    # 对外分发：Developer ID 签名 + 公证 + 装订 + 打 zip 到 ~/Downloads
```

发布到 GitHub：`gh release create <tag> <macOS zip> <Windows exe> --title … --notes …`

## 安全说明

- `config.json` 含 App Secret / API Key，权限 0600，**已 gitignore，严禁提交**
- `*.secret`、`logs/`、`target/` 均不入库
- agent 工作区限定在 `~/.agent-bridge/workspaces/<bot>/`，不接触仓库外文件
