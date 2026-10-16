const BUILD_PATH = '.github/workflows/build.yml';

// Keep release completeness tied to the same target list that creates the build matrix.
function expectedArtifacts() {
  const { coreTargets, guiTargets, androidArchs } = require('./build-config.js');
  return [
    ...coreTargets.map(({ target, name }) => ({
      name: `easytier-${name}`, kind: 'archive',
      extension: target.includes('windows') ? '.zip' : '.tar.gz',
    })),
    ...guiTargets.map(({ target, name }) => ({
      name: `easytier-gui-${name}`, kind: 'native',
      extension: target.includes('windows') ? '.exe' : '.dmg',
    })),
    ...androidArchs.map(arch => ({
      name: `easytier-mobile-android-${arch}`, kind: 'native', extension: '.apk',
    })),
  ];
}

function sameRepository(repository, repo) {
  return repository?.full_name?.toLowerCase() === `${repo.owner}/${repo.repo}`.toLowerCase();
}

async function validate({ github, context, core, now = Date.now(), runId = process.env.BUILD_RUN_ID }) {
  if (!/^[1-9]\d*$/.test(String(runId)) || !Number.isSafeInteger(Number(runId))) {
    throw new Error('build_run_id must be a positive integer.');
  }
  runId = Number(runId);
  const repo = context.repo;
  const { data: workflow } = await github.rest.actions.getWorkflow({ ...repo, workflow_id: 'build.yml' });
  const { data: run } = await github.rest.actions.getWorkflowRun({ ...repo, run_id: runId });
  const fullBuild = run.event === 'workflow_dispatch' || (run.event === 'push' && run.head_branch === 'main');
  if (!Number.isSafeInteger(workflow.id) || workflow.id <= 0 || workflow.path !== BUILD_PATH ||
      run.path !== BUILD_PATH || run.workflow_id !== workflow.id ||
      run.id !== runId || run.status !== 'completed' || run.conclusion !== 'success' || !fullBuild) {
    throw new Error(`Build run ${runId} must be a successful completed full ${BUILD_PATH} build (push to main or manual).`);
  }
  if (!sameRepository(run.repository, repo) || !sameRepository(run.head_repository, repo) ||
      !Number.isSafeInteger(run.repository?.id) || run.repository.id <= 0 ||
      run.head_repository?.id !== run.repository.id || !/^[a-f\d]{40}$/i.test(run.head_sha)) {
    throw new Error(`Build run ${runId} must belong to this repository and have a valid source commit.`);
  }
  const artifacts = await github.paginate(github.rest.actions.listWorkflowRunArtifacts, {
    ...repo, run_id: runId, per_page: 100,
  });
  const selected = expectedArtifacts().map(expected => {
    const matches = artifacts.filter(artifact => artifact.name === expected.name);
    if (matches.length !== 1) {
      throw new Error(`Build run ${runId}: expected exactly one ${expected.name} artifact, found ${matches.length}.`);
    }
    const artifact = matches[0];
    const source = artifact.workflow_run;
    if (!Number.isSafeInteger(artifact.id) || artifact.id <= 0 || artifact.expired !== false ||
        !Number.isFinite(Date.parse(artifact.expires_at)) || Date.parse(artifact.expires_at) <= now ||
        !(artifact.size_in_bytes > 0) || source?.id !== runId ||
        (source.repository_id != null && source.repository_id !== run.repository.id) ||
        (source.head_repository_id != null && source.head_repository_id !== run.repository.id) ||
        (source.head_sha != null && source.head_sha !== run.head_sha)) {
      throw new Error(`Build run ${runId}: ${expected.name} is expired, empty, or has invalid provenance.`);
    }
    return artifact;
  });
  if (new Set(selected.map(artifact => artifact.id)).size !== selected.length) {
    throw new Error(`Build run ${runId} has ambiguous artifact IDs.`);
  }
  const artifactIds = selected.map(artifact => artifact.id);
  core.setOutput('artifact_ids', artifactIds.join(','));
  core.setOutput('build_sha', run.head_sha);
  core.info(`Validated ${selected.length} release artifacts from full build ${runId} at ${run.head_sha}.`);
  return { runId, commit: run.head_sha, artifactIds, artifacts: selected };
}

// Copy already-packaged archives byte-for-byte, retaining Unix executable modes inside tarballs.
// Native installers keep their filenames and get a single combined checksum manifest.
async function prepareReleaseAssets({ sourceDir, destinationDir, commit }) {
  const fs = require('node:fs/promises');
  const path = require('node:path');
  const { createHash } = require('node:crypto');
  if (!/^[a-f\d]{40}$/i.test(commit)) throw new Error('A validated build commit is required.');
  const hash = async file => {
    const digest = createHash('sha256');
    for await (const chunk of require('node:fs').createReadStream(file)) digest.update(chunk);
    return digest.digest('hex');
  };
  const expected = expectedArtifacts();
  const directories = await fs.readdir(sourceDir, { withFileTypes: true });
  if (directories.length !== expected.length || directories.some(entry => !entry.isDirectory() ||
      !expected.some(artifact => artifact.name === entry.name))) {
    throw new Error('Downloaded artifacts do not match the validated release allowlist.');
  }
  const assets = [];
  const names = new Set(['COMMIT.txt', 'SHA256SUMS']);
  for (const artifact of expected) {
    const dir = path.join(sourceDir, artifact.name);
    const entries = await fs.readdir(dir, { withFileTypes: true });
    if (entries.some(entry => !entry.isFile() || /[\r\n\\]/.test(entry.name))) {
      throw new Error(`${artifact.name} contains an unsupported file or directory.`);
    }
    let files;
    if (artifact.kind === 'archive') {
      files = [`${artifact.name}${artifact.extension}`];
      if (entries.length !== 1 || entries[0].name !== files[0]) {
        throw new Error(`${artifact.name} must contain only its ready-made ${artifact.extension} archive.`);
      }
    } else {
      files = entries.map(entry => entry.name).filter(name => name.endsWith(artifact.extension));
      if (!files.length || entries.length !== files.length + 2 ||
          !entries.some(entry => entry.name === 'COMMIT.txt') ||
          !entries.some(entry => entry.name === 'SHA256SUMS')) {
        throw new Error(`${artifact.name} must contain native installers, COMMIT.txt, and SHA256SUMS.`);
      }
      if ((await fs.readFile(path.join(dir, 'COMMIT.txt'), 'utf8')).trim() !== commit) {
        throw new Error(`${artifact.name} was built from a different commit.`);
      }
      const manifest = (await fs.readFile(path.join(dir, 'SHA256SUMS'), 'utf8')).trim().split(/\r?\n/);
      const checksums = new Map();
      for (const line of manifest) {
        const match = /^([a-f\d]{64}) [ *](.+)$/i.exec(line);
        if (!match || checksums.has(match[2]) || ![...files, 'COMMIT.txt'].includes(match[2])) {
          throw new Error(`${artifact.name} has an invalid checksum manifest.`);
        }
        checksums.set(match[2], match[1].toLowerCase());
      }
      for (const name of [...files, 'COMMIT.txt']) {
        if (checksums.get(name) !== await hash(path.join(dir, name))) {
          throw new Error(`${artifact.name}: checksum mismatch for ${name}.`);
        }
      }
    }
    for (const name of files) {
      if (names.has(name)) throw new Error(`Release assets have a duplicate filename: ${name}.`);
      names.add(name);
      assets.push({ name, file: path.join(dir, name) });
    }
  }
  // Validate every input before creating output, and refuse to overwrite a previous preparation.
  await fs.mkdir(destinationDir);
  for (const { name, file } of assets) await fs.copyFile(file, path.join(destinationDir, name));
  await fs.writeFile(path.join(destinationDir, 'COMMIT.txt'), `${commit}\n`);
  const lines = [];
  for (const name of [...assets.map(asset => asset.name), 'COMMIT.txt'].sort()) {
    lines.push(`${await hash(path.join(destinationDir, name))}  ${name}`);
  }
  await fs.writeFile(path.join(destinationDir, 'SHA256SUMS'), `${lines.join('\n')}\n`);
  return [...assets.map(asset => asset.name), 'COMMIT.txt', 'SHA256SUMS'];
}

module.exports = validate;
module.exports.expectedArtifacts = expectedArtifacts;
module.exports.prepareReleaseAssets = prepareReleaseAssets;
