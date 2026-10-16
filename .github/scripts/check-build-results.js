const expectedJobs = (plan, kind = 'build') => {
  if (kind === 'build') return {
    frontend: plan.frontend === 'true',
    core: plan.full === 'true', gui: plan.full === 'true', android: plan.full === 'true',
  };
  if (kind !== 'check') throw new Error(`Unknown workflow kind: ${kind}`);
  return {
    'ci-scripts': plan.ci === 'true', frontend: plan.frontend === 'true',
    lint: plan.rust === 'true', unit: plan.rust === 'true', network: plan.rust === 'true',
    check: plan.rust === 'true' && plan.full !== 'true',
    android: plan.rust === 'true' && plan.full !== 'true',
    go: plan.go === 'true', js: plan.js === 'true', nix: plan.nix === 'true',
  };
};

function checkResults(needs, kind = 'build') {
  if (needs.prepare?.result !== 'success') throw new Error('Build preparation did not succeed');
  const expected = expectedJobs(needs.prepare.outputs, kind);
  for (const [job, required] of Object.entries(expected)) {
    const result = needs[job]?.result;
    if ((required && result !== 'success') || ['failure', 'cancelled'].includes(result)) {
      throw new Error(`${job}: expected ${required ? 'success' : 'success or skipped'}, got ${result}`);
    }
  }
}

module.exports = { expectedJobs, checkResults };
if (require.main === module) checkResults(JSON.parse(process.env.BUILD_RESULTS), process.env.WORKFLOW_KIND || 'build');
