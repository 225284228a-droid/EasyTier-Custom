const assert = require('node:assert/strict');
const { test } = require('node:test');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const { createHash } = require('node:crypto');
const { execFileSync } = require('node:child_process');
const { checksums, archive } = require('./package-build.js');
const commit = 'a'.repeat(40);

async function fixture(t) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'easytier-package-'));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  const directory = path.join(root, 'artifacts');
  await fs.mkdir(directory);
  return { root, directory };
}

test('native manifests include shipped installers, driver files and the source commit exactly once', async t => {
  const { directory } = await fixture(t);
  const files = { 'installer.exe': 'installer', 'wintun.dll': 'driver', 'WinDivert64.sys': 'kernel' };
  for (const [name, data] of Object.entries(files)) await fs.writeFile(path.join(directory, name), data);
  await checksums(directory, commit);
  await checksums(directory, commit);
  files['COMMIT.txt'] = `${commit}\n`;
  assert.equal(await fs.readFile(path.join(directory, 'SHA256SUMS'), 'utf8'), Object.keys(files).sort()
    .map(name => `${createHash('sha256').update(files[name]).digest('hex')}  ${name}\n`).join(''));
});

test('packaging refuses empty output, nested directories and invalid commits', async t => {
  const { directory } = await fixture(t);
  await assert.rejects(checksums(directory, commit), /nonempty/);
  await fs.writeFile(path.join(directory, 'program'), 'binary');
  await assert.rejects(checksums(directory, 'main'), /commit/);
  await fs.mkdir(path.join(directory, 'nested'));
  await assert.rejects(checksums(directory, commit), /flat/);
});

test('downloadable archives round-trip executables and checksum manifests', async t => {
  const { root, directory } = await fixture(t);
  await fs.writeFile(path.join(directory, 'easytier-cli'), '#!/bin/sh\nexit 0\n', { mode: 0o755 });
  const output = await archive('easytier-freebsd-13.2-x86_64', directory, path.join(root, 'dist'), commit);
  const extracted = path.join(root, 'extracted');
  await fs.mkdir(extracted);
  execFileSync('tar', ['-xf', output, '-C', extracted]);
  assert.equal(await fs.readFile(path.join(extracted, 'easytier-cli'), 'utf8'), '#!/bin/sh\nexit 0\n');
  assert.equal(await fs.readFile(path.join(extracted, 'COMMIT.txt'), 'utf8'), `${commit}\n`);
  if (process.platform !== 'win32') assert.equal((await fs.stat(path.join(extracted, 'easytier-cli'))).mode & 0o777, 0o755);
  assert.match(await fs.readFile(path.join(extracted, 'SHA256SUMS'), 'utf8'), /  easytier-cli\n/);
});
