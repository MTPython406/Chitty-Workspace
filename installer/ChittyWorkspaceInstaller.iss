; Chitty Workspace Installer Script
; Built with Inno Setup - https://jrsoftware.org/isinfo.php

#define MyAppName "Chitty Workspace"
#define MyAppVersion "0.1.0"
#define MyAppPublisher "DataVisions"
#define MyAppURL "https://datavisions.ai"
#define MyAppExeName "ChittyWorkspace.exe"

[Setup]
; Unique AppId for Chitty Workspace (separate from Chitty Bridge)
AppId={{A1B2C3D4-E5F6-7890-AB12-CD34EF56GH78}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}

; Installation directory
DefaultDirName={autopf}\DataVisions\Chitty Workspace
DefaultGroupName=DataVisions
AllowNoIcons=yes

; Output settings
OutputDir=output
OutputBaseFilename=ChittyWorkspace-Setup-{#MyAppVersion}
SetupIconFile=..\assets\chitty_icon.ico
UninstallDisplayIcon={app}\{#MyAppExeName}

; Compression
Compression=lzma2
SolidCompression=yes

; UI settings
WizardStyle=modern
WizardSizePercent=100

; Privileges - install for current user by default
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog

; Misc
DisableProgramGroupPage=yes

; Version info embedded in installer
VersionInfoVersion={#MyAppVersion}
VersionInfoCompany={#MyAppPublisher}
VersionInfoDescription={#MyAppName} Installer
VersionInfoCopyright=Copyright (C) 2026 {#MyAppPublisher}
VersionInfoProductName={#MyAppName}
VersionInfoProductVersion={#MyAppVersion}

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"
Name: "startupservice"; Description: "Start Chitty Workspace on Windows startup"; GroupDescription: "Startup Options:"; Flags: unchecked

[Files]
; Single release binary
Source: "..\target\release\chitty-workspace.exe"; DestDir: "{app}"; DestName: "{#MyAppExeName}"; Flags: ignoreversion

; Icon file for shortcuts
Source: "..\assets\chitty_icon.ico"; DestDir: "{app}"; Flags: ignoreversion

[Dirs]
; Create data directory
Name: "{%USERPROFILE}\.chitty-workspace"

[Icons]
; Start Menu shortcut
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\chitty_icon.ico"

; Desktop shortcut (optional)
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\chitty_icon.ico"; Tasks: desktopicon

[Registry]
; Add to Windows startup (if task selected)
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "ChittyWorkspace"; ValueData: """{app}\{#MyAppExeName}"""; Flags: uninsdeletevalue; Tasks: startupservice

[Run]
; Option to launch after install
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent
