const BUILD_PATH = '.github/workflows/build.yml';

// Keep release completeness tied to the same target list that creates the build matrix.
function expectedArtifacts() {
  const { coreTargets, guiTargets, androidArchs } = require('./build-config.js');
  return [
    ...coreTargets.map(({ target, name }) => {
      const extension = target.includes('windows') ? '.zip' : '.tar.gz';
      return { name: `easytier-${name}${extension}`, kind: 'archive', extension };
    }),
    ...guiTargets.map(({ target, name }) => {
      const extension = target.includes('windows') ? '.exe' : '.dmg';
      return { name: `easytier-gui-${name}${extension}`, kind: 'native', extension };
    }),
    ...androidArchs.map(arch => ({
      name: `easytier-mobile-android-${arch}.apk`, kind: 'native', extension: '.apk',
    })),
  ].map(artifact => ({ ...artifact, filename: artifact.name }));
}

const validDigest = digest => typeof digest === 'string' && /^sha256:[a-f\d]{64}$/.test(digest);

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
        !Number.isSafeInteger(artifact.size_in_bytes) || artifact.size_in_bytes <= 0 || source?.id !== runId ||
        (source.repository_id != null && source.repository_id !== run.repository.id) ||
        (source.head_repository_id != null && source.head_repository_id !== run.repository.id) ||
        (source.head_sha != null && source.head_sha !== run.head_sha)) {
      throw new Error(`Build run ${runId}: ${expected.name} is expired, empty, or has invalid provenance.`);
    }
    if (!validDigest(artifact.digest)) {
      throw new Error(`Build run ${runId}: ${expected.name} must have a valid API SHA256 digest.`);
    }
    return artifact;
  });
  if (new Set(selected.map(artifact => artifact.id)).size !== selected.length) {
    throw new Error(`Build run ${runId} has ambiguous artifact IDs.`);
  }
  const artifactIds = selected.map(artifact => artifact.id);
  const artifactMetadata = selected.map(({ name, digest }) => ({ name, digest }));
  core.setOutput('artifact_ids', artifactIds.join(','));
  core.setOutput('artifact_metadata', JSON.stringify(artifactMetadata));
  core.setOutput('build_sha', run.head_sha);
  core.info(`Validated ${selected.length} release artifacts from full build ${runId} at ${run.head_sha}.`);
  return { runId, commit: run.head_sha, artifactIds, artifacts: selected, artifactMetadata };
}

// Raw artifact digests bind the downloaded file bytes to the validated build's API metadata.
// Copy packaged products byte-for-byte, retaining Unix executable modes inside tarballs.
async function prepareReleaseAssets({ sourceDir, destinationDir, commit, artifacts }) {
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
  if (!Array.isArray(artifacts) || artifacts.length !== expected.length || artifacts.some(artifact =>
    !artifact || typeof artifact !== 'object' || Array.isArray(artifact) ||
    Object.keys(artifact).length !== 2 || !Object.hasOwn(artifact, 'name') || !Object.hasOwn(artifact, 'digest') ||
    !expected.some(product => product.name === artifact.name) || !validDigest(artifact.digest)) ||
    new Set(artifacts.map(artifact => artifact.name)).size !== expected.length) {
    throw new Error('Validated artifact metadata must contain exactly one name and API SHA256 digest for every release product.');
  }
  const metadata = new Map(artifacts.map(artifact => [artifact.name, artifact.digest]));
  const directories = await fs.readdir(sourceDir, { withFileTypes: true });
  if (directories.length !== expected.length || directories.some(entry => !entry.isDirectory() ||
      !expected.some(artifact => artifact.name === entry.name))) {
    throw new Error('Downloaded artifacts do not match the validated release allowlist.');
  }
  const assets = [];
  for (const artifact of expected) {
    const dir = path.join(sourceDir, artifact.name);
    const entries = await fs.readdir(dir, { withFileTypes: true });
    if (entries.length !== 1 || !entries[0].isFile() || entries[0].name !== artifact.filename) {
      throw new Error(`${artifact.name} must contain only its same-named product file.`);
    }
    const file = path.join(dir, artifact.filename);
    if ((await fs.stat(file)).size <= 0) throw new Error(`${artifact.name} is empty.`);
    if (`sha256:${await hash(file)}` !== metadata.get(artifact.name)) {
      throw new Error(`${artifact.name}: checksum mismatch with the validated API artifact digest.`);
    }
    assets.push({ name: artifact.filename, file });
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
module.exports.validDigest = validDigest;
