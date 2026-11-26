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
; 用户要求（2026-11-21）：安装/卸载只执行一次关停检查，不再轮询 150 次；
; helper 按系统代码页逐行输出关停结论、每条说明（会话、状态、在途请求）和完整日志位置。
; 用户第二次确认：安装和卸载一律放行。执行中的 Codex 留在 Elves 私有服务里继续（安装后新版自动接上），
; 没有执行中的会话时 Codex 一并关闭；新版 Harness 不再返回 busy，busy 只会来自尚未升级的旧版。
; 卸载传 --shutdown-mode=uninstall，只用于提示“卸载后不会再有 Elves 接上”。
; 用户第三次确认（2026-11-21）：安装程序自己关闭 Elves，任何情况下都不要求用户先退出，也不中止安装或卸载；
; helper 的退出码只决定安装日志里的说明，随后 StopElves 一律结束剩余的 Elves 进程。
; 用户要求（2026-11-26）：安装与卸载全程不得终止、删除或修改与 CodexElves 相关的程序、进程、
; 快捷方式及注册表项，仅管理 Elves 自身；旧版配置与数据在安装阶段单向同步到新版 `~/.elves` 目录，保留旧目录不变。
!define SHUTDOWN_BUSY_EXIT_CODE 10
!macro RemoveObsoleteElvesFiles DIRECTORY
  ; 仅清理 Elves 自身历史遗留文件和已内嵌的分散工具，绝不删除任何 CodexElves 程序。
  Delete "${DIRECTORY}\elves-management.exe"
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
!macro RemoveObsoleteElvesShortcuts
  ; 仅清理 Elves 自身旧快捷方式，完整保留 CodexElves 桌面与开始菜单入口。
  Delete "$DESKTOP\Elves 远程会话.lnk"
  Delete "$DESKTOP\Elves 管理工具.lnk"
  Delete "$SMPROGRAMS\Elves\Elves 远程会话.lnk"
  Delete "$SMPROGRAMS\Elves\Elves 管理工具.lnk"
!macroend
!macro StopElves
  DetailPrint "正在关闭 Elves 进程…"
  ; 用户要求不得终止旧版 CodexElves 进程，仅关闭 Elves 与 Elves 代理管理器自身。
  nsExec::ExecToLog 'taskkill /IM elves.exe /F'
  Pop $0
  nsExec::ExecToLog 'taskkill /IM elves-manager.exe /F'
  Pop $0
!macroend
!macro PrepareShutdown EXECUTABLE LABEL OPERATION
  ; 只说明结论，不 Abort：0 为已确认；busy 只会来自尚未升级的旧版（旧版的 Codex 是 Harness 子进程，
  ; 做不到退出后继续执行）；其他值为状态无法确认。三种情况都接着由 StopElves 关闭 Elves。
  nsExec::ExecToLog '"${EXECUTABLE}" --prepare-shutdown --shutdown-mode=${LABEL}'
  Pop $0
  ${If} $0 == ${SHUTDOWN_BUSY_EXIT_CODE}
    DetailPrint "旧版 Elves 判断仍有任务在执行，旧版做不到退出后继续执行；"
    DetailPrint "${OPERATION}程序直接关闭 Elves，这些任务会中断。"
  ${ElseIf} $0 != 0
    DetailPrint "未能确认 Elves 状态（见上方说明），${OPERATION}程序直接关闭 Elves。"
  ${EndIf}
!macroend
!macro MigrateLegacyData EXECUTABLE
  ; 用户要求在安装包阶段识别旧版目录并将配置与数据单向同步到新版 `~/.elves`（含 `desktop`、`cli`），
  ; 旧目录保持只读不变，同步结果按系统代码页输出到安装日志，不因部分非核心锁文件阻塞安装。
  DetailPrint "正在识别旧版配置并同步到新版目录…"
  nsExec::ExecToLog '"${EXECUTABLE}" --migrate-legacy-data'
  Pop $0
  ${If} $0 != 0
    DetailPrint "旧版配置同步未完全完成（退出码 $0，见上方说明），安装将继续。"
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
  !insertmacro RemoveObsoleteElvesFiles "$INSTDIR"
  !insertmacro RemoveObsoleteElvesShortcuts
  ; Elves 负责 Harness 工作区；代理设置、会话管理由独立管理器承载。
  File "${ROOT}\dist\windows\app\elves.exe"
  File "${ROOT}\dist\windows\app\elves-manager.exe"
  !insertmacro MigrateLegacyData "$INSTDIR\elves.exe"
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
  !insertmacro RemoveObsoleteElvesFiles "$INSTDIR"
  !insertmacro RemoveObsoleteElvesShortcuts
  Delete "$DESKTOP\Elves.lnk"
  Delete "$SMPROGRAMS\Elves\Elves.lnk"
  Delete "$SMPROGRAMS\Elves\Elves 代理管理器.lnk"
  Delete "$SMPROGRAMS\Elves\卸载 Elves.lnk"
  RMDir "$SMPROGRAMS\Elves"
  ; 设置、密钥、原生会话及 WebView 缓存位于独立用户目录 `~/.elves`，卸载保留。
  Delete "$INSTDIR\elves.exe"
  Delete "$INSTDIR\elves-manager.exe"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Elves"
  DeleteRegKey HKCU "Software\Elves"
SectionEnd
