; Meteor Browser installer (Inno Setup 6).
;
; Mirrors the browser's own "Install" menu action (src/shell.rs): the exe goes to
; %LOCALAPPDATA%\Programs\MBrowser, the browser is registered for the current user
; only (HKCU, no admin rights), shortcuts are named MBrowser.lnk. Keeping the same
; folder, registry keys and shortcut names means the in-app install/uninstall and
; this installer never create duplicates. User data (%APPDATA%\MBrowser and the
; WebView2 profile) is never touched by the uninstaller.
;
; Build: iscc /DAppVersion=0.1.0 installer\mbrowser.iss  ->  dist\MBrowser-Setup.exe
; (CI does this in .github/workflows/release.yml.)

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceExe
  #define SourceExe "..\target\release\mbrowser.exe"
#endif

#define AppName "Meteor Browser"
#define RegName "MBrowser"
#define ExeName "mbrowser.exe"
#define RepoUrl "https://github.com/mboxlab/MBrowser"
; Keys written by register() in src/shell.rs.
#define ClientKey "Software\Clients\StartMenuInternet\" + RegName
#define CapsKey ClientKey + "\Capabilities"

[Setup]
AppId={{17401014-DB4C-45D9-923E-64F2D04FAA62}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=mboxlab
AppPublisherURL={#RepoUrl}
AppSupportURL={#RepoUrl}/issues
AppUpdatesURL={#RepoUrl}/releases
; Same folder the browser installs itself into; not changeable on purpose.
DefaultDirName={localappdata}\Programs\{#RegName}
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir=..\dist
OutputBaseFilename=MBrowser-Setup
SetupIconFile=..\assets\mbrowser.ico
UninstallDisplayIcon={app}\{#ExeName}
UninstallDisplayName={#AppName}
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
; Restart Manager closes a running browser before its exe is replaced or removed.
CloseApplications=yes
RestartApplications=no
ChangesAssociations=yes
ShowLanguageDialog=no

[Languages]
Name: "russian"; MessagesFile: "compiler:Languages\Russian.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[CustomMessages]
russian.AppDescription=Быстрый лёгкий браузер на движке WebView2
english.AppDescription=Fast, lightweight browser built on WebView2
russian.MakeDefault=Сделать браузером по умолчанию (откроются настройки Windows)
english.MakeDefault=Make it the default browser (opens Windows Settings)
russian.NoWebView2=Для работы браузера нужен Microsoft Edge WebView2 Runtime, но он не найден.%n%nОткрыть страницу загрузки?
english.NoWebView2=Meteor Browser needs the Microsoft Edge WebView2 Runtime, which was not found.%n%nOpen the download page?

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "{#ExeName}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\{#RegName}"; Filename: "{app}\{#ExeName}"; WorkingDir: "{app}"; Comment: "{#AppName}"
Name: "{userdesktop}\{#RegName}"; Filename: "{app}\{#ExeName}"; WorkingDir: "{app}"; Comment: "{#AppName}"; Tasks: desktopicon

[Registry]
; Browser registration (Default apps). Same values as register() in src/shell.rs.
Root: HKCU; Subkey: "{#ClientKey}"; ValueType: string; ValueName: ""; ValueData: "{#AppName}"; Flags: uninsdeletekey
Root: HKCU; Subkey: "{#ClientKey}\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: """{app}\{#ExeName}"",0"
Root: HKCU; Subkey: "{#ClientKey}\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#ExeName}"""
Root: HKCU; Subkey: "{#CapsKey}"; ValueType: string; ValueName: "ApplicationName"; ValueData: "{#AppName}"
Root: HKCU; Subkey: "{#CapsKey}"; ValueType: string; ValueName: "ApplicationDescription"; ValueData: "{cm:AppDescription}"
Root: HKCU; Subkey: "{#CapsKey}"; ValueType: string; ValueName: "ApplicationIcon"; ValueData: "{app}\{#ExeName},0"
Root: HKCU; Subkey: "{#CapsKey}\Startmenu"; ValueType: string; ValueName: "StartMenuInternet"; ValueData: "{#RegName}"
Root: HKCU; Subkey: "{#CapsKey}\URLAssociations"; ValueType: string; ValueName: "http"; ValueData: "MBrowserURL"
Root: HKCU; Subkey: "{#CapsKey}\URLAssociations"; ValueType: string; ValueName: "https"; ValueData: "MBrowserURL"
Root: HKCU; Subkey: "{#CapsKey}\FileAssociations"; ValueType: string; ValueName: ".htm"; ValueData: "MBrowserHTML"
Root: HKCU; Subkey: "{#CapsKey}\FileAssociations"; ValueType: string; ValueName: ".html"; ValueData: "MBrowserHTML"
Root: HKCU; Subkey: "{#CapsKey}\FileAssociations"; ValueType: string; ValueName: ".xhtml"; ValueData: "MBrowserHTML"
Root: HKCU; Subkey: "{#CapsKey}\FileAssociations"; ValueType: string; ValueName: ".pdf"; ValueData: "MBrowserPDF"
Root: HKCU; Subkey: "Software\RegisteredApplications"; ValueType: string; ValueName: "{#RegName}"; ValueData: "{#CapsKey}"; Flags: uninsdeletevalue
; ProgIDs the associations point to.
Root: HKCU; Subkey: "Software\Classes\MBrowserURL"; ValueType: string; ValueName: ""; ValueData: "Meteor Browser URL"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\MBrowserURL"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""
Root: HKCU; Subkey: "Software\Classes\MBrowserURL\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: """{app}\{#ExeName}"",0"
Root: HKCU; Subkey: "Software\Classes\MBrowserURL\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#ExeName}"" ""%1"""
Root: HKCU; Subkey: "Software\Classes\MBrowserHTML"; ValueType: string; ValueName: ""; ValueData: "Meteor Browser HTML Document"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\MBrowserHTML\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: """{app}\{#ExeName}"",0"
Root: HKCU; Subkey: "Software\Classes\MBrowserHTML\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#ExeName}"" ""%1"""
Root: HKCU; Subkey: "Software\Classes\MBrowserPDF"; ValueType: string; ValueName: ""; ValueData: "Meteor Browser PDF Document"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\MBrowserPDF\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: """{app}\{#ExeName}"",0"
Root: HKCU; Subkey: "Software\Classes\MBrowserPDF\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#ExeName}"" ""%1"""

[Run]
Filename: "{app}\{#ExeName}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent
Filename: "ms-settings:defaultapps?registeredAppUser={#RegName}"; Description: "{cm:MakeDefault}"; Flags: shellexec nowait postinstall skipifsilent unchecked

[UninstallDelete]
; Leftovers of the browser's own in-place exe replacement (install_exe in src/shell.rs).
Type: files; Name: "{app}\{#ExeName}.new"
Type: files; Name: "{app}\*.old"

[Code]
const
  WebView2Guid = '{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}';

function HasWebView2Version(Root: Integer; Key: String): Boolean;
var
  V: String;
begin
  Result := RegQueryStringValue(Root, Key, 'pv', V) and (V <> '') and (V <> '0.0.0.0');
end;

// Microsoft's documented detection keys for the Evergreen WebView2 Runtime.
function WebView2Installed: Boolean;
begin
  Result :=
    HasWebView2Version(HKLM, 'SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\' + WebView2Guid) or
    HasWebView2Version(HKCU, 'Software\Microsoft\EdgeUpdate\Clients\' + WebView2Guid);
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Code: Integer;
begin
  if (CurStep = ssPostInstall) and not WebView2Installed then
    if SuppressibleMsgBox(CustomMessage('NoWebView2'), mbConfirmation, MB_YESNO, IDNO) = IDYES then
      ShellExec('open', 'https://developer.microsoft.com/microsoft-edge/webview2/', '', '', SW_SHOWNORMAL, ewNoWait, Code);
end;
