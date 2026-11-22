; Inno Setup script for the Windows installer.
; Built by .github/workflows/release.yml:
;   iscc /DMyAppVersion=1.2.3 packaging\windows\magic-pdf.iss
; Installs per user (no administrator rights needed), adds a Start menu entry,
; registers magic-pdf for PDF files ("Open with" and Windows' Default apps page)
; and an uninstaller.

#define MyAppName "magic-pdf"
#ifndef MyAppVersion
  #define MyAppVersion "0.0.0"
#endif
#define MyAppExe "magic-pdf.exe"
#define MyProgId "magic-pdf.Document"
#define Root "..\.."

[Setup]
AppId={{8C2D5E6A-4B1F-4E8A-9F3D-6A7B2C1D0E9F}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher=magic-pdf contributors
AppPublisherURL=https://github.com/magicmatt007/magic-pdf
AppSupportURL=https://github.com/magicmatt007/magic-pdf/issues
DefaultDirName={autopf}\{#MyAppName}
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#Root}\dist
OutputBaseFilename=magic-pdf-{#MyAppVersion}-setup
SetupIconFile={#Root}\assets\icons\magic-pdf.ico
UninstallDisplayIcon={app}\{#MyAppExe}
UninstallDisplayName={#MyAppName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ChangesAssociations=yes
; Closes a running magic-pdf when updating.
CloseApplications=yes

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#Root}\target\release\{#MyAppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\third_party\pdfium\pdfium.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "{#Root}\third_party\pdfium\LICENSE"; DestDir: "{app}"; DestName: "PDFIUM-LICENSE.txt"; Flags: ignoreversion
Source: "{#Root}\assets\fonts\phosphor\LICENSE"; DestDir: "{app}"; DestName: "PHOSPHOR-LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExe}"; Tasks: desktopicon

[Registry]
; File type used when magic-pdf opens PDFs.
Root: HKA; Subkey: "Software\Classes\{#MyProgId}"; ValueType: string; ValueName: ""; ValueData: "PDF Document"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\{#MyProgId}\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExe},0"
Root: HKA; Subkey: "Software\Classes\{#MyProgId}\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExe}"" ""%1"""
; Listed under "Open with" for PDFs.
Root: HKA; Subkey: "Software\Classes\.pdf\OpenWithProgids"; ValueType: string; ValueName: "{#MyProgId}"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\Applications\{#MyAppExe}"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Applications\{#MyAppExe}\SupportedTypes"; ValueType: string; ValueName: ".pdf"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\{#MyAppExe}\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExe}"" ""%1"""
; Shown on Windows' Default apps page.
Root: HKA; Subkey: "Software\{#MyAppName}\Capabilities"; ValueType: string; ValueName: "ApplicationName"; ValueData: "{#MyAppName}"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\{#MyAppName}\Capabilities"; ValueType: string; ValueName: "ApplicationDescription"; ValueData: "Lightweight PDF viewer with a horizontal page layout for wide screens"
Root: HKA; Subkey: "Software\{#MyAppName}\Capabilities"; ValueType: string; ValueName: "ApplicationIcon"; ValueData: "{app}\{#MyAppExe},0"
Root: HKA; Subkey: "Software\{#MyAppName}\Capabilities\FileAssociations"; ValueType: string; ValueName: ".pdf"; ValueData: "{#MyProgId}"
Root: HKA; Subkey: "Software\RegisteredApplications"; ValueType: string; ValueName: "{#MyAppName}"; ValueData: "Software\{#MyAppName}\Capabilities"; Flags: uninsdeletevalue

[Run]
Filename: "{app}\{#MyAppExe}"; Description: "{cm:LaunchProgram,{#MyAppName}}"; Flags: nowait postinstall skipifsilent
; Updates from within magic-pdf run the installer silently: start magic-pdf again.
Filename: "{app}\{#MyAppExe}"; Flags: nowait; Check: WizardSilent
; Windows does not let installers change the default app; this opens the right Settings page.
Filename: "ms-settings:defaultapps?registeredAppUser={#MyAppName}"; Description: "Make magic-pdf the default PDF viewer (opens Windows Settings)"; Flags: shellexec nowait postinstall skipifsilent unchecked
