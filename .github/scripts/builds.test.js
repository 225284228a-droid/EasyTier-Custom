const assert = require('node:assert/strict');
const { test } = require('node:test');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const { createHash } = require('node:crypto');
const validate = require('./validate-release-builds.js');
const { expectedArtifacts, prepareReleaseAssets } = validate;

const commit = 'a'.repeat(40);
const repository = { id: 1, full_name: 'EasyTier/EasyTier' };
const context = { repo: { owner: 'EasyTier', repo: 'EasyTier' } };
const now = Date.parse('2026-10-16T00:00:00Z');
const sha256 = value => createHash('sha256').update(value).digest('hex');

function fixture() {
  const workflow = { id: 12, path: '.github/workflows/build.yml', name: 'EasyTier Build' };
  const run = {
    id: 123, workflow_id: 12, path: workflow.path, status: 'completed', conclusion: 'success',
    event: 'push', head_branch: 'main', head_sha: commit, repository, head_repository: repository,
  };
  const artifacts = expectedArtifacts().map(({ name }, index) => ({
    id: index + 1, name, expired: false, expires_at: '2026-12-01T00:00:00Z', size_in_bytes: 100,
    workflow_run: { id: run.id, repository_id: 1, head_repository_id: 1, head_sha: commit },
  }));
  const outputs = {};
  const actions = {
    async getWorkflow(request) {
      assert.equal(request.workflow_id, 'build.yml');
      return { data: workflow };
    },
    async getWorkflowRun(request) {
      assert.equal(request.run_id, 123);
      return { data: run };
    },
    listWorkflowRunArtifacts: 'artifacts',
  };
  const args = {
    github: {
      rest: { actions },
      async paginate(method, request) {
        assert.equal(method, 'artifacts');
        assert.deepEqual(request, { ...context.repo, run_id: 123, per_page: 100 });
        return artifacts;
      },
    },
    context, core: { info() {}, setOutput(name, value) { outputs[name] = value; } },
    now, runId: '123',
  };
  return { args, run, artifacts, workflow, outputs };
}

test('release allowlist has exactly 16 Core, five desktop GUI, Android ARM64, and Web artifacts', () => {
  const expected = expectedArtifacts();
  assert.equal(expected.length, 23);
  assert.equal(expected.filter(a => a.name.startsWith('easytier-gui-')).length, 5);
  assert.deepEqual(expected.filter(a => a.name.startsWith('easytier-mobile-android-')).map(a => a.name),
    ['easytier-mobile-android-aarch64']);
  assert.equal(expected.filter(a => a.extension === '.zip').length, 3);
  assert.equal(new Set(expected.map(a => a.name)).size, expected.length);
  assert.ok(expected.every(a => !/ohos|magisk|gui-linux/i.test(a.name)));
});

test('release accepts completed full main pushes and selects only required artifact IDs', async () => {
  const f = fixture();
  for (const name of ['frontend-assets', 'tests', 'signpath-unsigned-windows-x86_64',
    'signpath-signed-windows-x86_64', 'unknown-extra']) {
    f.artifacts.push({ id: f.artifacts.length + 1, name });
  }
  const result = await validate(f.args);
  assert.deepEqual(result.artifactIds, Array.from({ length: 23 }, (_, i) => i + 1));
  assert.equal(f.outputs.artifact_ids, result.artifactIds.join(','));
  assert.equal(f.outputs.build_sha, commit);
});

test('release accepts manual full builds from any branch', async () => {
  const f = fixture();
  f.run.event = 'workflow_dispatch';
  f.run.head_branch = 'develop';
  await validate(f.args);
});

test('release rejects develop pushes, PRs, and legacy release-branch pushes', async () => {
  for (const [event, branch] of [['push', 'develop'], ['pull_request', 'main'], ['push', 'releases/2.7']]) {
    const f = fixture();
    f.run.event = event;
    f.run.head_branch = branch;
    await assert.rejects(validate(f.args), /successful completed full/);
    assert.deepEqual(f.outputs, {});
  }
});

test('release rejects unsuccessful, unfinished, or wrong-workflow runs', async () => {
  for (const changes of [
    { conclusion: 'failure' }, { conclusion: 'cancelled' }, { status: 'in_progress' },
    { path: '.github/workflows/core.yml' }, { workflow_id: 11 }, { id: 124 },
  ]) {
    const f = fixture();
    Object.assign(f.run, changes);
    await assert.rejects(validate(f.args), /successful completed full/);
  }
  const f = fixture();
  f.workflow.path = '.github/workflows/renamed.yml';
  await assert.rejects(validate(f.args), /successful completed full/);
});

test('release checks repository identity, fork provenance, and commit', async () => {
  for (const changes of [
    { repository: { ...repository, full_name: 'attacker/EasyTier' } },
    { head_repository: { ...repository, full_name: 'attacker/EasyTier' } },
    { head_repository: { ...repository, id: 2 } }, { head_repository: null },
    { head_sha: '' },
  ]) {
    const f = fixture();
    Object.assign(f.run, changes);
    await assert.rejects(validate(f.args), /this repository/);
  }
});

test('release rejects invalid run inputs before making API calls', async () => {
  for (const runId of ['', 0, -1, '1.5', '1e2', '9007199254740992', undefined]) {
    const f = fixture();
    f.args.runId = runId;
    await assert.rejects(validate(f.args), /positive integer/);
  }
});

test('release requires every artifact and rejects duplicate ambiguity even if one copy expired', async () => {
  const missing = fixture();
  missing.artifacts.pop();
  await assert.rejects(validate(missing.args), /exactly one easytier-web-dashboard artifact, found 0/);
  const duplicate = fixture();
  duplicate.artifacts.push({ ...duplicate.artifacts[0], id: 999, expired: true });
  await assert.rejects(validate(duplicate.args), /exactly one easytier-linux-x86_64 artifact, found 2/);
});

test('release rejects expired, empty, or wrongly associated artifacts', async () => {
  for (const changes of [
    { expired: true }, { expires_at: '2026-10-16T00:00:00Z' }, { expires_at: 'invalid' },
    { size_in_bytes: 0 }, { workflow_run: null }, { workflow_run: { id: 999 } },
    { workflow_run: { id: 123, repository_id: 99 } },
    { workflow_run: { id: 123, head_repository_id: 99 } },
    { workflow_run: { id: 123, head_sha: 'b'.repeat(40) } },
  ]) {
    const f = fixture();
    Object.assign(f.artifacts[0], changes);
    await assert.rejects(validate(f.args), /expired, empty, or has invalid provenance/);
  }
});

test('release rejects reused artifact IDs', async () => {
  const f = fixture();
  f.artifacts[1].id = f.artifacts[0].id;
  await assert.rejects(validate(f.args), /ambiguous artifact IDs/);
});

async function downloaded(t) {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), 'easytier-release-'));
  t.after(() => fs.rm(dir, { recursive: true, force: true }));
  const sourceDir = path.join(dir, 'downloaded');
  await fs.mkdir(sourceDir);
  for (const artifact of expectedArtifacts()) {
    const target = path.join(sourceDir, artifact.name);
    await fs.mkdir(target);
    const filename = `${artifact.name}${artifact.extension}`;
    const content = Buffer.from(`native or prebuilt bytes: ${filename}\n`);
    await fs.writeFile(path.join(target, filename), content);
    if (artifact.kind === 'native') {
      await fs.writeFile(path.join(target, 'COMMIT.txt'), `${commit}\n`);
      await fs.writeFile(path.join(target, 'SHA256SUMS'),
        `${sha256(content)}  ${filename}\n${sha256(`${commit}\n`)}  COMMIT.txt\n`);
    }
  }
  return { sourceDir, destinationDir: path.join(dir, 'assets'), commit };
}

test('release preserves archive/installer bytes and creates combined commit/checksum files', async t => {
  const args = await downloaded(t);
  const names = await prepareReleaseAssets(args);
  assert.equal(names.length, expectedArtifacts().length + 2);
  for (const artifact of expectedArtifacts()) {
    const name = `${artifact.name}${artifact.extension}`;
    assert.deepEqual(await fs.readFile(path.join(args.destinationDir, name)),
      await fs.readFile(path.join(args.sourceDir, artifact.name, name)));
  }
  assert.equal(await fs.readFile(path.join(args.destinationDir, 'COMMIT.txt'), 'utf8'), `${commit}\n`);
  const checksums = await fs.readFile(path.join(args.destinationDir, 'SHA256SUMS'), 'utf8');
  assert.equal(checksums.trim().split('\n').length, expectedArtifacts().length + 1);
  for (const line of checksums.trim().split('\n')) {
    const [digest, name] = line.split('  ');
    assert.equal(digest, sha256(await fs.readFile(path.join(args.destinationDir, name))));
  }
});

test('release preparation refuses extra internal artifacts and creates no partial output', async t => {
  const args = await downloaded(t);
  await fs.mkdir(path.join(args.sourceDir, 'frontend-assets'));
  await assert.rejects(prepareReleaseAssets(args), /allowlist/);
  await assert.rejects(fs.stat(args.destinationDir), { code: 'ENOENT' });
});

test('release preparation refuses raw binaries in place of ready-made archives', async t => {
  const args = await downloaded(t);
  const dir = path.join(args.sourceDir, 'easytier-linux-x86_64');
  await fs.rename(path.join(dir, 'easytier-linux-x86_64.tar.gz'), path.join(dir, 'easytier-core'));
  await assert.rejects(prepareReleaseAssets(args), /ready-made .tar.gz/);
});

test('release preparation rejects a native installer from another commit', async t => {
  const args = await downloaded(t);
  await fs.writeFile(path.join(args.sourceDir, 'easytier-gui-macos-x86_64', 'COMMIT.txt'), 'b'.repeat(40));
  await assert.rejects(prepareReleaseAssets(args), /different commit/);
});

test('release preparation detects native installer corruption', async t => {
  const args = await downloaded(t);
  const native = expectedArtifacts().find(a => a.kind === 'native');
  await fs.appendFile(path.join(args.sourceDir, native.name, native.name + native.extension), 'changed');
  await assert.rejects(prepareReleaseAssets(args), /checksum mismatch/);
});

test('release preparation rejects missing or ambiguous checksums', async t => {
  for (const manifest of ['', `${'0'.repeat(64)}  unknown.exe\n`]) {
    const args = await downloaded(t);
    const native = expectedArtifacts().find(a => a.kind === 'native');
    await fs.writeFile(path.join(args.sourceDir, native.name, 'SHA256SUMS'), manifest);
    await assert.rejects(prepareReleaseAssets(args), /checksum manifest/);
  }
});

test('release preparation refuses conflicting native asset filenames', async t => {
  const args = await downloaded(t);
  const native = expectedArtifacts().filter(a => a.extension === '.exe');
  const filename = 'same-installer.exe';
  for (const artifact of native) {
    const dir = path.join(args.sourceDir, artifact.name);
    const content = await fs.readFile(path.join(dir, artifact.name + artifact.extension));
    await fs.rename(path.join(dir, artifact.name + artifact.extension), path.join(dir, filename));
    await fs.writeFile(path.join(dir, 'SHA256SUMS'),
      `${sha256(content)}  ${filename}\n${sha256(`${commit}\n`)}  COMMIT.txt\n`);
  }
  await assert.rejects(prepareReleaseAssets(args), /duplicate filename/);
});

test('release preparation refuses to overwrite a prior destination', async t => {
  const args = await downloaded(t);
  await fs.mkdir(args.destinationDir);
  await fs.writeFile(path.join(args.destinationDir, 'existing'), 'keep');
  await assert.rejects(prepareReleaseAssets(args), { code: 'EEXIST' });
  assert.equal(await fs.readFile(path.join(args.destinationDir, 'existing'), 'utf8'), 'keep');
});
