#!/usr/bin/env node
// inject-deb-control.mjs — 把 build/deb/{prerm,postrm} 注入 fpm 产出的 .deb 控制段。
//
// 为什么需要：electron-builder 只暴露 afterInstall/afterRemove（→ postinst/postrm），
// 没有 prerm 钩子；而优雅停掉运行中的 starriverd 必须发生在 dpkg 删除文件之前
// （prerm），postrm 时包内二进制已经删除。见 desktop/electron/build/deb/prerm 头注。
//
// 纯 node 实现 ar / ustar / gzip 读写，打包农场（mac/linux）无需 binutils/dpkg。
// 幂等：重复运行安全（同名条目直接替换）。
//
// usage: node inject-deb-control.mjs <deb> <prerm> <postrm>

import { readFileSync, writeFileSync } from 'node:fs';
import { gzipSync, gunzipSync } from 'node:zlib';

const [debPath, prermPath, postrmPath] = process.argv.slice(2);
if (!debPath || !prermPath || !postrmPath) {
  console.error('usage: inject-deb-control.mjs <deb> <prerm> <postrm>');
  process.exit(2);
}

// ── ar archive ──────────────────────────────────────────────────────────────

function parseAr(data) {
  if (data.subarray(0, 8).toString('latin1') !== '!<arch>\n') {
    throw new Error(`${debPath}: not an ar archive`);
  }
  const members = [];
  let off = 8;
  while (off + 60 <= data.length) {
    const header = data.subarray(off, off + 60);
    if (header.toString('latin1', 58, 60) !== '`\n') {
      throw new Error(`${debPath}: bad ar member magic at offset ${off}`);
    }
    const name = header.toString('latin1', 0, 16).trimEnd().replace(/\/$/, '');
    const size = Number.parseInt(header.toString('latin1', 48, 58).trim(), 10);
    const start = off + 60;
    members.push({ name, data: data.subarray(start, start + size) });
    off = start + size + (size % 2);
  }
  return members;
}

function arHeader(name, size) {
  // GNU ar 风格：成员名带结尾 '/'（与 fpm 产出一致），mtime/uid/gid 归零（确定性构建）。
  // 字段布局：name 0..16 | mtime 16..28 | uid 28..34 | gid 34..40 | mode 40..48 | size 48..58 | magic 58..60。
  const header = Buffer.alloc(60, 0x20);
  header.write(`${name}/`, 0, 'latin1');
  header.write('0', 16, 'latin1');
  header.write('0', 28, 'latin1');
  header.write('0', 34, 'latin1');
  header.write('100644', 40, 'latin1');
  header.write(String(size), 48, 'latin1');
  header.write('`\n', 58, 'latin1');
  return header;
}

function buildAr(members) {
  const parts = [Buffer.from('!<arch>\n', 'latin1')];
  for (const member of members) {
    parts.push(arHeader(member.name, member.data.length), member.data);
    if (member.data.length % 2) parts.push(Buffer.from('\n', 'latin1'));
  }
  return Buffer.concat(parts);
}

// ── ustar ───────────────────────────────────────────────────────────────────

const BLOCK = 512;

function parseTar(data) {
  const entries = [];
  let off = 0;
  while (off + BLOCK <= data.length) {
    const block = data.subarray(off, off + BLOCK);
    if (block.every((byte) => byte === 0)) break;
    const name = block.toString('utf8', 0, 100).replace(/\0[\s\S]*$/, '');
    const size = Number.parseInt(
      block.toString('latin1', 124, 136).replace(/\0[\s\S]*$/, '').trim() || '0',
      8,
    );
    const rawType = String.fromCharCode(block[156] || 0x30);
    const start = off + BLOCK;
    entries.push({
      name,
      type: rawType === '\0' ? '0' : rawType,
      mode: Number.parseInt(block.toString('latin1', 100, 108).replace(/\0[\s\S]*$/, '').trim() || '644', 8),
      data: rawType === '0' || rawType === '\0' ? Buffer.from(data.subarray(start, start + size)) : Buffer.alloc(0),
    });
    off = start + Math.ceil(size / BLOCK) * BLOCK;
  }
  return entries;
}

const oct = (value, width) => `${value.toString(8).padStart(width - 1, '0')}\0`;

function tarHeader(entry) {
  const block = Buffer.alloc(BLOCK, 0);
  block.write(entry.name, 0, 'utf8');
  block.write(oct(entry.mode ?? 0o644, 8), 100, 'latin1');
  block.write(oct(0, 8), 108, 'latin1');
  block.write(oct(0, 8), 116, 'latin1');
  block.write(oct(entry.data.length, 12), 124, 'latin1');
  block.write(oct(0, 12), 136, 'latin1');
  block.write('        ', 148, 'latin1'); // checksum placeholder（全空格参与求和）
  block.write(entry.type, 156, 'latin1');
  block.write('ustar\0', 257, 'latin1');
  block.write('00', 263, 'latin1');
  let sum = 0;
  for (const byte of block) sum += byte;
  block.write(`${sum.toString(8).padStart(6, '0')}\0 `, 148, 'latin1');
  return block;
}

function buildTar(entries) {
  const parts = [];
  for (const entry of entries) {
    parts.push(tarHeader(entry), entry.data);
    const pad = (BLOCK - (entry.data.length % BLOCK)) % BLOCK;
    if (pad) parts.push(Buffer.alloc(pad));
  }
  parts.push(Buffer.alloc(BLOCK * 2)); // 结束零块
  return Buffer.concat(parts);
}

// ── 注入 ────────────────────────────────────────────────────────────────────

const members = parseAr(readFileSync(debPath));
const control = members.find((m) => /^control\.tar\.(gz|xz|zst)$/.test(m.name));
if (!control) throw new Error(`${debPath}: control.tar.* member not found`);
if (!control.name.endsWith('.gz')) {
  throw new Error(`${debPath}: unsupported control compression (${control.name}) — extend this script`);
}

const entries = parseTar(gunzipSync(control.data));
const scripts = [
  { name: './prerm', path: prermPath },
  { name: './postrm', path: postrmPath },
];
const injected = entries.filter((entry) => !scripts.some((script) => script.name === entry.name));
for (const script of scripts) {
  injected.push({ name: script.name, type: '0', mode: 0o755, data: readFileSync(script.path) });
}

const patched = buildAr(
  members.map((member) =>
    member === control ? { name: 'control.tar.gz', data: gzipSync(buildTar(injected)) } : member,
  ),
);
writeFileSync(debPath, patched);
console.log(
  `[deb-inject] ${debPath}: control entries now ${injected.map((entry) => entry.name).join(', ')}`,
);
