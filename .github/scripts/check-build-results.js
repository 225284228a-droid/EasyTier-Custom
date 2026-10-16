const expectedJobs = plan => ({
  'ci-scripts': plan.ci === 'true', frontend: plan.frontend === 'true',
  lint: plan.rust === 'true', unit: plan.rust === 'true', network: plan.rust === 'true',
  check: plan.rust === 'true' && plan.full !== 'true',
  core: plan.full === 'true', gui: plan.full === 'true', android: plan.rust === 'true',
  go: plan.go === 'true', js: plan.js === 'true', nix: plan.nix === 'true',
});

function checkResults(needs) {
  if (needs.prepare?.result !== 'success') throw new Error('Build preparation did not succeed');
  const expected = expectedJobs(needs.prepare.outputs);
  for (const [job, required] of Object.entries(expected)) {
    const result = needs[job]?.result;
    if ((required && result !== 'success') || ['failure', 'cancelled'].includes(result)) {
      throw new Error(`${job}: expected ${required ? 'success' : 'success or skipped'}, got ${result}`);
    }
  }
}

module.exports = { expectedJobs, checkResults };
if (require.main === module) checkResults(JSON.parse(process.env.BUILD_RESULTS));
