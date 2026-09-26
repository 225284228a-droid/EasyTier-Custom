const assert = require('node:assert/strict');
const test = require('node:test');
const prune = require('./prune-build-artifacts.cjs');

function fixture() {
  const current = { created_at: '2026-09-26T12:00:00Z' };
  const old = {
    workflow_id: 1, head_branch: 'main', event: 'push', status: 'completed',
    created_at: '2026-09-25T12:00:00Z',
  };
  const runs = {
    100: current,
    10: old,
    11: { ...old, status: 'in_progress' },
    12: { ...old, head_branch: 'feature' },
    13: { ...old, event: 'pull_request' },
    14: { ...old, created_at: '2026-09-26T13:00:00Z' },
    15: { ...old, workflow_id: 3 },
    16: { ...old, workflow_id: 2, event: 'workflow_dispatch' },
  };
  const artifacts = Object.keys(runs).map(id => ({
    id: Number(id), name: `artifact-${id}`, workflow_run: { id: Number(id) },
  }));
  artifacts.push({ id: 99, name: 'unassociated' });
  const deleted = [];
  const reads = [];
  let listed = false;
  const actions = {
    listRepoWorkflows: 'workflows',
    listArtifactsForRepo: 'artifacts',
    getWorkflowRun: async ({ run_id }) => {
      reads.push(run_id);
      return { data: runs[run_id] };
    },
    deleteArtifact: async ({ artifact_id }) => {
      assert.ok(listed, 'complete pagination before deleting');
      deleted.push(artifact_id);
    },
  };
  const args = {
    context: { repo: { owner: 'owner', repo: 'repo' }, runId: 100 },
    core: { info() {} },
    github: {
      rest: { actions },
      paginate: async method => {
        if (method === 'workflows') return [
          { id: 1, path: '.github/workflows/custom-static.yml' },
          { id: 2, path: '.github/workflows/core.yml' },
          { id: 3, path: '.github/workflows/test.yml' },
        ];
        listed = true;
        return artifacts;
      },
    },
  };
  return { args, artifacts, deleted, reads, runs };
}

test('only deletes older completed main build artifacts, including legacy builds', async () => {
  const f = fixture();
  await prune(f.args);
  assert.deepEqual(f.deleted, [10, 16]);
});

test('reads a run once even when it contains several artifacts', async () => {
  const f = fixture();
  f.artifacts.push({ id: 101, name: 'second-platform', workflow_run: { id: 10 } });
  await prune(f.args);
  assert.deepEqual(f.deleted, [10, 16, 101]);
  assert.equal(f.reads.filter(id => id === 10).length, 1);
});

test('does not delete equally new or invalidly dated runs', async () => {
  const f = fixture();
  f.runs[10].created_at = f.runs[100].created_at;
  f.runs[16].created_at = 'invalid';
  await prune(f.args);
  assert.deepEqual(f.deleted, []);
});
