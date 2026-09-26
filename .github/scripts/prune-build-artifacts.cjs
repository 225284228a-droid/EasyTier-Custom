// Only remove completed, older builds on main after the full replacement succeeds.
module.exports = async ({ github, context, core }) => {
  const repo = context.repo;
  const current = (await github.rest.actions.getWorkflowRun({
    ...repo, run_id: context.runId,
  })).data;
  const buildPaths = new Set([
    '.github/workflows/custom-static.yml',
    '.github/workflows/core.yml',
    '.github/workflows/gui.yml',
    '.github/workflows/mobile.yml',
    '.github/workflows/ohos.yml',
  ]);
  const workflows = await github.paginate(github.rest.actions.listRepoWorkflows, {
    ...repo, per_page: 100,
  });
  const buildIds = new Set(workflows.filter(w => buildPaths.has(w.path)).map(w => w.id));
  // Collect before deleting so pagination cannot skip artifacts as pages shrink.
  const artifacts = await github.paginate(github.rest.actions.listArtifactsForRepo, {
    ...repo, per_page: 100,
  });
  const runs = new Map();
  for (const artifact of artifacts) {
    const id = artifact.workflow_run?.id;
    if (!id || id === context.runId) continue;
    if (!runs.has(id)) {
      runs.set(id, (await github.rest.actions.getWorkflowRun({ ...repo, run_id: id })).data);
    }
    const run = runs.get(id);
    if (!buildIds.has(run.workflow_id) || run.head_branch !== 'main' ||
        !['push', 'workflow_dispatch'].includes(run.event) ||
        run.status !== 'completed' ||
        !(Date.parse(run.created_at) < Date.parse(current.created_at))) continue;
    core.info(`Deleting old build artifact ${artifact.name} (${artifact.id})`);
    await github.rest.actions.deleteArtifact({ ...repo, artifact_id: artifact.id });
  }
};
