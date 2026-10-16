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
    digest: `sha256:${sha256(name)}`,
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

test('release allowlist has three x86-64 Core, two desktop GUI, and Android ARM64 artifacts', () => {
  const expected = expectedArtifacts();
  assert.equal(expected.length, 6);
  assert.deepEqual(expected.filter(a => a.kind === 'archive').map(a => a.name),
    ['easytier-linux-x86_64.tar.gz', 'easytier-macos-x86_64.tar.gz', 'easytier-windows-x86_64.zip']);
  assert.deepEqual(expected.filter(a => a.name.startsWith('easytier-gui-')).map(a => a.name),
    ['easytier-gui-macos-x86_64.dmg', 'easytier-gui-windows-x86_64.exe']);
  assert.deepEqual(expected.filter(a => a.name.startsWith('easytier-mobile-android-')).map(a => a.name),
    ['easytier-mobile-android-aarch64.apk']);
  assert.ok(expected.every(a => a.filename === a.name && a.name.endsWith(a.extension)));
  assert.equal(expected.filter(a => a.extension === '.zip').length, 1);
  assert.equal(new Set(expected.map(a => a.name)).size, expected.length);
  assert.ok(expected.every(a => !/ohos|magisk|freebsd|gui-linux|web-dashboard/i.test(a.name)));
});

test('release accepts completed full main pushes and selects only required artifact IDs', async () => {
  const f = fixture();
  for (const name of ['frontend-assets', 'tests', 'signpath-unsigned-windows-x86_64',
    'signpath-signed-windows-x86_64', 'easytier-web-dashboard', 'unknown-extra']) {
    f.artifacts.push({ id: f.artifacts.length + 1, name });
  }
  const result = await validate(f.args);
  assert.deepEqual(result.artifactIds, Array.from({ length: 6 }, (_, i) => i + 1));
  assert.equal(f.outputs.artifact_ids, result.artifactIds.join(','));
  assert.equal(f.outputs.build_sha, commit);
  const metadata = f.artifacts.slice(0, 6).map(({ name, digest }) => ({ name, digest }));
  assert.deepEqual(result.artifactMetadata, metadata);
  assert.deepEqual(JSON.parse(f.outputs.artifact_metadata), metadata);
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
  await assert.rejects(validate(missing.args), /exactly one easytier-mobile-android-aarch64.apk artifact, found 0/);
  const duplicate = fixture();
  duplicate.artifacts.push({ ...duplicate.artifacts[0], id: 999, expired: true });
  await assert.rejects(validate(duplicate.args), /exactly one easytier-linux-x86_64.tar.gz artifact, found 2/);
});

test('release rejects expired, empty, or wrongly associated artifacts', async () => {
  for (const changes of [
    { expired: true }, { expires_at: '2026-10-16T00:00:00Z' }, { expires_at: 'invalid' },
    { size_in_bytes: 0 }, { size_in_bytes: Infinity }, { size_in_bytes: '100' },
    { workflow_run: null }, { workflow_run: { id: 999 } },
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

test('release requires a valid API SHA256 digest for every raw artifact before emitting outputs', async () => {
  for (const digest of [undefined, null, '', '0'.repeat(64), `md5:${'0'.repeat(64)}`,
    `sha256:${'0'.repeat(63)}`, `sha256:${'g'.repeat(64)}`, `sha256:${'A'.repeat(64)}`]) {
    const f = fixture();
    f.artifacts.at(-1).digest = digest;
    await assert.rejects(validate(f.args), /valid API SHA256 digest/);
    assert.deepEqual(f.outputs, {});
  }
});

test('release rejects legacy product names that omit the file extension', async () => {
  const f = fixture();
  f.artifacts[0].name = 'easytier-linux-x86_64';
  await assert.rejects(validate(f.args), /expected exactly one easytier-linux-x86_64.tar.gz/);
  assert.deepEqual(f.outputs, {});
});

async function downloaded(t) {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), 'easytier-release-'));
  t.after(() => fs.rm(dir, { recursive: true, force: true }));
  const sourceDir = path.join(dir, 'downloaded');
  await fs.mkdir(sourceDir);
  const artifacts = [];
  for (const artifact of expectedArtifacts()) {
    const target = path.join(sourceDir, artifact.name);
    await fs.mkdir(target);
    const filename = artifact.filename;
    const content = Buffer.from(`native or prebuilt bytes: ${filename}\n`);
    await fs.writeFile(path.join(target, filename), content);
    artifacts.push({ name: artifact.name, digest: `sha256:${sha256(content)}` });
  }
  return { sourceDir, destinationDir: path.join(dir, 'assets'), commit, artifacts };
}

test('release preserves archive/installer bytes and creates combined commit/checksum files', async t => {
  const args = await downloaded(t);
  const names = await prepareReleaseAssets(args);
  assert.equal(names.length, expectedArtifacts().length + 2);
  assert.ok(names.every(name => !name.startsWith('easytier-web-dashboard')));
  for (const artifact of expectedArtifacts()) {
    const name = artifact.filename;
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

test('release preparation refuses internal or retired Web artifacts and creates no partial output', async t => {
  for (const name of ['frontend-assets', 'easytier-web-dashboard']) {
    const args = await downloaded(t);
    await fs.mkdir(path.join(args.sourceDir, name));
    await assert.rejects(prepareReleaseAssets(args), /allowlist/);
    await assert.rejects(fs.stat(args.destinationDir), { code: 'ENOENT' });
  }
});

test('release preparation requires precisely the six same-named product directories', async t => {
  for (const change of ['missing', 'legacy-name', 'top-level-file']) {
    const args = await downloaded(t);
    const first = expectedArtifacts()[0];
    const dir = path.join(args.sourceDir, first.name);
    if (change === 'missing') await fs.rm(dir, { recursive: true });
    if (change === 'legacy-name') await fs.rename(dir, path.join(args.sourceDir, 'easytier-linux-x86_64'));
    if (change === 'top-level-file') {
      await fs.rm(dir, { recursive: true });
      await fs.writeFile(dir, 'downloaded file in the wrong directory');
    }
    await assert.rejects(prepareReleaseAssets(args), /allowlist/);
    await assert.rejects(fs.stat(args.destinationDir), { code: 'ENOENT' });
  }
});

test('release preparation requires a single same-named file in each raw product artifact', async t => {
  for (const artifact of expectedArtifacts()) {
    for (const change of ['missing', 'wrong-name', 'extra-installer', 'commit', 'checksums', 'nested-directory']) {
      const args = await downloaded(t);
      const dir = path.join(args.sourceDir, artifact.name);
      const file = path.join(dir, artifact.filename);
      if (change === 'missing') await fs.unlink(file);
      if (change === 'wrong-name') await fs.rename(file, path.join(dir, 'unexpected' + artifact.extension));
      if (change === 'extra-installer') await fs.writeFile(path.join(dir, 'second' + artifact.extension), 'extra');
      if (change === 'commit') await fs.writeFile(path.join(dir, 'COMMIT.txt'), `${commit}\n`);
      if (change === 'checksums') await fs.writeFile(path.join(dir, 'SHA256SUMS'), 'extra manifest');
      if (change === 'nested-directory') {
        await fs.unlink(file);
        await fs.mkdir(file);
      }
      await assert.rejects(prepareReleaseAssets(args), /only its same-named product file/);
      await assert.rejects(fs.stat(args.destinationDir), { code: 'ENOENT' });
    }
  }
});

test('release preparation rejects empty or corrupted archive and installer bytes', async t => {
  for (const artifact of expectedArtifacts()) {
    for (const content of ['', 'changed download']) {
      const args = await downloaded(t);
      await fs.writeFile(path.join(args.sourceDir, artifact.name, artifact.filename), content);
      await assert.rejects(prepareReleaseAssets(args), content ? /checksum mismatch/ : /is empty/);
      await assert.rejects(fs.stat(args.destinationDir), { code: 'ENOENT' });
    }
  }
});

test('release preparation requires complete unique validated API digest metadata', async t => {
  const mutations = [
    () => undefined, () => null, () => ({}), () => [],
    entries => entries.slice(1), entries => [...entries, entries[0]],
    entries => [...entries.slice(0, -1), entries[0]],
    entries => [{ ...entries[0], name: 'frontend-assets' }, ...entries.slice(1)],
    entries => [{ name: entries[0].name }, ...entries.slice(1)],
    entries => [{ ...entries[0], digest: 'sha256:invalid' }, ...entries.slice(1)],
    entries => [{ ...entries[0], digest: `md5:${'0'.repeat(64)}` }, ...entries.slice(1)],
    entries => [{ ...entries[0], id: 1 }, ...entries.slice(1)],
    entries => [null, ...entries.slice(1)],
  ];
  for (const mutate of mutations) {
    const args = await downloaded(t);
    args.artifacts = mutate(args.artifacts);
    await assert.rejects(prepareReleaseAssets(args), /Validated artifact metadata/);
    await assert.rejects(fs.stat(args.destinationDir), { code: 'ENOENT' });
  }
});

test('release preparation compares against API digests and accepts metadata in any order', async t => {
  const rejected = await downloaded(t);
  rejected.artifacts[0].digest = `sha256:${'0'.repeat(64)}`;
  await assert.rejects(prepareReleaseAssets(rejected), /checksum mismatch with the validated API artifact digest/);
  await assert.rejects(fs.stat(rejected.destinationDir), { code: 'ENOENT' });
  const accepted = await downloaded(t);
  accepted.artifacts.reverse();
  assert.equal((await prepareReleaseAssets(accepted)).length, 8);
});

test('release preparation requires a validated build commit', async t => {
  const args = await downloaded(t);
  args.commit = 'invalid';
  await assert.rejects(prepareReleaseAssets(args), /validated build commit/);
  await assert.rejects(fs.stat(args.destinationDir), { code: 'ENOENT' });
});

test('release preparation refuses to overwrite a prior destination', async t => {
  const args = await downloaded(t);
  await fs.mkdir(args.destinationDir);
  await fs.writeFile(path.join(args.destinationDir, 'existing'), 'keep');
  await assert.rejects(prepareReleaseAssets(args), { code: 'EEXIST' });
  assert.equal(await fs.readFile(path.join(args.destinationDir, 'existing'), 'utf8'), 'keep');
});
