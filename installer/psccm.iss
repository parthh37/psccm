; PSCCM installer (Inno Setup 6).
; Build:  iscc /DAppVersion=0.2.0 installer\psccm.iss   (after: cargo build --release)
;
; Installs psccm.exe to Program Files, keeps data in %ProgramData%\PSCCM and
; runs the console as the "PSCCM" Windows service. Upgrades (including the
; silent ones started by the built-in auto-update) stop the service, replace
; the files and start it again, keeping its account and settings.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{6F1C4E2B-8D3A-4C59-9B0E-5A7D2F81C3E4}
AppName=PSCCM
AppVersion={#AppVersion}
AppVerName=PSCCM {#AppVersion}
AppPublisher=PSCCM
AppPublisherURL=https://github.com/parthh37/psccm
AppSupportURL=https://github.com/parthh37/psccm/issues
AppUpdatesURL=https://github.com/parthh37/psccm/releases
DefaultDirName={autopf}\PSCCM
DisableProgramGroupPage=yes
DefaultGroupName=PSCCM
OutputDir=..\dist
OutputBaseFilename=psccm-setup-{#AppVersion}
Compression=lzma2/max
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=admin
WizardStyle=modern
UninstallDisplayIcon={app}\psccm.exe
UninstallDisplayName=PSCCM Deployment Console
CloseApplications=no
SetupLogging=yes
LicenseFile=..\LICENSE

[Files]
Source: "..\target\release\psccm.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Dirs]
Name: "{commonappdata}\PSCCM"

[Icons]
Name: "{group}\PSCCM Console"; Filename: "{code:ConsoleUrl}"
Name: "{group}\PSCCM README"; Filename: "{app}\README.md"
Name: "{group}\Uninstall PSCCM"; Filename: "{uninstallexe}"

[Run]
Filename: "{code:ConsoleUrl}"; Description: "Open the PSCCM console"; Flags: shellexec postinstall skipifsilent nowait

[UninstallRun]
Filename: "{app}\psccm.exe"; Parameters: "service uninstall"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveService"

[Messages]
FinishedLabel=PSCCM is installed and running as the "PSCCM" Windows service.

[Code]
const
  ServiceName = 'PSCCM';

var
  ConfigPage: TInputQueryWizardPage;
  NetworkPage: TInputOptionWizardPage;
  AccountPage: TInputQueryWizardPage;
  Token: string;
  Upgrading: Boolean;

function SetEnvironmentVariable(lpName: string; lpValue: string): BOOL;
  external 'SetEnvironmentVariableW@kernel32.dll stdcall';

function ServiceExists(): Boolean;
var
  Code: Integer;
begin
  Result := Exec(ExpandConstant('{sys}\sc.exe'), 'query ' + ServiceName, '', SW_HIDE, ewWaitUntilTerminated, Code) and (Code = 0);
end;

function ServiceStopped(): Boolean;
var
  Code: Integer;
begin
  Result := Exec(ExpandConstant('{cmd}'), '/C sc.exe query ' + ServiceName + ' | find "STOPPED"', '', SW_HIDE, ewWaitUntilTerminated, Code) and (Code = 0);
end;

function GetPort(Param: string): string;
begin
  if Upgrading or (ConfigPage = nil) then
    Result := GetPreviousData('Port', '8080')
  else
    Result := Trim(ConfigPage.Values[0]);
  if Result = '' then
    Result := '8080';
end;

function ConsoleUrl(Param: string): string;
begin
  Result := 'http://localhost:' + GetPort('') + '/';
end;

function RandomToken(): string;
var
  i: Integer;
begin
  Result := '';
  for i := 1 to 40 do
    Result := Result + Copy('abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789', Random(56) + 1, 1);
end;

procedure InitializeWizard();
begin
  Upgrading := ServiceExists();

  ConfigPage := CreateInputQueryPage(wpSelectDir, 'Console settings', 'Where should the web console listen?',
    'The console is a web page served by the PSCCM service.');
  ConfigPage.Add('Port:', False);
  ConfigPage.Values[0] := GetPreviousData('Port', '8080');

  NetworkPage := CreateInputOptionPage(ConfigPage.ID, 'Network access', 'Who can open the console?',
    'Anyone who can use the console can run commands as administrator on every managed computer.', True, False);
  NetworkPage.Add('Only this computer (http://localhost) - recommended');
  NetworkPage.Add('Other computers too (an access token is generated and shown at the end)');
  if GetPreviousData('Network', '0') = '1' then
    NetworkPage.SelectedValueIndex := 1
  else
    NetworkPage.SelectedValueIndex := 0;

  AccountPage := CreateInputQueryPage(NetworkPage.ID, 'Service account',
    'Which Windows account should run deployments?',
    'Deployments to Windows computers use this account (Kerberos). Use a domain account that is a local administrator ' +
    'on the target computers, e.g. CORP\svc-psccm. Leave empty to run as Local System (then add a Windows credential in the console).');
  AccountPage.Add('Account (DOMAIN\user):', False);
  AccountPage.Add('Password:', True);
  AccountPage.Values[0] := GetPreviousData('Account', '');
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  // On upgrades the existing service keeps its settings and account.
  Result := Upgrading and ((PageID = ConfigPage.ID) or (PageID = NetworkPage.ID) or (PageID = AccountPage.ID));
end;

procedure RegisterPreviousData(PreviousDataKey: Integer);
begin
  SetPreviousData(PreviousDataKey, 'Port', GetPort(''));
  if NetworkPage.SelectedValueIndex = 1 then
    SetPreviousData(PreviousDataKey, 'Network', '1')
  else
    SetPreviousData(PreviousDataKey, 'Network', '0');
  SetPreviousData(PreviousDataKey, 'Account', AccountPage.Values[0]);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code, i: Integer;
begin
  Result := '';
  if ServiceExists() then begin
    Exec(ExpandConstant('{sys}\sc.exe'), 'stop ' + ServiceName, '', SW_HIDE, ewWaitUntilTerminated, Code);
    for i := 1 to 60 do begin
      if ServiceStopped() then break;
      Sleep(500);
    end;
    Sleep(1000);
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Code: Integer;
  Params: string;
begin
  if CurStep <> ssPostInstall then exit;
  if Upgrading then begin
    Exec(ExpandConstant('{sys}\sc.exe'), 'start ' + ServiceName, '', SW_HIDE, ewWaitUntilTerminated, Code);
    exit;
  end;
  Params := '--data-dir "' + ExpandConstant('{commonappdata}\PSCCM') + '" service install --port ' + GetPort('');
  if NetworkPage.SelectedValueIndex = 1 then begin
    Token := RandomToken();
    Params := Params + ' --bind 0.0.0.0 --token ' + Token;
  end;
  if Trim(AccountPage.Values[0]) <> '' then begin
    Params := Params + ' --account "' + Trim(AccountPage.Values[0]) + '"';
    SetEnvironmentVariable('PSCCM_SERVICE_PASSWORD', AccountPage.Values[1]);
  end;
  if not Exec(ExpandConstant('{app}\psccm.exe'), Params, '', SW_HIDE, ewWaitUntilTerminated, Code) or (Code <> 0) then
    MsgBox('The PSCCM service could not be installed (exit code ' + IntToStr(Code) + ').' + #13#10 +
      'Check the account name and password, or run "psccm service install" from an administrator prompt.', mbError, MB_OK);
  SetEnvironmentVariable('PSCCM_SERVICE_PASSWORD', '');
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if (CurPageID = wpFinished) and (Token <> '') then
    WizardForm.FinishedLabel.Caption := 'PSCCM is installed and running as a Windows service.' + #13#10#13#10 +
      'Console: http://<this-computer>:' + GetPort('') + '/' + #13#10 +
      'Access token (save it now): ' + Token;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    if MsgBox('Delete the PSCCM data folder (computers, credentials, history, packages)?' + #13#10 +
      ExpandConstant('{commonappdata}\PSCCM'), mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
      DelTree(ExpandConstant('{commonappdata}\PSCCM'), True, True, True);
end;
