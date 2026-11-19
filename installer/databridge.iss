; DataBridge installer (Inno Setup 6). Built by CI on every `v*` tag:
;
;   iscc /DAppVersion=0.5.0 installer\databridge.iss
;
; -> dist\DataBridge-Setup-x64.exe (the name `core::update::SETUP_ASSET`
; downloads; keep them in step).
;
; Per-user install like VS Code's user setup: %LOCALAPPDATA%\Programs\
; DataBridge, no administrator rights, Start menu entry, optional desktop
; icon, an entry in "Installed apps" with an uninstaller.
;
; Updates run this same setup silently from the app:
;
;   DataBridge-Setup-x64.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP- /update=1
;
; The app starts the setup and quits; with /update=1 the setup waits for
; the app's instance mutex to go away (the app holds `DataBridge.Instance`
; while it runs, `core::update::hold_instance_mutex`), installs over the
; previous directory and tasks, and starts the new version again.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

#define AppName "DataBridge"
#define AppExe "databridge.exe"
#define AppMutexName "DataBridge.Instance"

[Setup]
; Never change the AppId: it ties updates and the uninstaller together.
AppId={{6E1C3B2A-8F4D-4C9E-A5B7-2D90F3E41C58}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=Incoderr
AppPublisherURL=https://github.com/Incoderr/DataBridge
AppSupportURL=https://github.com/Incoderr/DataBridge/issues
AppUpdatesURL=https://github.com/Incoderr/DataBridge/releases
VersionInfoVersion={#AppVersion}
; Per-user by default ({autopf} = %LOCALAPPDATA%\Programs); the user may
; still choose "for all users" in the dialog (then it asks for admin).
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
DefaultDirName={autopf}\{#AppName}
DisableProgramGroupPage=yes
DisableDirPage=auto
DisableReadyPage=yes
UninstallDisplayName={#AppName}
UninstallDisplayIcon={app}\{#AppExe}
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\dist
OutputBaseFilename=DataBridge-Setup-x64
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; The app is waited for in [Code] (an update must not prompt).
CloseApplications=no

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"
Name: "ru"; MessagesFile: "compiler:Languages\Russian.isl"

[CustomMessages]
en.CloseApp=DataBridge is running. Close it and press OK to continue.
ru.CloseApp=DataBridge запущен. Закройте его и нажмите OK, чтобы продолжить.

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
; Interactive install: the usual "Launch DataBridge" checkbox.
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent
; Silent update from the app: start the new version again.
Filename: "{app}\{#AppExe}"; Flags: nowait; Check: IsUpdate

[Code]
function IsUpdate: Boolean;
begin
  Result := ExpandConstant('{param:update|0}') = '1';
end;

{ Wait until no DataBridge instance holds the mutex: quietly for an update
  (the app is quitting right now), with a retry prompt otherwise. }
function WaitForApp(Prompt: Boolean): Boolean;
var
  I: Integer;
begin
  Result := True;
  { An app that just started this setup is quitting: give it a moment
    (30 s for a silent update, 3 s before asking the user). }
  for I := 1 to 120 do
  begin
    if Prompt and (I > 12) then
      Break;
    if not CheckForMutexes('{#AppMutexName}') then
      Exit;
    Sleep(250);
  end;
  while CheckForMutexes('{#AppMutexName}') do
  begin
    if not Prompt then
    begin
      Result := False;
      Exit;
    end;
    if MsgBox(CustomMessage('CloseApp'), mbError, MB_OKCANCEL) = IDCANCEL then
    begin
      Result := False;
      Exit;
    end;
  end;
end;

function InitializeSetup: Boolean;
begin
  if IsUpdate then
    Result := WaitForApp(False)
  else if CheckForMutexes('{#AppMutexName}') then
    Result := WaitForApp(True)
  else
    Result := True;
end;

function InitializeUninstall: Boolean;
begin
  Result := (not CheckForMutexes('{#AppMutexName}')) or WaitForApp(True);
end;
