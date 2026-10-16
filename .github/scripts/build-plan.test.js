const assert = require('node:assert/strict');
const { test } = require('node:test');
const { coreTargets, guiTargets, androidArchs, buildPlan } = require('./build-config.js');
const prepare = require('./prepare-build.js');
const { expectedJobs, checkResults } = require('./check-build-results.js');

test('the public build matrix retains desktop targets and builds Android ARM64 only', () => {
  assert.equal(coreTargets.length, 16);
  assert.equal(guiTargets.length, 5);
  assert.equal(new Set(coreTargets.map(t => t.target)).size, 16);
  assert.equal(new Set(guiTargets.map(t => t.target)).size, 5);
  assert.deepEqual(androidArchs, ['aarch64']);
  assert.deepEqual(guiTargets.map(t => t.name).sort(), [
    'macos-aarch64', 'macos-x86_64', 'windows-arm64', 'windows-i686', 'windows-x86_64',
  ]);
  assert.ok(coreTargets.some(t => t.target === 'mips-unknown-linux-musl'));
  assert.ok(coreTargets.some(t => t.target === 'mipsel-unknown-linux-musl'));
  assert.ok(coreTargets.some(t => t.target === 'x86_64-unknown-freebsd'));
  assert.ok([...coreTargets, ...guiTargets].every(t => !/ohos|magisk/.test(t.target)));
});

test('a main push always builds full artifacts, even for documentation changes', () => {
  const plan = buildPlan({ eventName: 'push', ref: 'refs/heads/main', changedFiles: ['README.md'] });
  assert.equal(plan.full, true);
  assert.equal(plan.rust, true);
  assert.equal(plan.frontend, true);
  assert.equal(plan.go, false);
  assert.equal(plan.js, false);
  assert.equal(plan.nix, false);
});

test('manual runs on any branch build every platform and run all checks', () => {
  const plan = buildPlan({ eventName: 'workflow_dispatch', ref: 'refs/heads/feature' });
  assert.ok(Object.values(plan).every(Boolean));
});

test('PR and develop code changes run checks without production builds', () => {
  for (const [eventName, ref] of [['pull_request', 'refs/pull/42/merge'], ['push', 'refs/heads/develop']]) {
    const plan = buildPlan({ eventName, ref, changedFiles: ['easytier-core/src/config/mod.rs'] });
    assert.equal(plan.full, false);
    assert.equal(plan.rust, true);
    assert.equal(plan.frontend, true);
    assert.equal(plan.js, true);
    assert.equal(plan.go, false);
  }
});

test('unrelated PR changes skip expensive checks', () => {
  const plan = buildPlan({ eventName: 'pull_request', changedFiles: ['README.md'] });
  assert.equal(plan.ci, true);
  for (const key of ['full', 'rust', 'frontend', 'go', 'js', 'nix']) assert.equal(plan[key], false);
});

test('draft PRs and legacy scheduled triggers do not run builds or checks', () => {
  for (const context of [
    { eventName: 'pull_request', draft: true },
    { eventName: 'schedule', ref: 'refs/heads/main' },
    { eventName: 'push', ref: 'refs/heads/releases/2.7' },
  ]) {
    const plan = buildPlan({ ...context, changedFiles: ['.github/workflows/build.yml'] });
    assert.ok(Object.values(plan).every(value => value === false));
  }
});

test('Go, JavaScript and Nix checks retain independent path filters', () => {
  for (const [file, enabled] of [
    ['easytier-go/client.go', ['go']],
    ['easytier-js/runtime/src/index.ts', ['js']],
    ['nix/dev-shell.nix', ['nix']],
    ['flake.lock', ['nix']],
    ['easytier-proto/proto/api_manage.proto', ['go', 'js']],
    ['.github/tauri-ci.json', ['go', 'js', 'nix']],
  ]) {
    const plan = buildPlan({ eventName: 'pull_request', changedFiles: [file] });
    for (const key of ['go', 'js', 'nix']) assert.equal(plan[key], enabled.includes(key), `${file}: ${key}`);
  }
});

async function prepared(context, files = []) {
  const outputs = {};
  await prepare({
    context,
    core: { setOutput(key, value) { outputs[key] = value; }, info() {} },
    github: {
      rest: { pulls: { listFiles: 'files' } },
      async paginate(method, args) {
        assert.equal(method, 'files');
        assert.equal(args.pull_number, 42);
        return files;
      },
    },
  });
  return outputs;
}

test('preparation exports the same full matrices consumed by release validation', async () => {
  const outputs = await prepared({ eventName: 'workflow_dispatch', ref: 'refs/heads/main', payload: {} });
  assert.equal(outputs.full, 'true');
  assert.deepEqual(JSON.parse(outputs.core_matrix).include, coreTargets);
  assert.deepEqual(JSON.parse(outputs.gui_matrix).include, guiTargets);
  assert.deepEqual(JSON.parse(outputs.android_matrix).arch, androidArchs);
});

test('preparation checks both sides of renamed PR files and uses ARM64-only Android checks', async () => {
  const outputs = await prepared({
    eventName: 'pull_request', ref: 'refs/pull/42/merge',
    payload: { pull_request: { number: 42, draft: false } }, repo: { owner: 'owner', repo: 'repo' },
  }, [{ filename: 'archive/old.ts', previous_filename: 'easytier-js/runtime/src/old.ts' }]);
  assert.equal(outputs.js, 'true');
  assert.equal(outputs.full, 'false');
  assert.deepEqual(JSON.parse(outputs.android_matrix), { arch: ['aarch64'] });
});

test('an incomplete PR file listing conservatively enables every check', async () => {
  const outputs = await prepared({
    eventName: 'pull_request', ref: 'refs/pull/42/merge',
    payload: { pull_request: { number: 42, draft: false, changed_files: 3001 } },
    repo: { owner: 'owner', repo: 'repo' },
  }, [{ filename: 'README.md' }]);
  for (const key of ['rust', 'frontend', 'go', 'js', 'nix']) assert.equal(outputs[key], 'true');
  assert.equal(outputs.full, 'false');
});

function results(plan) {
  const outputs = Object.fromEntries(Object.entries(plan).map(([key, value]) => [key, String(value)]));
  return {
    prepare: { result: 'success', outputs },
    ...Object.fromEntries(Object.entries(expectedJobs(outputs)).map(([job, required]) =>
      [job, { result: required ? 'success' : 'skipped' }])),
  };
}

test('aggregate accepts successful full builds, lightweight checks, unrelated changes and draft skips', () => {
  for (const context of [
    { eventName: 'push', ref: 'refs/heads/main' },
    { eventName: 'pull_request', changedFiles: ['easytier/src/lib.rs'] },
    { eventName: 'pull_request', changedFiles: ['README.md'] },
    { eventName: 'pull_request', draft: true },
  ]) assert.doesNotThrow(() => checkResults(results(buildPlan(context))));
});

test('aggregate blocks cleanup for failed, cancelled, unexpectedly skipped or missing planned jobs', () => {
  const full = buildPlan({ eventName: 'push', ref: 'refs/heads/main' });
  for (const job of Object.keys(expectedJobs(results(full).prepare.outputs))) {
    if (!expectedJobs(results(full).prepare.outputs)[job]) continue;
    for (const status of ['failure', 'cancelled', 'skipped', undefined]) {
      const needs = results(full);
      needs[job].result = status;
      assert.throws(() => checkResults(needs), new RegExp(job));
    }
  }
  const needs = results(full);
  needs.prepare.result = 'failure';
  assert.throws(() => checkResults(needs), /preparation/);
});

test('aggregate requires the Android check even when no APK is being packaged', () => {
  const needs = results(buildPlan({ eventName: 'pull_request', changedFiles: ['easytier/src/lib.rs'] }));
  needs.android.result = 'skipped';
  assert.throws(() => checkResults(needs), /android/);
});
