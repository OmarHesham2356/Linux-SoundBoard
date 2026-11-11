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

[Files]
Source: "..\..\dist\windows\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"; WorkingDir: "{app}"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExe}"; WorkingDir: "{app}"; Description: "Launch {#AppName}"; Flags: nowait postinstall skipifsilent
