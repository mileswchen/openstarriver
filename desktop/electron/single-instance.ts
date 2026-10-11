// Single-instance guard. Before this existed, a second launch of ANY copy of
// the app (double-click, another build in dist, the DMG) ran a full second
// instance: two identical dock icons, both contending for the same starriverd
// and the same SQLite WALs. The lock keys on the userData dir, which every
// copy shares (no setName/setPath overrides anywhere), so it holds across
// copies: the loser exits, and 'second-instance' surfaces the winner's window.
import { app, BrowserWindow } from 'electron';

export function acquireSingleInstanceLock(): boolean {
  const gotTheLock = app.requestSingleInstanceLock();
  if (!gotTheLock) return false;
  // Someone launched us again: raise the existing window instead of sitting
  // silently in the background (the second process exits on its own).
  app.on('second-instance', () => {
    for (const win of BrowserWindow.getAllWindows()) {
      if (win.isDestroyed()) continue;
      win.show();
      win.focus();
    }
  });
  return true;
}
