const assert = require('node:assert/strict');
const { test } = require('node:test');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const { createHash } = require('node:crypto');
const { execFileSync } = require('node:child_process');
const { checksums, archive, singleNative, recordDownload } = require('./package-build.js');
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
  const output = await archive('easytier-linux-x86_64', directory, path.join(root, 'dist'), commit);
  const extracted = path.join(root, 'extracted');
  await fs.mkdir(extracted);
  execFileSync('tar', ['-xf', output, '-C', extracted]);
  assert.equal(await fs.readFile(path.join(extracted, 'easytier-cli'), 'utf8'), '#!/bin/sh\nexit 0\n');
  assert.equal(await fs.readFile(path.join(extracted, 'COMMIT.txt'), 'utf8'), `${commit}\n`);
  if (process.platform !== 'win32') assert.equal((await fs.stat(path.join(extracted, 'easytier-cli'))).mode & 0o777, 0o755);
  assert.match(await fs.readFile(path.join(extracted, 'SHA256SUMS'), 'utf8'), /  easytier-cli\n/);
});

test('native downloads preserve installer bytes and contain only the named file', async t => {
  for (const extension of ['exe', 'dmg', 'apk']) {
    const { root, directory } = await fixture(t);
    const data = Buffer.from([0, 1, 2, 255, 50, 34]);
    await fs.writeFile(path.join(directory, `upstream-name.${extension}`), data);
    const name = `easytier-native-x86_64.${extension}`;
    const destination = path.join(root, 'dist');
    const output = await singleNative(name, directory, destination, commit);
    assert.deepEqual(await fs.readdir(destination), [name]);
    assert.deepEqual(await fs.readFile(output), data);
  }
});

test('native uploads refuse empty, ambiguous, wrong-type or unsafe files', async t => {
  const { root, directory } = await fixture(t);
  const destination = path.join(root, 'dist');
  const build = (name = 'easytier-android.apk') => singleNative(name, directory, destination, commit);
  await assert.rejects(build(), /exactly one/);
  await fs.writeFile(path.join(directory, 'installer.apk'), '');
  await assert.rejects(build(), /Empty/);
  await fs.writeFile(path.join(directory, 'installer.apk'), 'apk');
  await assert.rejects(build('../installer.apk'), /Invalid/);
  await assert.rejects(build('installer.zip'), /Invalid/);
  await assert.rejects(build('installer.exe'), /exactly one/);
  await assert.rejects(singleNative('installer.apk', directory, destination, 'main'), /commit/);
  await fs.writeFile(path.join(directory, 'extra.apk'), 'second ABI');
  await assert.rejects(build(), /exactly one/);
  await assert.rejects(fs.stat(destination), { code: 'ENOENT' });
});

test('native staging never overwrites a previous output', async t => {
  const { root, directory } = await fixture(t);
  await fs.writeFile(path.join(directory, 'installer.apk'), 'first');
  const destination = path.join(root, 'dist');
  const output = await singleNative('easytier.apk', directory, destination, commit);
  await fs.writeFile(path.join(directory, 'installer.apk'), 'second');
  await assert.rejects(singleNative('easytier.apk', directory, destination, commit), { code: 'EEXIST' });
  assert.equal(await fs.readFile(output, 'utf8'), 'first');
});

test('download outputs record the exact file and its checksum without creating extra artifacts', async t => {
  const { root, directory } = await fixture(t);
  const file = path.join(directory, 'installer.apk');
  await fs.writeFile(file, 'APK bytes');
  const previous = { output: process.env.GITHUB_OUTPUT, summary: process.env.GITHUB_STEP_SUMMARY };
  const output = path.join(root, 'outputs');
  const summary = path.join(root, 'summary');
  process.env.GITHUB_OUTPUT = output;
  process.env.GITHUB_STEP_SUMMARY = summary;
  try {
    const digest = await recordDownload(file, commit);
    assert.equal(digest, createHash('sha256').update('APK bytes').digest('hex'));
    assert.equal(await fs.readFile(output, 'utf8'), `path=${file}\n`);
    assert.match(await fs.readFile(summary, 'utf8'), new RegExp(digest));
    assert.match(await fs.readFile(summary, 'utf8'), new RegExp(commit));
    assert.deepEqual(await fs.readdir(directory), ['installer.apk']);
  } finally {
    for (const [key, value] of [['GITHUB_OUTPUT', previous.output], ['GITHUB_STEP_SUMMARY', previous.summary]]) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
  }
});
