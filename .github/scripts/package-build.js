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

module.exports = { checksums, archive };
if (require.main === module) {
  const [name, directory, destination, commit] = process.argv.slice(2);
  const work = name === '--native' ? checksums(directory, destination) : archive(name, directory, destination, commit);
  work.catch(error => { console.error(error); process.exitCode = 1; });
}
