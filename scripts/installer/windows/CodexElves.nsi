Unicode true
!include "MUI2.nsh"
!include "LogicLib.nsh"

!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!define ROOT "..\..\.."

Name "CodexElves"
OutFile "${ROOT}\dist\windows\CodexElves-${VERSION}-windows-x64-setup.exe"
InstallDir "$LOCALAPPDATA\Programs\CodexElves"
InstallDirRegKey HKCU "Software\CodexElves" "InstallDir"
RequestExecutionLevel admin
SetCompressor /SOLID lzma

!define MUI_ICON "${ROOT}\apps\codex-elves-manager\src-tauri\icons\icon.ico"
!define MUI_UNICON "${ROOT}\apps\codex-elves-manager\src-tauri\icons\icon.ico"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
; 用户要求只安装独立远程客户端；完成页沿用默认勾选，取消勾选或静默安装均不启动。
!define MUI_FINISHPAGE_RUN "$INSTDIR\codex-elves-remote.exe"
!define MUI_FINISHPAGE_RUN_TEXT "$(FinishRunRemote)"
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "SimpChinese"
!insertmacro MUI_LANGUAGE "English"

LangString FinishRunRemote ${LANG_SIMPCHINESE} "运行 CodexElves 远程会话"
LangString FinishRunRemote ${LANG_ENGLISH} "Run CodexElves Remote"

Section "Install"
  ; 用户要求安装升级先请求安全回收；使用包内新版 helper，旧版无接口时保存意图供启动补偿。
  InitPluginsDir
  SetOutPath "$PLUGINSDIR"
  File /oname=codex-elves-shutdown-helper.exe "${ROOT}\dist\windows\app\codex-elves-remote.exe"
  nsExec::ExecToLog '"$PLUGINSDIR\codex-elves-shutdown-helper.exe" --prepare-remote-shutdown'
  Pop $0
  ${If} $0 != 0
    DetailPrint "Claude 回收尚未全部确认，已保留记录供下次启动核对。"
  ${EndIf}
  SetOutPath "$INSTDIR"

  nsExec::ExecToLog 'taskkill /IM codex-elves.exe /F'
  Pop $0
  nsExec::ExecToLog 'taskkill /IM codex-elves-manager.exe /F'
  Pop $0
  nsExec::ExecToLog 'taskkill /IM codex-elves-task-board.exe /F'
  Pop $0
  ; 单客户端安装：升级前释放客户端及其内置后台，旧程序仅为升级清理保留退出逻辑。
  nsExec::ExecToLog 'taskkill /IM codex-elves-remote.exe /F'
  Pop $0

  ; 升级时移除旧管理器、launcher 和任务看板；设置、密钥、设备授权及登录缓存仍保留在用户目录。
  Delete "$INSTDIR\codex-elves.exe"
  Delete "$INSTDIR\codex-elves-manager.exe"
  Delete "$INSTDIR\codex-elves-task-board.exe"
  ; 清理旧安装器部署的固定工具；新版工具仅在用户缓存中释放，不递归删除未知文件。
  Delete "$INSTDIR\tools\windows-x64\rg.exe"
  Delete "$INSTDIR\tools\windows-x64\node_repl.exe"
  Delete "$INSTDIR\tools\windows-x64\node.exe"
  Delete "$INSTDIR\tools\windows-x64\LICENSE-MIT.txt"
  Delete "$INSTDIR\tools\windows-x64\LICENSE-UNLICENSE.txt"
  Delete "$INSTDIR\tools\windows-x64\LICENSE-NODE.txt"
  Delete "$INSTDIR\tools\tool-manifest.json"
  RMDir "$INSTDIR\tools\windows-x64"
  RMDir "$INSTDIR\tools"
  File "${ROOT}\dist\windows\app\codex-elves-remote.exe"
  ; 本机后台及固定工具已内嵌客户端；安装目录只放客户端和卸载程序。

  Delete "$DESKTOP\CodexElves 绠＄悊宸ュ叿.lnk"
  Delete "$DESKTOP\CodexElves Manager.lnk"
  Delete "$DESKTOP\CodexElves.lnk"
  Delete "$DESKTOP\CodexElves 管理工具.lnk"
  Delete "$SMPROGRAMS\CodexElves\CodexElves 绠＄悊宸ュ叿.lnk"
  Delete "$SMPROGRAMS\CodexElves\鍗歌浇 CodexElves.lnk"
  Delete "$SMPROGRAMS\CodexElves Manager.lnk"
  Delete "$SMPROGRAMS\CodexElves\CodexElves.lnk"
  Delete "$SMPROGRAMS\CodexElves\CodexElves 管理工具.lnk"

  ; 用户要求为独立程序创建快捷方式：桌面与开始菜单均指向客户端自身，不经管理器启动。
  CreateShortcut "$DESKTOP\CodexElves 远程会话.lnk" "$INSTDIR\codex-elves-remote.exe" "" "$INSTDIR\codex-elves-remote.exe"
  CreateDirectory "$SMPROGRAMS\CodexElves"
  CreateShortcut "$SMPROGRAMS\CodexElves\CodexElves 远程会话.lnk" "$INSTDIR\codex-elves-remote.exe" "" "$INSTDIR\codex-elves-remote.exe"
  CreateShortcut "$SMPROGRAMS\CodexElves\卸载 CodexElves.lnk" "$INSTDIR\uninstall.exe" "" "$INSTDIR\codex-elves-remote.exe"

  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "Software\CodexElves" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\CodexElves" "DisplayName" "CodexElves"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\CodexElves" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\CodexElves" "Publisher" "junxin367"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\CodexElves" "DisplayIcon" "$INSTDIR\codex-elves-remote.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\CodexElves" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\CodexElves" "UninstallString" "$INSTDIR\uninstall.exe"

SectionEnd

Section "Uninstall"
  ; 卸载也先走受控退出；只回收安全空闲 job，不删除原生历史、草稿或用户数据。
  nsExec::ExecToLog '"$INSTDIR\codex-elves-remote.exe" --prepare-remote-shutdown'
  Pop $0
  nsExec::ExecToLog 'taskkill /IM codex-elves.exe /F'
  Pop $0
  nsExec::ExecToLog 'taskkill /IM codex-elves-manager.exe /F'
  Pop $0
  nsExec::ExecToLog 'taskkill /IM codex-elves-task-board.exe /F'
  Pop $0
  nsExec::ExecToLog 'taskkill /IM codex-elves-remote.exe /F'
  Pop $0

  Delete "$DESKTOP\CodexElves.lnk"
  Delete "$DESKTOP\CodexElves 管理工具.lnk"
  Delete "$DESKTOP\CodexElves 远程会话.lnk"
  Delete "$DESKTOP\CodexElves Manager.lnk"
  Delete "$DESKTOP\CodexElves 绠＄悊宸ュ叿.lnk"
  Delete "$SMPROGRAMS\CodexElves\CodexElves.lnk"
  Delete "$SMPROGRAMS\CodexElves\CodexElves 管理工具.lnk"
  Delete "$SMPROGRAMS\CodexElves\CodexElves 远程会话.lnk"
  Delete "$SMPROGRAMS\CodexElves\CodexElves 绠＄悊宸ュ叿.lnk"
  Delete "$SMPROGRAMS\CodexElves\卸载 CodexElves.lnk"
  Delete "$SMPROGRAMS\CodexElves\鍗歌浇 CodexElves.lnk"
  Delete "$SMPROGRAMS\CodexElves Manager.lnk"
  RMDir "$SMPROGRAMS\CodexElves"

  Delete "$INSTDIR\codex-elves.exe"
  Delete "$INSTDIR\codex-elves-manager.exe"
  Delete "$INSTDIR\codex-elves-task-board.exe"
  ; 只移除程序；远程客户端设置和登录缓存位于独立用户数据目录，升级及卸载不在此清除。
  Delete "$INSTDIR\codex-elves-remote.exe"
  RMDir /r "$INSTDIR\tools"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\CodexElves"
  DeleteRegKey HKCU "Software\CodexElves"
SectionEnd
