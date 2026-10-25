; TMR's installer (TODO 104), for Inno Setup 6. release.yml builds it on each tag, from the files it stages:
;
;   iscc /DAppVersion=0.1.0 /DStage=<folder> dist\tmr.iss
;
; <folder> holds what it installs: tmr.exe, docs\ and configs\. For all users by default, in Program Files (it asks UAC); "install for
; me only" on its first page or /CURRENTUSER, in %LOCALAPPDATA%\Programs, with no UAC. winget passes /ALLUSERS or /CURRENTUSER by the
; scope it picks.
;
; TMR keeps its files beside tmr.exe (src/home.rs): configs\, results\, logs\ and tmr-cfg.json. Users can't write in Program Files, and
; TMR runs its tests unelevated, so for all users those are made writable by users: never the exe's folder or the exe, since anyone able
; to replace tmr.exe could have an admin run their program. An upgrade never overwrites a config, and an uninstall leaves what TMR and
; the user made.

#ifndef AppVersion
  #error Pass the version: iscc /DAppVersion=0.1.0 /DStage=<folder> dist\tmr.iss
#endif
#ifndef Stage
  #error Pass the folder of staged files: iscc /DAppVersion=0.1.0 /DStage=<folder> dist\tmr.iss
#endif
; The file version Windows shows takes numbers only: 0.1.0 of 0.1.0-rc.1
#define NumericVersion Copy(AppVersion, 1, Pos("-", AppVersion + "-") - 1)

[Setup]
; The same for every release: how Windows knows a new one is an upgrade of the one installed
AppId={{A1BC7167-67F4-461D-BE00-B57FBAF0B53C}
AppName=TMR (Test Memory R)
AppVersion={#AppVersion}
AppVerName=TMR (Test Memory R) {#AppVersion}
AppPublisher=Jarrod Petz
AppPublisherURL=https://github.com/jlpetz/Test-Memory-R
AppSupportURL=https://github.com/jlpetz/Test-Memory-R/issues
AppUpdatesURL=https://github.com/jlpetz/Test-Memory-R/releases
DefaultDirName={autopf}\TMR
DisableProgramGroupPage=yes
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=dialog commandline
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; The console runs from Server 2022 (build 20348); the window needs build 22000 (crates/tmr-app/build.rs)
MinVersion=10.0.20348
ChangesEnvironment=yes
UninstallDisplayIcon={app}\tmr.exe
UninstallDisplayName=TMR (Test Memory R) {#AppVersion}
OutputBaseFilename=tmr-{#AppVersion}-setup
SetupIconFile=..\crates\tmr-app\tmr.ico
VersionInfoVersion={#NumericVersion}
VersionInfoProductName=TMR (Test Memory R)
VersionInfoDescription=TMR (Test Memory R) setup
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "addtopath"; Description: "Put tmr on the PATH, to run it from a console"
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Dirs]
Name: "{app}\configs"; Permissions: users-modify; Flags: uninsneveruninstall
Name: "{app}\results"; Permissions: users-modify; Flags: uninsneveruninstall
Name: "{app}\logs"; Permissions: users-modify; Flags: uninsneveruninstall

[Files]
Source: "{#Stage}\tmr.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\docs\*"; DestDir: "{app}\docs"; Flags: ignoreversion recursesubdirs
Source: "{#Stage}\configs\*"; DestDir: "{app}\configs"; Flags: onlyifdoesntexist uninsneveruninstall
; The calibrations save into it, beside the exe: an empty one, writable by users, which TMR reads as no settings
Source: "tmr-cfg.json"; DestDir: "{app}"; Flags: onlyifdoesntexist uninsneveruninstall; Permissions: users-modify

[Icons]
; A double-click opens TMR's window (crates/tmr-app/src/launch.rs)
Name: "{autoprograms}\TMR"; Filename: "{app}\tmr.exe"; WorkingDir: "{app}"; Comment: "Test memory with TMR"
; The guide as GitHub shows it, for this version (a .md file may have no program to open it); docs\ has the file too
Name: "{autoprograms}\TMR guide"; Filename: "https://github.com/jlpetz/Test-Memory-R/blob/v{#AppVersion}/dist/docs/guide.md"
Name: "{autodesktop}\TMR"; Filename: "{app}\tmr.exe"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\tmr.exe"; Description: "Open TMR"; Flags: nowait postinstall skipifsilent unchecked runasoriginaluser

[Messages]
FinishedLabel=TMR is installed. It keeps its configs, results and logs in its folder, beside tmr.exe.%n%nIts window needs the Windows App Runtime 2.4 or later, which TMR offers to download when it is missing; its console runs without it.

[Code]
const
  SystemEnvironment = 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment';
  UserEnvironment = 'Environment';
  VCRedistKey = 'SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64';

{ The PATH this install changes: the machine's for all users, the user's for one }
function EnvironmentRoot: Integer;
begin
  if IsAdminInstallMode then Result := HKEY_LOCAL_MACHINE else Result := HKEY_CURRENT_USER;
end;

function EnvironmentKey: String;
begin
  if IsAdminInstallMode then Result := SystemEnvironment else Result := UserEnvironment;
end;

{ Where Dir is among Path's entries, whatever their case: 0 when it isn't }
function PathEntry(Path, Dir: String): Integer;
begin
  Result := Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Path) + ';');
end;

procedure AddToPath(Dir: String);
var
  Path: String;
begin
  if not RegQueryStringValue(EnvironmentRoot, EnvironmentKey, 'Path', Path) then Path := '';
  if PathEntry(Path, Dir) > 0 then exit;
  if (Path <> '') and (Copy(Path, Length(Path), 1) <> ';') then Path := Path + ';';
  RegWriteExpandStringValue(EnvironmentRoot, EnvironmentKey, 'Path', Path + Dir);
end;

procedure RemoveFromPath(Dir: String);
var
  Path: String;
  At: Integer;
begin
  if not RegQueryStringValue(EnvironmentRoot, EnvironmentKey, 'Path', Path) then exit;
  At := PathEntry(Path, Dir);
  if At = 0 then exit;
  Path := ';' + Path + ';';
  Delete(Path, At, Length(Dir) + 1);
  RegWriteExpandStringValue(EnvironmentRoot, EnvironmentKey, 'Path', Copy(Path, 2, Length(Path) - 2));
end;

{ TMR doesn't bundle the Visual C++ runtime (each PC keeps its one copy current), and won't start without it. winget installs it as a
  dependency, so a silent install doesn't ask. }
function InitializeSetup: Boolean;
var
  Installed: Cardinal;
begin
  Result := True;
  if WizardSilent then exit;
  if RegQueryDWordValue(HKLM64, VCRedistKey, 'Installed', Installed) and (Installed = 1) then exit;
  Result := MsgBox('TMR needs the Microsoft Visual C++ Redistributable (x64), which this PC doesn''t have. TMR doesn''t bundle it, '
    + 'so that Windows keeps one copy up to date.' + #13#10#13#10
    + 'Get it from https://aka.ms/vs/17/release/vc_redist.x64.exe, or with:' + #13#10
    + 'winget install Microsoft.VCRedist.2015+.x64' + #13#10#13#10
    + 'Install TMR anyway?', mbConfirmation, MB_YESNO) = IDYES;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('addtopath') then AddToPath(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then RemoveFromPath(ExpandConstant('{app}'));
end;
