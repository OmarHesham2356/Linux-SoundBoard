; Inno Setup script for the Windows build.
;
; Build the portable tree first with packaging/windows/package.sh, then:
;   ISCC.exe /DAppVersion=2.4.4 packaging/windows/linux-soundboard.iss

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

#define AppName "Linux Soundboard"
#define AppExe "linux-soundboard.exe"

[Setup]
AppId={{0A8F7E52-3F2B-4D1E-9B6A-2C6F1D9A7C41}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=germanua
AppPublisherURL=https://github.com/germanua/Linux-SoundBoard
AppSupportURL=https://github.com/germanua/Linux-SoundBoard/issues
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
OutputDir=..\..\dist\windows-installer
OutputBaseFilename=LinuxSoundboard-{#AppVersion}-windows-x64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\{#AppExe}
PrivilegesRequiredOverridesAllowed=dialog
ChangesEnvironment=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional icons:"; Flags: unchecked
Name: "vbcable"; Description: "Download and install VB-CABLE (the virtual microphone driver Windows needs, 1.3 MB)"; GroupDescription: "Requirements:"

[Files]
Source: "..\..\dist\windows\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"; WorkingDir: "{app}"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExe}"; WorkingDir: "{app}"; Description: "Launch {#AppName}"; Flags: nowait postinstall skipifsilent

[Code]
const
  // The official package. VB-Audio publishes a new, versioned file name for each
  // driver revision, so picking up a newer cable means updating this URL.
  VbCableZipUrl = 'https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack45.zip';
  VbCablePageUrl = 'https://vb-audio.com/Cable/';
  CRLF = #13#10;

var
  VbCableSetupPath: String;
  VbCableProblem: String;

function VbCableFolder(): String;
begin
  Result := ExpandConstant('{localappdata}\Linux Soundboard\vbcable');
end;

// Downloads the official VB-CABLE package from VB-Audio and unpacks it into the
// app data folder. True means VbCableSetupPath points at a ready setup program;
// False means VbCableProblem explains the download failure to the user.
function DownloadVbCable(): Boolean;
var
  Target, ZipPath, Params: String;
  ResultCode: Integer;
begin
  Result := False;
  VbCableProblem := '';
  Target := VbCableFolder();
  ZipPath := Target + '\VBCABLE_Driver_Pack45.zip';

  if not ForceDirectories(Target) then
  begin
    VbCableProblem := 'The folder ' + Target + ' could not be created.';
    exit;
  end;

  Params := '-NoProfile -ExecutionPolicy Bypass -Command "' +
    '$ProgressPreference = ''SilentlyContinue''; ' +
    '$ErrorActionPreference = ''Stop''; ' +
    'Invoke-WebRequest -UseBasicParsing -Uri ''' + VbCableZipUrl + ''' -OutFile ''' + ZipPath + '''; ' +
    'Expand-Archive -LiteralPath ''' + ZipPath + ''' -DestinationPath ''' + Target + ''' -Force"';

  if not Exec('powershell.exe', Params, Target, SW_HIDE, ewWaitUntilTerminated, ResultCode) then
  begin
    VbCableProblem := 'Windows could not start PowerShell.';
    exit;
  end;

  if ResultCode <> 0 then
  begin
    VbCableProblem := 'Windows could not download or unpack the package.';
    exit;
  end;

  if FileExists(Target + '\VBCABLE_Setup_x64.exe') then
    VbCableSetupPath := Target + '\VBCABLE_Setup_x64.exe'
  else if FileExists(Target + '\VBCABLE_Setup.exe') then
    VbCableSetupPath := Target + '\VBCABLE_Setup.exe'
  else
  begin
    VbCableProblem := 'The downloaded package did not contain the VB-CABLE setup program.';
    exit;
  end;

  Result := True;
end;

// Asks before running the driver setup: it is a signed third-party program that
// needs administrator rights and a restart, so it stays the user's decision.
procedure OfferVbCableInstall();
var
  ResultCode: Integer;
begin
  if VbCableSetupPath = '' then
    exit;

  if MsgBox('VB-CABLE is downloaded and unpacked.' + CRLF + CRLF +
      'Install it now? Windows will ask for administrator rights, and the virtual ' +
      'microphone only appears after a restart.', mbConfirmation, MB_YESNO) <> IDYES then
  begin
    MsgBox('You can run the setup later from:' + CRLF + VbCableSetupPath,
      mbInformation, MB_OK);
    exit;
  end;

  if not ShellExec('runas', VbCableSetupPath, '', ExtractFileDir(VbCableSetupPath),
      SW_SHOWNORMAL, ewNoWait, ResultCode) then
    MsgBox('The VB-CABLE setup could not be started. Run it yourself from' + CRLF +
      VbCableSetupPath, mbError, MB_OK)
  else
    MsgBox('The VB-CABLE setup is starting. Install the driver, then restart Windows ' +
      'so the virtual microphone appears.', mbInformation, MB_OK);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep <> ssPostInstall then
    exit;
  if not WizardIsTaskSelected('vbcable') then
    exit;

  WizardForm.StatusLabel.Caption := 'Downloading VB-CABLE (virtual microphone driver)...';
  WizardForm.StatusLabel.Update;

  if DownloadVbCable() then
    OfferVbCableInstall()
  else
    MsgBox('VB-CABLE could not be downloaded.' + CRLF + CRLF + VbCableProblem +
      CRLF + CRLF +
      '{#AppName} is installed and runs, but without a virtual audio cable it cannot ' +
      'be heard in other applications. Download VB-CABLE from ' + VbCablePageUrl +
      ' later, or use the Download VB-CABLE button in the app.', mbInformation, MB_OK);

  WizardForm.StatusLabel.Caption := 'VB-CABLE: done.';
  WizardForm.StatusLabel.Update;
end;
