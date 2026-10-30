; Instalator Komunikatora (Inno Setup 6). Budowany przez workflow „Wydanie klienta”:
;   ISCC.exe /DAppVersion=X.Y.Z /DExeDir=<katalog z komunikator.exe> /DOutputDir=<wyjście> installer\komunikator.iss
; Instaluje dla bieżącego użytkownika, bez UAC, do %LOCALAPPDATA%\Programs\Komunikator: program
; aktualizuje się sam, podmieniając swój plik, więc potrzebuje folderu z prawem zapisu.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef ExeDir
  #define ExeDir "..\target\release"
#endif
#ifndef OutputDir
  #define OutputDir "..\dist"
#endif

#define AppName "Komunikator"
#define AppExe "komunikator.exe"
; Ta sama nazwa i ta sama wartość co w programie (src/ui/autostart.rs).
#define RunKey "Software\Microsoft\Windows\CurrentVersion\Run"
#define RunValue "komunikator"

[Setup]
AppId={{2FC32A67-8179-444C-9397-D2987C4E2C29}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppName}
VersionInfoVersion={#AppVersion}
PrivilegesRequired=lowest
DefaultDirName={autopf}\{#AppName}
DisableProgramGroupPage=yes
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
SetupIconFile=..\crates\client-win\icon\komunikator.ico
OutputDir={#OutputDir}
OutputBaseFilename=komunikator-setup-{#AppVersion}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
; Działający program trzyma muteks o tej nazwie (src/ui/instance.rs, APP_MUTEX): instalator
; i deinstalator proszą wtedy o jego zamknięcie.
AppMutex=komunikator-app
CloseApplications=no

[Languages]
Name: "polish"; MessagesFile: "compiler:Languages\Polish.isl"

[Tasks]
Name: "autostart"; Description: "Uruchamiaj Komunikator razem z Windows (w zasobniku)"; GroupDescription: "Uruchamianie:"; Flags: unchecked
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#ExeDir}\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
; Ten sam wpis, który zapisuje program; program przy starcie zaznacza wtedy „Uruchamiaj z Windows”.
Root: HKCU; Subkey: "{#RunKey}"; ValueType: string; ValueName: "{#RunValue}"; ValueData: """{app}\{#AppExe}"" --minimized"; Tasks: autostart

[Run]
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; Pozostałości samoaktualizacji (src/updater.rs). Dane w %APPDATA%\komunikator i zapisane
; logowanie zostają: po ponownej instalacji konto i zweryfikowane osoby są na miejscu.
Type: files; Name: "{app}\{#AppExe}.old"
Type: files; Name: "{app}\{#AppExe}.new"
Type: files; Name: "{app}\komunikator.version"
Type: dirifempty; Name: "{app}"

[Code]
// Autostart mógł włączyć sam program (Ustawienia), więc wpis znika przy odinstalowaniu
// niezależnie od tego, czy zaznaczono go w instalatorze.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
    RegDeleteValue(HKEY_CURRENT_USER, '{#RunKey}', '{#RunValue}');
end;
