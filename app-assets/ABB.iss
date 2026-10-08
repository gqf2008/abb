; ABB (Agent Bridge Bar) - Windows installer script
; Build: ISCC.exe installer\ABB.iss
#define MyAppName "ABB"
// 版本默认值；CI 用 /DMyAppVersion=<Cargo.toml 版本> 传入（#ifndef 让命令行定义生效，
// 避免硬编码漂移——v2.1.0 曾因硬编码 2.0.3 覆盖 /D 导致安装包版本/文件名错误）。
#ifndef MyAppVersion
  #define MyAppVersion "2.0.3"
#endif
#define MyAppPublisher "SQB"
#define MyAppExeName "agent-bridge.exe"

[Setup]
AppId={{0EEFF4CA-5184-4FBB-81D7-EEB910AB0FE7}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
; 2026-09-28（批 abb-svc-persist-password-gate）：改为 **per-machine** 安装。
;
; 为什么必须改：常驻服务以 `RunLevel=HighestAvailable`（高完整性）由计划任务拉起，
; 而高完整性进程要真正"不可被普通用户摆布"，它执行的 exe 必须放在**普通用户不可写**的位置
; —— 否则普通进程在一次 UAC 同意后改写 exe，之后每次登录都会静默以高完整性执行被改过的文件
; （经典的"计划任务 UAC 绕过/持久化"形态）。`{localappdata}` 是用户可写的，故改到 `{autopf}`
; （Program Files），并让安装器需要管理员权限（升级时多一次 UAC，安装过程仍是静默无向导）。
DefaultDirName={autopf}\ABB
DefaultGroupName=ABB
DisableProgramGroupPage=yes
PrivilegesRequired=admin
; 静默升级：自动关闭仍在运行的实例（配合更新器的 /CLOSEAPPLICATIONS），
; 但**不要**让安装器自己重启——重启由本文件末尾的 [Run] 段负责（安装成功后再执行）。
; （ABB **当前未注册** RegisterApplicationRestart，Inno 的 restart 本来也不会生效；
;   这条是防御：若将来注册了它，InstallMode 的自动重启就会与 [Run] 撞成双开。）
CloseApplications=yes
RestartApplications=no
OutputDir=Output
OutputBaseFilename=ABB-Setup-{#MyAppVersion}
SetupIconFile=ABB.ico
UninstallDisplayIcon={app}\ABB.ico
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "chinesesimplified"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[InstallDelete]
; 2026-10-04 去提权收尾：这三个提权件已从产品中删除（Cargo.toml 里 0 个 [[bin]]），但旧版本
; 装过它们 —— 升级时必须删掉，否则永远留在 Program Files 里（owner 2026-10-04 专门问过）。
; abb-helper 是 macOS 专用，旧版 Windows 包也带着它，故一并清。
Type: files; Name: "{app}\abb-spawner.exe"
Type: files; Name: "{app}\abb-elev-helper.exe"
Type: files; Name: "{app}\abb-helper.exe"
; 2026-10-05 审计 #12：per-user 时代（≤2.23.97，`{localappdata}\Programs\ABB`）迁到 per-machine
; 后，旧目录与旧快捷方式都没人清 —— 本机实测残留 162MB，且 `{userprograms}\ABB.lnk` 指向一个
; **已经不存在**的旧 exe（点了没反应）。这里在安装/升级时顺手清掉（不存在时为 no-op）。
Type: filesandordirs; Name: "{localappdata}\Programs\ABB"
Type: files; Name: "{userprograms}\ABB.lnk"

[Files]
Source: "..\target\release\agent-bridge.exe"; DestDir: "{app}"; DestName: "{#MyAppExeName}"; Flags: ignoreversion
Source: "ABB.ico"; DestDir: "{app}"; Flags: ignoreversion
; #200 fork buzz-agent：与主程序同目录（运行时按 current_exe 同目录解析；
; buzz_agent_exe 空时先查同目录）。分叉 Apache-2.0，再分发附 LICENSE。
; fake-mcp 是测试桩，不入包。buzz-acp 已随 #200 进程内化退役，不入包。
Source: "..\crates\buzz-agent\target\release\buzz-agent.exe"; DestDir: "{app}"; Flags: ignoreversion
; abb-agent（新执行层）：owner 会话优先用它；授权者会话仍走 buzz-agent。运行时按
; current_exe 同目录解析（解析链按角色），所以必须与主程序同目录。
Source: "..\crates\abb-agent\target\release\abb-agent.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\crates\buzz-agent\LICENSE"; DestDir: "{app}"; DestName: "buzz-LICENSE.txt"; Flags: ignoreversion
; ABB 本体 MIT：再分发需附副本。
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "ABB-LICENSE.txt"; Flags: ignoreversion
; 随包工具：rg/jq/uv/gh/wassette；git/bun/sed/find 明确不随包。
Source: "..\tools-dist\bin\*"; DestDir: "{app}\tools\bin"; Flags: ignoreversion
Source: "..\tools-dist\licenses\*"; DestDir: "{app}\tools\licenses"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\ABB.ico"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\ABB.ico"; Tasks: desktopicon

; 交互安装：完成页的「运行 ABB」勾选项（skipifsilent：静默安装时这一条不跑）。
; 静默升级：必须**也**拉起新实例 —— 下面第二条用 Check: WizardSilent 只在 /SILENT|/VERYSILENT
; 下生效；2026-09-28 owner 报「升级装完但没自动运行」就是缺这一条（旧行为只靠上面的 postinstall）。
;
; 两条都用 --wait-lock 拉起：安装器关掉旧实例与本进程拿「gui」独占锁之间有极短窗口，
; 没有它新实例会按「已有实例在跑」立刻静默退出（表现为「装完没起来」）。
; 该参数只在被安装器拉起时生效，用户手点图标仍是即退语义。
; runasoriginaluser（2026-09-28）：安装器现在以管理员运行（PrivilegesRequired=admin），
; 若不加这个标志，[Run] 会用**管理员令牌**拉起托盘 —— 托盘再 spawn 的 claude/codex 就会带着
; 管理员权限跑（本批要避免的事）。加它 ⇒ 托盘回到「启动安装的那个普通用户」身份；
; 需要高完整性的 bridge 由下面的 [Code] 注册的计划任务以 HighestAvailable 拉起，各就各位。
; ─────────────────────────── 卸载收工（2026-10-05 审计 #11）───────────────────
;
; 为什么必须有：常驻服务（agent-bridge.exe --service）是**无窗口独立进程**，Windows 的卸载器
; （RestartManager）关不掉它 ⇒ 它锁着 {app}\agent-bridge.exe，卸载删不掉/要求重启系统；
; 而卸载也不会动 HKCU\...\Run 的 ABB 值 ⇒ 下次登录去启动一个**已经不存在**的 exe（托盘永不出现）。
[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/F /T /IM agent-bridge.exe"; Flags: runhidden; RunOnceId: "AbbKillBridge"
Filename: "{sys}\taskkill.exe"; Parameters: "/F /T /IM buzz-agent.exe"; Flags: runhidden; RunOnceId: "AbbKillBuzz"
Filename: "{sys}\taskkill.exe"; Parameters: "/F /T /IM abb-agent.exe"; Flags: runhidden; RunOnceId: "AbbKillNewAgent"
Filename: "{sys}\reg.exe"; Parameters: "delete ""HKCU\Software\Microsoft\Windows\CurrentVersion\Run"" /v ABB /f"; Flags: runhidden; RunOnceId: "AbbDelRunKey"

[Run]
; 顺序要紧：先把安装期间 `/disable` 掉的常驻任务**恢复启用**，再拉起 APP —— 反过来的话，
; 托盘启动那一刻任务还是禁用的，它的看门狗启服务必然失败（2026-10-01 实测顺序就是反的：
; 11:41:51.172 拉起 APP / 11:41:52.657 才 enable ⇒ 装完 APP 与 bridge 都没起来，用户得手动开）。
Filename: "{app}\{#MyAppExeName}"; Parameters: "--wait-lock"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent runasoriginaluser
Filename: "{app}\{#MyAppExeName}"; Parameters: "--wait-lock"; Flags: nowait runasoriginaluser; Check: WizardSilent

; ─────────────────────────── 常驻计划任务（per-machine 安装的附带动作）───────────
;
; 安装器此时已是管理员：顺手把**高完整性**的 bridge 常驻任务登记好，装完即生效，
; 用户不需要在应用里再点一次、也不会多一次 UAC（这正是 per-machine 安装换来的好处）。
;
; 任务指向 `{app}`（Program Files，普通用户不可写）里的 exe —— 与"高完整性常驻"配套；
; 卸载时删掉任务，别给系统留孤儿。
[Code]

/// 安装/升级前**强制收工**：把 bridge 的常驻服务与其子孙进程真的停掉。
///
/// 为什么必须由安装器动手（2026-10-01 owner 实报「安装程序杀不掉进程，必须手动杀才能装」）：
/// `CloseApplications=yes` 走的是 Windows RestartManager，而 RM 只能关**有窗口的 GUI 进程**；
/// 常驻服务 `agent-bridge.exe`（计划任务以 HighestAvailable 拉起，无窗口）它关不掉，于是静默
/// 安装（`/SUPPRESSMSGBOXES`）下那个 Abort/Retry/Ignore 默认取 **Abort** ⇒ 回滚重来。
/// 实测日志（%TEMP%\Setup Log *.txt）：`Some applications could not be shut down.` →
/// `Defaulting to Abort for suppressed message box` → `User canceled the installation process.`。
///
/// 新模型（2026-10-04）下没有计划任务要停/禁用：ABB 全部以普通用户运行，服务由托盘看守。
/// 于是只剩「杀进程 + 等句柄真正释放」两步（`Sleep` 不可省：kill 返回 ≠ 句柄已关）。
procedure StopBridgeForInstall;
var
  Rc: Integer;
begin
  // /T 连子孙一起收；两次都是幂等的（进程不在也只是 rc<>0）
  Exec('taskkill.exe', '/F /T /IM agent-bridge.exe', '', SW_HIDE, ewWaitUntilTerminated, Rc);
  Log('ABB: 结束 agent-bridge.exe rc=' + IntToStr(Rc));
  Exec('taskkill.exe', '/F /T /IM buzz-agent.exe', '', SW_HIDE, ewWaitUntilTerminated, Rc);
  // abb-agent（新执行层）也要收：它可能正跑着工具/shell 子进程。
  Exec('taskkill.exe', '/F /T /IM abb-agent.exe', '', SW_HIDE, ewWaitUntilTerminated, Rc);
  Sleep(1200);
end;

/// Inno 事件：**复制文件之前**（RestartManager 那步之前）先把 ABB 收干净。
function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopBridgeForInstall;
  Result := '';
end;

/// 2026-10-04 新模型：**不再注册任何常驻计划任务**（服务由托盘以普通用户身份拉起并看守，
/// 自启 = 用户 Run 键）。安装器因此只剩「收工」这一件事。
procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then
    // 第二道（幂等）：万一 PrepareToInstall 之后又有进程被拉起来（看门狗/用户点了启动）
    StopBridgeForInstall;
end;

// 卸载不再需要删任何常驻任务（2026-10-04 新模型已无计划任务）。
