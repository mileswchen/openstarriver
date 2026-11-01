; Starriver NSIS additions — included via electron-builder.yml win.nsis.include.
;
; Windows 的卸载器只感知 GUI 主进程；starriverd.exe 是 Electron spawn 的后台
; 守护进程，且运行中的 exe 被内核锁定——不在删文件前停掉它，卸载会留下
; resources\starriverd.exe 文件尸体 + 存活进程。
; 对齐 Docker Desktop 的系统卸载入口语义：卸载器负责停后台组件。

!macro customUnInstall
  ; 1) 优雅优先：--shutdown 走控制通道（一行 JSON 协议，跨版本稳定），
  ;    配对客户端看到的是干净退出而非进程死亡。
  DetailPrint "$(starriverStoppingDaemon)"
  nsExec::Exec '"$INSTDIR\resources\starriverd.exe" --shutdown'

  ; 2) 等进程真正退出（25 × 200ms ≈ 5s），否则文件仍被锁定删不掉。
  StrCpy $R9 0
  starriver_wait_graceful:
    ${nsProcess::FindProcess} "starriverd.exe" $R8
    IntCmp $R8 0 0 starriver_killed starriver_killed ; !=0 → 已退出
    IntOp $R9 $R9 + 1
    IntCmp $R9 25 0 0 starriver_killed
    Sleep 200
    Goto starriver_wait_graceful

  starriver_killed:
  ; 3) 兜底：按映像名强杀（覆盖从其他安装路径启动的守护进程）。
    ${nsProcess::FindProcess} "starriverd.exe" $R8
    IntCmp $R8 0 0 starriver_done starriver_done
    ${nsProcess::KillProcess} "starriverd.exe" $R8
    Sleep 500

  starriver_done:
    ${nsProcess::Unload}
!macroend

LangString starriverStoppingDaemon ${LANG_ENGLISH} "Stopping the Starriver background service..."
LangString starriverStoppingDaemon ${LANG_SIMPCHINESE} "正在停止 Starriver 后台服务..."
