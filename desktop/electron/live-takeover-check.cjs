// 活体验证：对 39617 上的陈旧守护进程（daemon.json 无 build_id，记录 pid 已死）
// 执行 build_id 接管，然后 lan.state 必须从 METHOD_NOT_FOUND 变为真实 JSON。
const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawn } = require('child_process');
const {
  DaemonClient,
  daemonInvoke,
  bundledDaemonBuildId,
  stopDaemonAndCleanup,
} = require('./daemon.js');

const dataDir = path.join(os.homedir(), '.starriver');
const infoPath = path.join(dataDir, 'daemon.json');
const binary = '/home/miles/Projects/starriver/target/debug/starriverd';
const readInfo = () => JSON.parse(fs.readFileSync(infoPath, 'utf8'));

(async () => {
  const old = readInfo();
  console.log('[before] daemon.json:', JSON.stringify(old));
  console.log('[before] bundled build_id:', bundledDaemonBuildId(binary));
  try {
    const r = await daemonInvoke(old, 'lan.state', {});
    console.log('[before] lan.state OK（意外）:', JSON.stringify(r).slice(0, 200));
  } catch (e) {
    console.log('[before] lan.state:', e.message || String(e));
  }

  const client = new DaemonClient({
    binaryPath: () => binary,
    spawnFn: spawn,
    dataDir,
    infoPath,
  });
  await client.connect();
  const info = client.currentInfo();
  const fresh = readInfo();
  console.log('[after] daemon.json:', JSON.stringify(fresh));
  console.log(
    '[after] build_id match:',
    info.build_id === bundledDaemonBuildId(binary),
    `(${info.build_id} vs ${bundledDaemonBuildId(binary)})`,
  );
  try {
    const r2 = await daemonInvoke(fresh, 'lan.state', {});
    console.log('[after] lan.state:', JSON.stringify(r2).slice(0, 300));
  } catch (e) {
    console.log('[after] lan.state FAIL:', e.message || String(e));
    process.exitCode = 1;
  }
  client.dispose();
  // 验证完不留幸存者：停掉守护进程（否则本进程组的常驻子进程会让调用方等不到退出）。
  const stop = await stopDaemonAndCleanup({ dataDir, infoPath, binaryPath: binary });
  console.log('[cleanup] stopDaemonAndCleanup:', JSON.stringify(stop));
})().catch((e) => {
  console.error('FAIL:', e.message || e);
  process.exit(1);
});
