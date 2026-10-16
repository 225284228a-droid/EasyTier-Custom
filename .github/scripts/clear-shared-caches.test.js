const assert = require('node:assert/strict');
const { test } = require('node:test');
const clear = require('./clear-shared-caches.js');

function fixture() {
  const repository = { id: 1, full_name: 'owner/repo' };
  const current = {
    id: 123, workflow_id: 10, path: '.github/workflows/build.yml',
    repository, head_repository: repository, status: 'in_progress',
    created_at: '2026-10-16T12:00:00Z',
  };
  const old = { ...current, status: 'completed', conclusion: 'success', created_at: '2026-10-15T00:00:00Z' };
  const runs = {
    123: current, 122: { ...old, id: 122 },
    120: { ...old, id: 120, status: 'in_progress' },
    119: { ...old, id: 119, workflow_id: 11, path: '.github/workflows/checks.yml' },
    118: { ...old, id: 118, head_repository: { id: 2, full_name: 'fork/repo' } },
    117: { ...old, id: 117, created_at: '2026-10-17T00:00:00Z' },
    116: { ...old, id: 116, status: 'queued' },
    115: { ...old, id: 115, repository: { ...repository, id: 3 }, head_repository: { ...repository, id: 3 } },
  };
  const caches = [
    { id: 1, key: 'easytier-shared-frontend-122', ref: 'refs/heads/main' },
    { id: 2, key: 'easytier-shared-tests-122', ref: 'refs/heads/main' },
    { id: 3, key: 'easytier-shared-frontend-123', ref: 'refs/heads/main' },
    { id: 4, key: 'easytier-shared-frontend-124', ref: 'refs/heads/main' },
    { id: 5, key: 'easytier-shared-frontend-122', ref: 'refs/pull/7/merge' },
    { id: 6, key: 'easytier-shared-frontend-122-suffix', ref: 'refs/heads/main' },
    { id: 7, key: 'rust-target-cache', ref: 'refs/heads/main' },
    { id: -1, key: 'easytier-shared-frontend-122', ref: 'refs/heads/main' },
    ...[120, 119, 118, 117, 116, 115, 114].map(id => ({
      id, key: `easytier-shared-frontend-${id}`, ref: 'refs/heads/main',
    })),
  ];
  const deleted = [];
  const reads = [];
  const context = { runId: 123, ref: 'refs/heads/main', repo: { owner: 'owner', repo: 'repo' } };
  const actions = {
    getActionsCacheList: 'caches',
    async getWorkflowRun({ run_id }) {
      reads.push(run_id);
      if (!runs[run_id]) throw Object.assign(new Error('not found'), { status: 404 });
      return { data: runs[run_id] };
    },
    async deleteActionsCacheById({ cache_id }) { deleted.push(cache_id); },
  };
  return { deleted, reads, runs, args: { context, core: { info() {} }, github: {
    rest: { actions },
    async paginate(method, request) {
      assert.equal(method, 'caches');
      assert.deepEqual(request, { ...context.repo, key: 'easytier-shared-', ref: context.ref, per_page: 100 });
      // Deliberately include prefix/identity collisions to exercise exact matching.
      return caches;
    },
  } } };
}

test('new success deletes only older completed transfer caches in the same workflow and ref', async () => {
  const f = fixture();
  await clear(f.args);
  assert.deepEqual(f.deleted, [1, 2]);
  assert.equal(f.reads.filter(id => id === 122).length, 1);
  assert.ok(!f.reads.includes(124));
});

test('cache cleanup keeps the current run available for failed-job reruns', async () => {
  const f = fixture();
  f.runs[123].status = 'completed';
  f.runs[123].conclusion = 'success';
  await clear(f.args);
  assert.ok(!f.deleted.includes(3));
});

test('cache cleanup refuses invalid current identities and unsuccessful runs', async () => {
  for (const changes of [{ runId: 0 }, { ref: '' }]) {
    const f = fixture();
    Object.assign(f.args.context, changes);
    await assert.rejects(clear(f.args), /valid run/);
    assert.deepEqual(f.deleted, []);
  }
  for (const changes of [
    { status: 'completed', conclusion: 'failure' }, { path: '.github/workflows/release.yml' },
    { head_repository: { id: 2, full_name: 'attacker/repo' } }, { workflow_id: 0 },
    { created_at: '' },
  ]) {
    const f = fixture();
    Object.assign(f.runs[123], changes);
    await assert.rejects(clear(f.args), /successful build or CI/);
    assert.deepEqual(f.deleted, []);
  }
});

test('cache eviction races are harmless but other API failures remain visible', async () => {
  for (const status of [404, 403]) {
    const f = fixture();
    f.args.github.rest.actions.deleteActionsCacheById = async () => {
      throw Object.assign(new Error('API failure'), { status });
    };
    if (status === 404) await clear(f.args);
    else await assert.rejects(clear(f.args), /API failure/);
  }
});
