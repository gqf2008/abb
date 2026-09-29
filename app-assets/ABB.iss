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

[Files]
Source: "..\target\release\agent-bridge.exe"; DestDir: "{app}"; DestName: "{#MyAppExeName}"; Flags: ignoreversion
; B2（批 abb-svc-persist-password-gate）：Windows「降权启动器」必须随包 —— bridge 以高完整性
; 跑时用它把 agent 降到桌面 shell 身份；缺了它 agent 会**起不来**（fail-closed，见 agent_spawn.rs）。
; 文件名必须是 `abb-spawner.exe`（与 `agent_spawn::spawner_exe()` 同源假设），别改名。
Source: "..\target\release\abb-spawner.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "ABB.ico"; DestDir: "{app}"; Flags: ignoreversion
; #200 fork buzz-agent：与主程序同目录（运行时按 current_exe 同目录解析；
; buzz_agent_exe 空时先查同目录）。分叉 Apache-2.0，再分发附 LICENSE。
; fake-mcp 是测试桩，不入包。buzz-acp 已随 #200 进程内化退役，不入包。
Source: "..\crates\buzz-agent\target\release\buzz-agent.exe"; DestDir: "{app}"; Flags: ignoreversion
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
[Run]
Filename: "{app}\{#MyAppExeName}"; Parameters: "--wait-lock"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent runasoriginaluser
Filename: "{app}\{#MyAppExeName}"; Parameters: "--wait-lock"; Flags: nowait; Check: WizardSilent; runasoriginaluser

; ─────────────────────────── 常驻计划任务（per-machine 安装的附带动作）───────────
;
; 安装器此时已是管理员：顺手把**高完整性**的 bridge 常驻任务登记好，装完即生效，
; 用户不需要在应用里再点一次、也不会多一次 UAC（这正是 per-machine 安装换来的好处）。
;
; 任务指向 `{app}`（Program Files，普通用户不可写）里的 exe —— 与"高完整性常驻"配套；
; 卸载时删掉任务，别给系统留孤儿。
[Code]
const
  BridgeTaskName = 'ABB-Bridge';

/// 注册常驻任务（幂等：/f 覆盖）。失败只记日志，不打断安装 —— 应用侧仍能在用户开自启时补建。
///
/// **不在这里拼 XML**：任务 XML 的单一定义在 `src/svc_task.rs::task_xml`（UTF-16LE+BOM 落盘、
/// 转义、逐项设置都有单测）。安装器只是以管理员身份调一次我们自己的隐藏子命令
/// `agent-bridge.exe --install-bridge-task`，由 Rust 侧走同一条注册路径 —— 免得两份 XML 漂移。
procedure RegisterBridgeTask;
var
  Rc: Integer;
begin
  Exec(ExpandConstant('{app}\{#MyAppExeName}'), '--install-bridge-task', '',
       SW_HIDE, ewWaitUntilTerminated, Rc);
  if Rc = 0 then
    Log('ABB: 已登记 bridge 常驻计划任务（HighestAvailable）')
  else
    Log('ABB: 计划任务登记失败 rc=' + IntToStr(Rc) + '（应用侧开自启时会重试）');
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    RegisterBridgeTask;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Rc: Integer;
begin
  if CurUninstallStep = usUninstall then
  begin
    Exec('schtasks.exe', '/delete /tn ' + BridgeTaskName + ' /f', '', SW_HIDE,
         ewWaitUntilTerminated, Rc);
    Log('ABB: 卸载时删除常驻计划任务 rc=' + IntToStr(Rc));
  end;
end;
