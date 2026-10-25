; Talkee for Windows. Built by the release workflow with TALKEE_VERSION set; see .github/workflows.
;
; Per-user on purpose: no administrator prompt to install, and an update can run silently without one.
; The one thing that does need administrator rights, installing the VB-CABLE virtual microphone, is
; done by the app itself when the user asks for it, with its own explanation.

#define AppVersion GetEnv("TALKEE_VERSION")
#if AppVersion == ""
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{41ABF9E6-B56A-4A46-9DD1-50C6F2D3B477}
AppName=Talkee
AppVersion={#AppVersion}
AppPublisher=Talkee
DefaultDirName={autopf}\Talkee
DefaultGroupName=Talkee
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=Output
OutputBaseFilename=Talkee-Setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayName=Talkee
; An update replaces the running program, so close it and start it again afterwards.
CloseApplications=yes
RestartApplications=yes

[Tasks]
Name: "startup"; Description: "Start Talkee when I sign in"; GroupDescription: "Options:"

[Files]
Source: "..\target\release\talkee-app.exe"; DestDir: "{app}"; DestName: "Talkee.exe"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\Talkee"; Filename: "{app}\Talkee.exe"

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "Talkee"; ValueData: """{app}\Talkee.exe"""; Flags: uninsdeletevalue; Tasks: startup

[Run]
; Interactive install: offer to open it.
Filename: "{app}\Talkee.exe"; Description: "Open Talkee"; Flags: nowait postinstall skipifsilent
; Silent install, which is what an update is: open it again without asking.
Filename: "{app}\Talkee.exe"; Flags: nowait skipifnotsilent
