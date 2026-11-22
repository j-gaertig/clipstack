; Inno Setup script for Clipstack-Setup-x64.exe (per-user, no admin rights).
; Build: iscc /DAppVersion=1.2.3 packaging\windows\clipstack.iss
; Expects the release build as dist\Clipstack.exe in the repository root.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{6B1D8C2E-3F4A-4E7B-9C1D-2A5F8E0B7C44}
AppName=Clipstack
AppVersion={#AppVersion}
AppVerName=Clipstack {#AppVersion}
AppPublisher=Clipstack
AppPublisherURL=https://github.com/j-gaertig/clipstack
AppSupportURL=https://github.com/j-gaertig/clipstack/issues
DefaultDirName={localappdata}\Programs\Clipstack
DisableProgramGroupPage=yes
DisableDirPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\..
OutputBaseFilename=Clipstack-Setup-x64
SetupIconFile=clipstack.ico
UninstallDisplayIcon={app}\Clipstack.exe
LicenseFile=..\..\LICENSE
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
CloseApplications=yes

[Files]
Source: "..\..\dist\Clipstack.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Clipstack"; Filename: "{app}\Clipstack.exe"

[Run]
; Starts the service; it registers autostart itself and then listens for Win+V.
Filename: "{app}\Clipstack.exe"; Parameters: "--background"; Description: "Start Clipstack"; Flags: nowait postinstall

[UninstallRun]
Filename: "{app}\Clipstack.exe"; Parameters: "--uninstall"; Flags: runhidden; RunOnceId: "RemoveIntegration"
Filename: "{sys}\taskkill.exe"; Parameters: "/f /im Clipstack.exe"; Flags: runhidden; RunOnceId: "StopClipstack"
