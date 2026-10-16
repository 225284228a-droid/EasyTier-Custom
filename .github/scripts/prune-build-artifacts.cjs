const BUILD_PATH = '.github/workflows/build.yml';
// Bounded migration for artifacts left by the retired split build workflows.
const LEGACY_BUILD_PATHS = new Set([
  '.github/workflows/core.yml',
  '.github/workflows/gui.yml',
  '.github/workflows/mobile.yml',
  '.github/workflows/custom-static.yml',
  '.github/workflows/ohos.yml',
]);

// The workflow calls this only after every required full-build job has succeeded.
module.exports = async ({ github, context, core }) => {
  const repo = context.repo;
  const sameRepository = repository => repository?.full_name?.toLowerCase() ===
    `${repo.owner}/${repo.repo}`.toLowerCase();
  const mainFullBuild = run => run.head_branch === 'main' &&
    ['push', 'workflow_dispatch'].includes(run.event);
  const { data: workflow } = await github.rest.actions.getWorkflow({ ...repo, workflow_id: 'build.yml' });
  const { data: current } = await github.rest.actions.getWorkflowRun({ ...repo, run_id: context.runId });
  const currentTime = Date.parse(current.created_at);
  if (!Number.isSafeInteger(workflow.id) || workflow.id <= 0 || workflow.path !== BUILD_PATH ||
      !Number.isSafeInteger(current.id) || current.id <= 0 || current.id !== context.runId ||
      current.workflow_id !== workflow.id || current.path !== BUILD_PATH || !mainFullBuild(current) ||
      !sameRepository(current.repository) || !sameRepository(current.head_repository) ||
      !Number.isSafeInteger(current.repository?.id) || current.repository.id <= 0 ||
      current.head_repository?.id !== current.repository.id || !Number.isFinite(currentTime) ||
      !['in_progress', 'completed'].includes(current.status) ||
      (current.status === 'completed' && current.conclusion !== 'success')) {
    throw new Error('Artifact cleanup requires this repository\'s current main full build.yml run.');
  }
  // Collect before deleting so pagination cannot skip artifacts as pages shrink.
  const artifacts = await github.paginate(github.rest.actions.listArtifactsForRepo, { ...repo, per_page: 100 });
  const runs = new Map();
  for (const artifact of artifacts) {
    const association = artifact.workflow_run;
    const id = association?.id;
    if (!Number.isSafeInteger(artifact.id) || artifact.id <= 0 ||
        !Number.isSafeInteger(id) || id <= 0 || id >= context.runId ||
        (association.repository_id != null && association.repository_id !== current.repository.id) ||
        (association.head_repository_id != null && association.head_repository_id !== current.repository.id)) continue;
    if (!runs.has(id)) {
      try {
        runs.set(id, (await github.rest.actions.getWorkflowRun({ ...repo, run_id: id })).data);
      } catch (error) {
        // Deleted/unavailable runs cannot establish that these artifacts are safe to remove.
        if (error.status !== 404) throw error;
        runs.set(id, null);
      }
    }
    const run = runs.get(id);
    const knownBuildWorkflow = run && (run.path === BUILD_PATH
      ? run.workflow_id === current.workflow_id
      : LEGACY_BUILD_PATHS.has(run.path) && Number.isSafeInteger(run.workflow_id) && run.workflow_id > 0);
    if (!knownBuildWorkflow || run.id !== id || !mainFullBuild(run) ||
        !sameRepository(run.repository) || !sameRepository(run.head_repository) ||
        run.repository.id !== current.repository.id || run.head_repository.id !== current.repository.id ||
        run.status !== 'completed' || !(Date.parse(run.created_at) < currentTime) ||
        (association.head_sha != null && association.head_sha !== run.head_sha)) continue;
    core.info(`Deleting older full build artifact ${artifact.name} (${artifact.id}) from run ${id}.`);
    await github.rest.actions.deleteArtifact({ ...repo, artifact_id: artifact.id });
  }
};
