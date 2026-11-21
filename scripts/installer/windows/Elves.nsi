Unicode true
!include "MUI2.nsh"
!include "LogicLib.nsh"
!ifndef VERSION
!define VERSION "0.0.0"
!endif
!define ROOT "..\..\.."
Name "Elves"
OutFile "${ROOT}\dist\windows\Elves-${VERSION}-windows-x64-setup.exe"
InstallDir "$LOCALAPPDATA\Programs\Elves"
InstallDirRegKey HKCU "Software\Elves" "InstallDir"
RequestExecutionLevel admin
!ifdef FAST_COMPRESSION
; 快速本地包使用 zlib，减少重复打包等待；正式包仍使用固实 LZMA 以保持较小体积。
SetCompressor zlib
!else
SetCompressor /SOLID lzma
!endif
!define MUI_ICON "${ROOT}\apps\elves\src-tauri\icons\elves-app-icon.ico"
!define MUI_UNICON "${ROOT}\apps\elves\src-tauri\icons\elves-app-icon.ico"
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
; 完成页直接运行 Elves；独立代理管理器从开始菜单进入，不自动启动。
!define MUI_FINISHPAGE_RUN "$INSTDIR\elves.exe"
!define MUI_FINISHPAGE_RUN_TEXT "运行 Elves"
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "SimpChinese"
!insertmacro MUI_LANGUAGE "English"
Var LegacyInstallDir
; 用户要求（2026-11-21）：安装/卸载只执行一次关停检查，不再轮询 150 次；
; helper 按系统代码页逐行输出关停结论、每条说明（会话、状态、在途请求）和完整日志位置。
; 用户第二次确认：安装和卸载一律放行。执行中的 Codex 留在 Elves 私有服务里继续（安装后新版自动接上），
; 没有执行中的会话时 Codex 一并关闭；新版 Harness 不再返回 busy，busy 只会来自尚未升级的旧版。
; 卸载传 --shutdown-mode=uninstall，只用于提示“卸载后不会再有 Elves 接上”。
!define SHUTDOWN_BUSY_EXIT_CODE 10
Function .onInit
  ; 旧品牌只用于迁移，不继续创建旧产品入口或覆盖用户数据。
  ReadRegStr $LegacyInstallDir HKCU "Software\CodexElves" "InstallDir"
FunctionEnd
!macro RemoveLegacyPrograms DIRECTORY
  Delete "${DIRECTORY}\codex-elves.exe"
  Delete "${DIRECTORY}\codex-elves-manager.exe"
  Delete "${DIRECTORY}\codex-elves-task-board.exe"
  Delete "${DIRECTORY}\codex-elves-remote.exe"
  Delete "${DIRECTORY}\elves-management.exe"
  Delete "${DIRECTORY}\elves-manager.exe"
  Delete "${DIRECTORY}\tools\windows-x64\rg.exe"
  Delete "${DIRECTORY}\tools\windows-x64\node_repl.exe"
  Delete "${DIRECTORY}\tools\windows-x64\node.exe"
  Delete "${DIRECTORY}\tools\windows-x64\LICENSE-MIT.txt"
  Delete "${DIRECTORY}\tools\windows-x64\LICENSE-UNLICENSE.txt"
  Delete "${DIRECTORY}\tools\windows-x64\LICENSE-NODE.txt"
  Delete "${DIRECTORY}\tools\tool-manifest.json"
  RMDir "${DIRECTORY}\tools\windows-x64"
  RMDir "${DIRECTORY}\tools"
!macroend
!macro RemoveLegacyShortcuts
  Delete "$DESKTOP\CodexElves.lnk"
  Delete "$DESKTOP\CodexElves 管理工具.lnk"
  Delete "$DESKTOP\CodexElves Manager.lnk"
  Delete "$DESKTOP\CodexElves 远程会话.lnk"
  Delete "$DESKTOP\CodexElves 绠＄悊宸ュ叿.lnk"
  Delete "$SMPROGRAMS\CodexElves\CodexElves.lnk"
  Delete "$SMPROGRAMS\CodexElves\CodexElves 管理工具.lnk"
  Delete "$SMPROGRAMS\CodexElves\CodexElves 远程会话.lnk"
  Delete "$SMPROGRAMS\CodexElves\卸载 CodexElves.lnk"
  Delete "$SMPROGRAMS\CodexElves\CodexElves 绠＄悊宸ュ叿.lnk"
  Delete "$SMPROGRAMS\CodexElves\鍗歌浇 CodexElves.lnk"
  Delete "$SMPROGRAMS\CodexElves Manager.lnk"
  RMDir "$SMPROGRAMS\CodexElves"
  Delete "$DESKTOP\Elves 远程会话.lnk"
  Delete "$DESKTOP\Elves 管理工具.lnk"
  Delete "$SMPROGRAMS\Elves\Elves 远程会话.lnk"
  Delete "$SMPROGRAMS\Elves\Elves 管理工具.lnk"
!macroend
!macro StopElves
  nsExec::ExecToLog 'taskkill /IM codex-elves.exe /F'
  Pop $0
  nsExec::ExecToLog 'taskkill /IM codex-elves-manager.exe /F'
  Pop $0
  nsExec::ExecToLog 'taskkill /IM codex-elves-task-board.exe /F'
  Pop $0
  nsExec::ExecToLog 'taskkill /IM codex-elves-remote.exe /F'
  Pop $0
  nsExec::ExecToLog 'taskkill /IM elves.exe /F'
  Pop $0
  nsExec::ExecToLog 'taskkill /IM elves-manager.exe /F'
  Pop $0
!macroend
!macro PrepareShutdown EXECUTABLE LABEL OPERATION
  nsExec::ExecToLog '"${EXECUTABLE}" --prepare-shutdown --shutdown-mode=${LABEL}'
  Pop $0
  ${If} $0 == ${SHUTDOWN_BUSY_EXIT_CODE}
    DetailPrint "正在运行的旧版 Elves 判断仍有任务，未能停止，${OPERATION}已中止；确认没有任务在执行后，在任务管理器结束所有 elves.exe 再重新运行。"
    Abort
  ${ElseIf} $0 != 0
    DetailPrint "未能确认 Elves 已安全停止，${OPERATION}已中止；请查看上方关停检查结果。"
    Abort
  ${EndIf}
!macroend
Section "Install"
  InitPluginsDir
  SetOutPath "$PLUGINSDIR"
  ; 退出 helper 只是同一 Elves 程序的临时副本，不作为第二个应用安装。
  File /oname=elves-shutdown-helper.exe "${ROOT}\dist\windows\app\elves.exe"
  !insertmacro PrepareShutdown "$PLUGINSDIR\elves-shutdown-helper.exe" install "安装"
  !insertmacro StopElves
  SetOutPath "$INSTDIR"
  !insertmacro RemoveLegacyPrograms "$INSTDIR"
  ${If} $LegacyInstallDir != ""
  ${AndIf} $LegacyInstallDir != $INSTDIR
    !insertmacro RemoveLegacyPrograms "$LegacyInstallDir"
    Delete "$LegacyInstallDir\uninstall.exe"
    RMDir "$LegacyInstallDir"
  ${EndIf}
  !insertmacro RemoveLegacyShortcuts
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\CodexElves"
  DeleteRegKey HKCU "Software\CodexElves"
  ; Elves 负责 Harness 工作区；代理设置、会话管理由独立管理器承载。
  File "${ROOT}\dist\windows\app\elves.exe"
  File "${ROOT}\dist\windows\app\elves-manager.exe"
  CreateShortcut "$DESKTOP\Elves.lnk" "$INSTDIR\elves.exe" "" "$INSTDIR\elves.exe"
  CreateDirectory "$SMPROGRAMS\Elves"
  CreateShortcut "$SMPROGRAMS\Elves\Elves.lnk" "$INSTDIR\elves.exe" "" "$INSTDIR\elves.exe"
  CreateShortcut "$SMPROGRAMS\Elves\Elves 代理管理器.lnk" "$INSTDIR\elves-manager.exe" "" "$INSTDIR\elves-manager.exe"
  CreateShortcut "$SMPROGRAMS\Elves\卸载 Elves.lnk" "$INSTDIR\uninstall.exe" "" "$INSTDIR\elves.exe"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "Software\Elves" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Elves" "DisplayName" "Elves"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Elves" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Elves" "Publisher" "junxin367"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Elves" "DisplayIcon" "$INSTDIR\elves.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Elves" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Elves" "UninstallString" "$INSTDIR\uninstall.exe"
SectionEnd
Section "Uninstall"
  !insertmacro PrepareShutdown "$INSTDIR\elves.exe" uninstall "卸载"
  !insertmacro StopElves
  !insertmacro RemoveLegacyPrograms "$INSTDIR"
  !insertmacro RemoveLegacyShortcuts
  Delete "$DESKTOP\Elves.lnk"
  Delete "$SMPROGRAMS\Elves\Elves.lnk"
  Delete "$SMPROGRAMS\Elves\Elves 代理管理器.lnk"
  Delete "$SMPROGRAMS\Elves\卸载 Elves.lnk"
  RMDir "$SMPROGRAMS\Elves"
  ; 设置、密钥、原生会话及 WebView 缓存位于独立用户目录，卸载保留。
  Delete "$INSTDIR\elves.exe"
  Delete "$INSTDIR\elves-manager.exe"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Elves"
  DeleteRegKey HKCU "Software\Elves"
SectionEnd
