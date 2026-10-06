; Switchyard NSIS installer (x64, per-user: no administrator rights needed).
;
; Installs to %LOCALAPPDATA%\Programs\Switchyard, registers the uninstaller under HKCU, puts
; shortcuts in the user's Start menu and desktop, and adds swy to the user PATH.
;
; Built by build-windows.ps1, which passes:
;   /DVERSION=0.1.0  /DVERSION_QUAD=0.1.0.0  /DBIN_DIR=<dir with switchyard.exe, swy.exe>
;   /DREPO_ROOT=<repo>  /DOUT_FILE=<installer path>  [/DSIGN_SCRIPT=<sign-one.ps1>]

Unicode true
SetCompressor /SOLID lzma

!include "MUI2.nsh"
!include "x64.nsh"
!include "FileFunc.nsh"

!define APP_NAME "Switchyard"
!define APP_EXE "switchyard.exe"
!define CLI_EXE "swy.exe"
!define PUBLISHER "Switchyard"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_NAME}"
!define ICON_FILE "${REPO_ROOT}\packaging\icons\switchyard.ico"

; Signing (build-windows.ps1 -Sign passes /DSIGN_SCRIPT): the uninstaller written into the
; installer and the installer itself are signed as they are produced. Needs NSIS 3.08+.
!ifdef SIGN_SCRIPT
  !uninstfinalize 'powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "${SIGN_SCRIPT}" "%1"' = 0
  !finalize 'powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "${SIGN_SCRIPT}" "%1"' = 0
!endif

Name "${APP_NAME} ${VERSION}"
OutFile "${OUT_FILE}"
InstallDir "$LOCALAPPDATA\Programs\${APP_NAME}"
InstallDirRegKey HKCU "${UNINST_KEY}" "InstallLocation"
RequestExecutionLevel user
ShowInstDetails show

VIProductVersion "${VERSION_QUAD}"
VIAddVersionKey "ProductName" "${APP_NAME}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "${APP_NAME} installer"
VIAddVersionKey "LegalCopyright" "Licensed under Apache-2.0"
VIAddVersionKey "CompanyName" "${PUBLISHER}"

!define MUI_ICON "${ICON_FILE}"
!define MUI_UNICON "${ICON_FILE}"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\${APP_EXE}"
!define MUI_FINISHPAGE_RUN_TEXT "Launch ${APP_NAME}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_OK|MB_ICONSTOP "${APP_NAME} requires 64-bit Windows."
    Abort
  ${EndIf}
  SetShellVarContext current
  SetRegView 64
  ; Earlier installers were per-machine (Program Files, HKLM). That copy needs an
  ; administrator to remove, so say so; this one installs alongside it for this user.
  ReadRegStr $0 HKLM "${UNINST_KEY}" "InstallLocation"
  ${If} $0 != ""
    MessageBox MB_OKCANCEL|MB_ICONINFORMATION "${APP_NAME} is also installed for all users in$\r$\n$0$\r$\n$\r$\nThis installer adds a copy for your account only. To remove the all-users copy, uninstall it from Settings > Apps (needs an administrator).$\r$\n$\r$\nContinue?" /SD IDOK IDOK +2
    Abort
  ${EndIf}
FunctionEnd

Function un.onInit
  SetShellVarContext current
  SetRegView 64
FunctionEnd

Section "${APP_NAME} (required)" SecApp
  SectionIn RO
  ; Close a running copy so its files can be replaced.
  nsExec::Exec 'taskkill /IM "${APP_EXE}" /F'

  SetOutPath "$INSTDIR"
  File "${BIN_DIR}\${APP_EXE}"
  File "${BIN_DIR}\${CLI_EXE}"
  File "/oname=switchyard.ico" "${ICON_FILE}"
  File "${REPO_ROOT}\packaging\windows\path.ps1"
  File "/oname=OFL-Geist.txt" "${REPO_ROOT}\crates\app\assets\fonts\OFL-Geist.txt"

  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateDirectory "$SMPROGRAMS\${APP_NAME}"
  CreateShortcut "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}" "" "$INSTDIR\switchyard.ico"
  CreateShortcut "$SMPROGRAMS\${APP_NAME}\Uninstall ${APP_NAME}.lnk" "$INSTDIR\uninstall.exe"

  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\switchyard.ico"
  WriteRegStr HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINST_KEY}" "EstimatedSize" "$0"
SectionEnd

Section "Desktop shortcut" SecDesktop
  CreateShortcut "$DESKTOP\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}" "" "$INSTDIR\switchyard.ico"
SectionEnd

Section "Add swy CLI to PATH" SecPath
  nsExec::ExecToLog 'powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$INSTDIR\path.ps1" -Action add -Dir "$INSTDIR"'
  Pop $0
  ${If} $0 == 0
    WriteRegDWORD HKCU "${UNINST_KEY}" "AddedToPath" 1
    SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000
  ${Else}
    DetailPrint "Could not update PATH (exit $0); add $INSTDIR to PATH manually."
  ${EndIf}
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecApp} "The Switchyard desktop app and the swy command-line tool."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} "Put a Switchyard shortcut on the desktop."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecPath} "Let terminals and coding CLIs run swy without a full path."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
  nsExec::Exec 'taskkill /IM "${APP_EXE}" /F'

  ReadRegDWORD $0 HKCU "${UNINST_KEY}" "AddedToPath"
  ${If} $0 == 1
    nsExec::ExecToLog 'powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$INSTDIR\path.ps1" -Action remove -Dir "$INSTDIR"'
    Pop $0
    SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000
  ${EndIf}

  Delete "$INSTDIR\${APP_EXE}"
  Delete "$INSTDIR\${CLI_EXE}"
  Delete "$INSTDIR\switchyard.ico"
  Delete "$INSTDIR\OFL-Geist.txt"
  Delete "$INSTDIR\path.ps1"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk"
  Delete "$SMPROGRAMS\${APP_NAME}\Uninstall ${APP_NAME}.lnk"
  RMDir "$SMPROGRAMS\${APP_NAME}"
  Delete "$DESKTOP\${APP_NAME}.lnk"

  ; User data (profiles, history, drivers) is left in place on purpose.
  DeleteRegKey HKCU "${UNINST_KEY}"
SectionEnd
