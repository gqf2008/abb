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
DefaultDirName={localappdata}\Programs\ABB
DefaultGroupName=ABB
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
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
Source: "ABB.ico"; DestDir: "{app}"; Flags: ignoreversion
; #207 内置 buzz 执行层：与主程序同目录（运行时按 current_exe 同目录解析）；
; buzz 上游 Apache-2.0，再分发附 LICENSE。fake-mcp 是测试桩，不入包。
Source: "..\buzz\target\release\buzz-acp.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\buzz\target\release\buzz-agent.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\buzz\LICENSE"; DestDir: "{app}"; DestName: "buzz-LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\ABB.ico"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\ABB.ico"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent

