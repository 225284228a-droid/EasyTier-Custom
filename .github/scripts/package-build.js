const { createHash } = require('node:crypto');
const { execFileSync } = require('node:child_process');
const fs = require('node:fs/promises');
const path = require('node:path');

async function checksums(directory, commit) {
  if (!/^[a-f0-9]{40,64}$/.test(commit)) throw new Error('A full build commit is required');
  const entries = (await fs.readdir(directory, { withFileTypes: true }))
    .filter(entry => !['COMMIT.txt', 'SHA256SUMS'].includes(entry.name));
  if (!entries.length || entries.some(entry => !entry.isFile())) throw new Error('Expected nonempty, flat build artifacts');
  await fs.writeFile(path.join(directory, 'COMMIT.txt'), `${commit}\n`);
  const names = [...entries.map(entry => entry.name), 'COMMIT.txt'].sort();
  const lines = [];
  for (const name of names) {
    const data = await fs.readFile(path.join(directory, name));
    if (!data.length) throw new Error(`Empty artifact: ${name}`);
    lines.push(`${createHash('sha256').update(data).digest('hex')}  ${name}\n`);
  }
  await fs.writeFile(path.join(directory, 'SHA256SUMS'), lines.join(''));
}

async function archive(name, directory, destination, commit) {
  if (!/^[a-zA-Z0-9][a-zA-Z0-9_.-]*$/.test(name)) throw new Error('Invalid artifact name');
  await checksums(directory, commit);
  await fs.mkdir(destination, { recursive: true });
  const windows = process.platform === 'win32';
  const output = path.resolve(destination, `${name}.${windows ? 'zip' : 'tar.gz'}`);
  if (windows) {
    execFileSync('7z', ['a', '-tzip', output, '.'], { cwd: directory, stdio: 'inherit' });
  } else {
    // GitHub ZIP artifacts discard executable modes; keep the tarball intact.
    execFileSync('tar', ['-czf', output, '-C', directory, '.'], { stdio: 'inherit' });
  }
  return output;
}

async function singleNative(name, directory, destination, commit) {
  if (!/^[a-zA-Z0-9][a-zA-Z0-9_.-]*\.(exe|dmg|apk)$/.test(name)) throw new Error('Invalid native artifact name');
  if (!/^[a-f0-9]{40,64}$/.test(commit)) throw new Error('A full build commit is required');
  const extension = path.extname(name);
  const entries = await fs.readdir(directory, { withFileTypes: true });
  if (entries.length !== 1 || !entries[0].isFile() || !entries[0].name.endsWith(extension)) {
    throw new Error(`Expected exactly one ${extension} installer`);
  }
  const source = path.join(directory, entries[0].name);
  if (!(await fs.stat(source)).size) throw new Error('Empty native artifact');
  await fs.mkdir(destination, { recursive: true });
  const output = path.resolve(destination, name);
  await fs.copyFile(source, output, require('node:fs').constants.COPYFILE_EXCL);
  return output;
}

async function recordDownload(file, commit) {
  const digest = createHash('sha256');
  for await (const chunk of require('node:fs').createReadStream(file)) digest.update(chunk);
  const checksum = digest.digest('hex');
  if (process.env.GITHUB_OUTPUT) await fs.appendFile(process.env.GITHUB_OUTPUT, `path=${file}\n`);
  if (process.env.GITHUB_STEP_SUMMARY) await fs.appendFile(process.env.GITHUB_STEP_SUMMARY,
    `### ${path.basename(file)}\n\nCommit: \`${commit}\`\n\nSHA-256: \`${checksum}\`\n\n`);
  return checksum;
}

module.exports = { checksums, archive, singleNative, recordDownload };
if (require.main === module) {
  const args = process.argv.slice(2);
  const native = args[0] === '--single-native';
  const [name, directory, destination, commit] = native ? args.slice(1) : args;
  const work = (native ? singleNative : archive)(name, directory, destination, commit)
    .then(file => recordDownload(file, commit));
  work.catch(error => { console.error(error); process.exitCode = 1; });
}
