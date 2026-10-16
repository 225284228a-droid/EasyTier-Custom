// Successful runs retire earlier transfer caches, while keeping their own cache
// available to rerun jobs. No cache from another workflow or active run is removed.
module.exports = async ({ github, context, core }) => {
  if (!Number.isSafeInteger(context.runId) || context.runId <= 0 || !context.ref) {
    throw new Error('A valid run and ref are required to clear shared caches');
  }
  const actions = github.rest.actions;
  const { data: current } = await actions.getWorkflowRun({ ...context.repo, run_id: context.runId });
  const sameRepository = run => [run?.repository, run?.head_repository].every(repository =>
    repository?.full_name?.toLowerCase() === `${context.repo.owner}/${context.repo.repo}`.toLowerCase()) &&
    Number.isSafeInteger(run.repository?.id) && run.repository.id > 0 &&
    run.head_repository?.id === run.repository.id;
  if (current.id !== context.runId || !sameRepository(current) ||
      !['.github/workflows/build.yml', '.github/workflows/checks.yml'].includes(current.path) ||
      !Number.isSafeInteger(current.workflow_id) || current.workflow_id <= 0 ||
      !Number.isFinite(Date.parse(current.created_at)) ||
      !['in_progress', 'completed'].includes(current.status) ||
      (current.status === 'completed' && current.conclusion !== 'success')) {
    throw new Error('Shared cache cleanup requires a successful build or CI run in this repository');
  }
  const caches = await github.paginate(actions.getActionsCacheList, {
    ...context.repo, key: 'easytier-shared-', ref: context.ref, per_page: 100,
  });
  const runs = new Map();
  for (const cache of caches) {
    const match = /^easytier-shared-(frontend|tests)-([1-9]\d*)$/.exec(cache.key);
    const id = Number(match?.[2]);
    if (!match || !Number.isSafeInteger(id) || id >= current.id || cache.ref !== context.ref ||
        !Number.isSafeInteger(cache.id) || cache.id <= 0) continue;
    if (!runs.has(id)) {
      try {
        runs.set(id, (await actions.getWorkflowRun({ ...context.repo, run_id: id })).data);
      } catch (error) {
        if (error.status !== 404) throw error;
        runs.set(id, null);
      }
    }
    const run = runs.get(id);
    if (!run || run.id !== id || !sameRepository(run) || run.repository.id !== current.repository.id ||
        run.workflow_id !== current.workflow_id || run.path !== current.path || run.status !== 'completed' ||
        !(Date.parse(run.created_at) < Date.parse(current.created_at))) continue;
    try {
      await actions.deleteActionsCacheById({ ...context.repo, cache_id: cache.id });
      core.info(`Removed older internal resource cache ${cache.key}`);
    } catch (error) {
      if (error.status !== 404) throw error;
    }
  }
};
