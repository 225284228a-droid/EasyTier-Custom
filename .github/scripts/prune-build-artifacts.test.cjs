const assert = require('node:assert/strict');
const test = require('node:test');
const prune = require('./prune-build-artifacts.cjs');
const { expectedArtifacts } = require('./validate-release-builds.js');
const eligible = [10, 15, 16, 18, 19, 20, 21, 22];

function fixture() {
  const repository = { id: 1, full_name: 'owner/repo' };
  const workflow = { id: 7, path: '.github/workflows/build.yml' };
  const current = {
    id: 100, workflow_id: 7, path: workflow.path, head_branch: 'main', event: 'push',
    status: 'in_progress', conclusion: null, repository, head_repository: repository,
    created_at: '2026-10-16T12:00:00Z', head_sha: 'a'.repeat(40),
  };
  const old = {
    ...current, status: 'completed', conclusion: 'success', created_at: '2026-10-15T12:00:00Z',
  };
  const runs = {
    100: current,
    10: { ...old, id: 10 },
    11: { ...old, id: 11, status: 'in_progress' },
    12: { ...old, id: 12, head_branch: 'develop' },
    13: { ...old, id: 13, event: 'pull_request' },
    14: { ...old, id: 14, created_at: '2026-10-16T13:00:00Z' },
    15: { ...old, id: 15, workflow_id: 8, path: '.github/workflows/core.yml' },
    16: { ...old, id: 16, event: 'workflow_dispatch' },
    17: { ...old, id: 17, event: 'workflow_dispatch', head_branch: 'feature' },
    18: { ...old, id: 18, path: '.github/workflows/custom-static.yml' },
    19: { ...old, id: 19, conclusion: 'failure' },
    20: { ...old, id: 20, workflow_id: 20, path: '.github/workflows/gui.yml' },
    21: { ...old, id: 21, workflow_id: 21, path: '.github/workflows/mobile.yml' },
    22: { ...old, id: 22, workflow_id: 22, path: '.github/workflows/ohos.yml' },
    23: { ...old, id: 23, workflow_id: 23, path: '.github/workflows/test.yml' },
    24: { ...old, id: 24, workflow_id: 24, path: '.github/workflows/release.yml' },
    25: { ...old, id: 25, workflow_id: 25 },
    26: { ...old, id: 26, path: undefined },
    101: { ...old, id: 101 }, // A newer ID is preserved even if its timestamp looks old.
  };
  const artifacts = Object.keys(runs).map(id => ({
    id: Number(id), name: `artifact-${id}`, workflow_run: { id: Number(id) },
  }));
  artifacts.push({ id: 999, name: 'unassociated' });
  const deleted = [];
  const reads = [];
  const currentArtifacts = expectedArtifacts().map(({ name }, index) => ({
    id: 1000 + index, name, expired: false, expires_at: '2026-12-01T00:00:00Z', size_in_bytes: 100,
    workflow_run: { id: 100, repository_id: 1, head_repository_id: 1, head_sha: current.head_sha },
  }));
  let listed = false;
  const actions = {
    async getWorkflow({ workflow_id }) {
      assert.equal(workflow_id, 'build.yml');
      return { data: workflow };
    },
    listArtifactsForRepo: 'artifacts',
    listWorkflowRunArtifacts: 'current-artifacts',
    async getWorkflowRun({ run_id }) {
      reads.push(run_id);
      if (!runs[run_id]) throw Object.assign(new Error('missing'), { status: 404 });
      return { data: runs[run_id] };
    },
    async deleteArtifact({ artifact_id }) {
      assert.ok(listed, 'complete pagination before deleting');
      deleted.push(artifact_id);
    },
  };
  const args = {
    context: { repo: { owner: 'owner', repo: 'repo' }, runId: 100 },
    now: Date.parse('2026-10-16T12:00:00Z'),
    core: { info() {} },
    github: {
      rest: { actions },
      async paginate(method, request) {
        if (method === 'current-artifacts') {
          assert.deepEqual(request, { owner: 'owner', repo: 'repo', run_id: 100, per_page: 100 });
          return currentArtifacts;
        }
        assert.equal(method, 'artifacts');
        assert.deepEqual(request, { owner: 'owner', repo: 'repo', per_page: 100 });
        listed = true;
        return artifacts;
      },
    },
  };
  return { args, artifacts, currentArtifacts, deleted, reads, runs, workflow };
}

test('cleanup removes older completed main full builds and the bounded legacy migration allowlist', async () => {
  const f = fixture();
  await prune(f.args);
  assert.deepEqual(f.deleted, eligible);
  assert.ok(!f.reads.includes(101), 'newer runs are never inspected for deletion');
  assert.equal(f.reads.filter(id => id === 100).length, 1, 'preserve current run artifacts');
});

test('cleanup reads each older run once despite multiple artifacts', async () => {
  const f = fixture();
  f.artifacts.push({ id: 110, name: 'second-platform', workflow_run: { id: 10 } });
  await prune(f.args);
  assert.deepEqual(f.deleted, [...eligible, 110]);
  assert.equal(f.reads.filter(id => id === 10).length, 1);
});

test('cleanup preserves legacy PR, develop, running, newer, or unidentified runs', async () => {
  const f = fixture();
  Object.assign(f.runs[15], { head_branch: 'develop' });
  Object.assign(f.runs[18], { event: 'pull_request' });
  Object.assign(f.runs[20], { status: 'in_progress' });
  Object.assign(f.runs[21], { created_at: '2026-10-17T00:00:00Z' });
  Object.assign(f.runs[22], { workflow_id: undefined });
  await prune(f.args);
  assert.deepEqual(f.deleted, [10, 16, 19]);
});

test('cleanup rejects a non-main, non-full, foreign, or wrong-workflow current run before listing', async () => {
  for (const changes of [
    { head_branch: 'develop' }, { head_branch: 'feature', event: 'workflow_dispatch' },
    { event: 'pull_request' }, { workflow_id: 8 }, { path: '.github/workflows/core.yml' },
    { created_at: 'invalid' }, { id: 99 }, { status: 'queued' },
    { head_sha: 'invalid' },
    { status: 'completed', conclusion: 'failure' },
    { repository: { id: 1, full_name: 'foreign/repo' } },
    { head_repository: { id: 2, full_name: 'owner/repo' } },
    { head_repository: null },
  ]) {
    const f = fixture();
    Object.assign(f.runs[100], changes);
    await assert.rejects(prune(f.args), /current main full build.yml run/);
    assert.deepEqual(f.deleted, []);
    assert.deepEqual(f.reads, [100]);
  }
});

test('cleanup permits a manual main current run and a successful completed current run', async () => {
  const f = fixture();
  Object.assign(f.runs[100], { event: 'workflow_dispatch', status: 'completed', conclusion: 'success' });
  await prune(f.args);
  assert.deepEqual(f.deleted, eligible);
});

test('cleanup rejects a workflow renamed away from build.yml', async () => {
  const f = fixture();
  f.workflow.path = '.github/workflows/moved.yml';
  await assert.rejects(prune(f.args), /current main full build.yml run/);
  assert.deepEqual(f.deleted, []);
});

test('cleanup preserves equally new and invalidly dated runs', async () => {
  const f = fixture();
  for (const id of eligible) f.runs[id].created_at = id % 2 ? f.runs[100].created_at : 'invalid';
  await prune(f.args);
  assert.deepEqual(f.deleted, []);
});

test('cleanup preserves unknown and unavailable run associations', async () => {
  const f = fixture();
  f.artifacts.push({ id: 98, name: 'deleted-run', workflow_run: { id: 98 } });
  f.artifacts.push({ id: 97, name: 'deleted-run-copy', workflow_run: { id: 98 } });
  await prune(f.args);
  assert.deepEqual(f.deleted, eligible);
  assert.equal(f.reads.filter(id => id === 98).length, 1);
});

test('cleanup preserves artifacts whose embedded repository or commit conflicts with their run', async () => {
  const f = fixture();
  for (const id of eligible) {
    const association = f.artifacts.find(a => a.id === id).workflow_run;
    Object.assign(association, id % 3 === 0 ? { repository_id: 99 }
      : id % 3 === 1 ? { head_repository_id: 99 } : { head_sha: 'b'.repeat(40) });
  }
  await prune(f.args);
  assert.deepEqual(f.deleted, []);
});

test('cleanup preserves older runs with foreign repository provenance', async () => {
  const f = fixture();
  for (const id of eligible) {
    Object.assign(f.runs[id], id % 3 === 0 ? { repository: { id: 1, full_name: 'foreign/repo' } }
      : id % 3 === 1 ? { head_repository: { id: 2, full_name: 'owner/repo' } } : { id: 99 });
  }
  await prune(f.args);
  assert.deepEqual(f.deleted, []);
});

test('cleanup stops on API errors other than an unknown run', async () => {
  const f = fixture();
  const getRun = f.args.github.rest.actions.getWorkflowRun;
  f.args.github.rest.actions.getWorkflowRun = request => {
    if (request.run_id === 10) throw Object.assign(new Error('rate limited'), { status: 403 });
    return getRun(request);
  };
  await assert.rejects(prune(f.args), /rate limited/);
  assert.deepEqual(f.deleted, []);
});

test('cleanup requires every new product before removing older usable products', async () => {
  const f = fixture();
  f.currentArtifacts.pop();
  await assert.rejects(prune(f.args), /exactly one complete, valid easytier-mobile-android-aarch64/);
  assert.deepEqual(f.deleted, []);
  assert.deepEqual(f.reads, [100]);
});

test('cleanup refuses duplicate, expired, empty, or mismatched current product artifacts', async () => {
  for (const changes of [
    { id: 0 }, { expired: true }, { expires_at: 'invalid' },
    { expires_at: '2026-10-16T12:00:00Z' }, { size_in_bytes: 0 },
    { workflow_run: undefined }, { workflow_run: { id: 99 } },
    { workflow_run: { id: 100, repository_id: 2 } },
    { workflow_run: { id: 100, head_repository_id: 2 } },
    { workflow_run: { id: 100, head_sha: 'b'.repeat(40) } },
  ]) {
    const f = fixture();
    Object.assign(f.currentArtifacts[0], changes);
    await assert.rejects(prune(f.args), /exactly one complete, valid/);
    assert.deepEqual(f.deleted, []);
  }
  const duplicate = fixture();
  duplicate.currentArtifacts.push({ ...duplicate.currentArtifacts[0], id: 9999 });
  await assert.rejects(prune(duplicate.args), /exactly one complete, valid/);
  assert.deepEqual(duplicate.deleted, []);
  const reused = fixture();
  reused.currentArtifacts[1].id = reused.currentArtifacts[0].id;
  await assert.rejects(prune(reused.args), /distinct current build artifact IDs/);
  assert.deepEqual(reused.deleted, []);
});

test('cleanup retires legacy architectures, Web, Magisk, HarmonyOS and static build archives', async () => {
  const f = fixture();
  for (const [id, name] of [[10, 'easytier-linux-aarch64'], [15, 'Easytier-Magisk'],
    [16, 'easytier-web-dashboard'], [18, 'custom-static-windows-x86_64.zip'], [20, 'easytier-gui-windows-i686'],
    [21, 'easytier-mobile-android-armv7'], [22, 'easytier-ohrs']]) {
    f.artifacts.find(artifact => artifact.id === id).name = name;
  }
  await prune(f.args);
  assert.deepEqual(f.deleted, eligible);
});
