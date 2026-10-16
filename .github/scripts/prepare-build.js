const { execFileSync } = require('node:child_process');
const { coreTargets, guiTargets, androidArchs, checkTargets, buildPlan } = require('./build-config.js');

module.exports = async ({ github, context, core, workflowKind = 'build' }) => {
  let changedFiles = [];
  if (context.eventName === 'pull_request' && !context.payload.pull_request.draft) {
    // API pagination is necessary for large/fork PRs; never truncate the list.
    const files = await github.paginate(github.rest.pulls.listFiles, {
      ...context.repo, pull_number: context.payload.pull_request.number, per_page: 100,
    });
    changedFiles = files.flatMap(file => [file.filename, file.previous_filename].filter(Boolean));
    if (files.length >= 3000 || context.payload.pull_request.changed_files > files.length) {
      core.info('The PR file listing is incomplete; run all checks conservatively.');
      changedFiles = ['.github/workflows/build.yml'];
    }
  } else if (context.eventName === 'push') {
    const before = context.payload.before;
    // A new branch has no parent range. Check all paths conservatively.
    if (!before || /^0+$/.test(before)) {
      changedFiles = ['.github/workflows/build.yml'];
    } else {
      try {
        changedFiles = execFileSync('git', ['diff', '--name-only', '-z', before, context.sha], { encoding: 'utf8' }).split('\0').filter(Boolean);
      } catch {
        core.info('The previous push commit is unavailable; run all checks conservatively.');
        changedFiles = ['.github/workflows/build.yml'];
      }
    }
  }
  const plan = buildPlan({
    eventName: context.eventName, ref: context.ref,
    draft: context.payload.pull_request?.draft ?? false, changedFiles, workflowKind,
  });
  for (const [key, value] of Object.entries(plan)) core.setOutput(key, String(value));
  for (const [key, value] of Object.entries({ core: coreTargets, gui: guiTargets, android: androidArchs, check: checkTargets })) {
    core.setOutput(`${key}_matrix`, JSON.stringify(key === 'android' ? { arch: value } : { include: value }));
  }
  core.info(`Build plan: ${JSON.stringify(plan)}`);
};
