import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { withRegistry, canonicalizeLock } from '/fixture/registry.js';

const read = (path) => JSON.parse(readFileSync(path, 'utf8'));
const hash = (path) => createHash('sha256').update(readFileSync(path)).digest('hex');
const workspace = mkdtempSync(join(tmpdir(), 'devkit-registry-'));
const cleanEnvironment = Object.fromEntries(
  Object.entries(process.env).filter(([key]) => !/^npm_|^NODE_/i.test(key)),
);
let serial = 0;
async function command(args, cwd, registry, expectedFailure) {
  const result = await new Promise((resolve, reject) => {
    const child = spawn('npm', args, {
      cwd,
      env: {
        ...cleanEnvironment,
        CI: 'true',
        npm_config_registry: registry,
        npm_config_cache: join(workspace, `fresh-cache-${serial++}`),
        npm_config_userconfig: '/dev/null',
        npm_config_audit: 'false',
        npm_config_fund: 'false',
        npm_config_update_notifier: 'false',
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let output = '';
    child.stdout.on('data', (bytes) => {
      output += bytes;
    });
    child.stderr.on('data', (bytes) => {
      output += bytes;
    });
    child.once('error', reject);
    child.once('close', (code, signal) => resolve({ code, signal, output }));
  });
  process.stdout.write(`npm ${args.join(' ')}: ${result.code}\n${result.output}\n`);
  assert.equal(result.signal, null);
  if (expectedFailure) {
    assert.notEqual(result.code, 0);
    assert.match(result.output, expectedFailure);
  } else {
    assert.equal(result.code, 0);
  }
}

const releases = ['/release-v1', '/release-v2'];
const metadata = releases.map((root) => read(join(root, 'release.json')));
assert.notEqual(metadata[0].version, metadata[1].version);
const entries = metadata.map((release, index) => ({
  manifest: read(join(releases[index], 'templates/node-app/package.json')),
  archive: join(releases[index], release.environment.archive),
}));
// The registry metadata must be the actual packed environment manifest, rather
// than a template's dependencies or a hand-maintained resolver substitute.
for (let index = 0; index < entries.length; index++) {
  const bytes = await new Promise((resolve, reject) => {
    const child = spawn('tar', ['-xOf', entries[index].archive, 'package/package.json']);
    let output = '';
    child.stdout.on('data', (chunk) => {
      output += chunk;
    });
    child.once('error', reject);
    child.once('close', (code) => (code === 0 ? resolve(output) : reject(new Error('tar failed'))));
  });
  entries[index].manifest = JSON.parse(bytes);
  assert.equal(entries[index].manifest.version, metadata[index].version);
}

try {
  await withRegistry(entries, async (registry, requests) => {
    const { initProject } = await import('/release-v1/cli/project.js');
    const { updateProject } = await import('/release-v2/cli/project-update.js');
    const target = join(workspace, 'consumer');
    await initProject(releases[0], target, { template: 'node', packageMode: 'registry' });
    const app = join(target, 'apps/main');
    const lockPath = join(app, 'package-lock.json');
    const canonicalLock = readFileSync(lockPath);
    assert.doesNotMatch(canonicalLock.toString(), /127\.0\.0\.1|localhost|file:|\/workspace\//);
    assert.equal(
      read(join(app, 'package.json')).devDependencies['@docouno/env'],
      metadata[0].version,
    );
    await command(['ci', '--include=dev', '--strict-peer-deps'], app, registry);
    assert.deepEqual(readFileSync(lockPath), canonicalLock);
    await command(['run', 'check'], app, registry);
    await command(['run', 'build'], app, registry);
    assert.equal(
      read(join(app, 'node_modules/@docouno/env/package.json')).version,
      metadata[0].version,
    );

    const sourcePath = join(app, 'src/greeting.ts');
    const sourceHash = hash(sourcePath);
    await updateProject(releases[1], target, { registry });
    assert.equal(hash(sourcePath), sourceHash);
    assert.equal(read(join(target, '.devkit/manifest.json')).version, metadata[1].version);
    assert.equal(
      read(join(app, 'package.json')).devDependencies['@docouno/env'],
      metadata[1].version,
    );
    writeFileSync(
      lockPath,
      `${JSON.stringify(canonicalizeLock(read(lockPath), registry), null, 2)}\n`,
    );
    const upgradedLock = readFileSync(lockPath);
    assert.doesNotMatch(upgradedLock.toString(), /127\.0\.0\.1|localhost|file:|\/workspace\//);
    await command(['ci', '--include=dev', '--strict-peer-deps'], app, registry);
    assert.deepEqual(readFileSync(lockPath), upgradedLock);
    await command(['run', 'check'], app, registry);
    await command(['run', 'build'], app, registry);
    assert.equal(
      read(join(app, 'node_modules/@docouno/env/package.json')).version,
      metadata[1].version,
    );

    const corrupt = read(lockPath);
    corrupt.packages['node_modules/@docouno/env'].integrity =
      `sha512-${Buffer.alloc(64).toString('base64')}`;
    writeFileSync(lockPath, JSON.stringify(corrupt));
    await command(
      ['ci', '--include=dev', '--strict-peer-deps', '--fetch-retries=0'],
      app,
      registry,
      /EINTEGRITY/,
    );
    writeFileSync(lockPath, upgradedLock);
    await command(
      ['install', '--package-lock-only', '--ignore-scripts', '@docouno/env@99.99.99'],
      app,
      registry,
      /ETARGET|No matching version/,
    );
    for (const release of metadata) {
      assert(requests.includes(`/@docouno/env/-/env-${release.version}.tgz`));
    }
    assert(requests.some((path) => !path.startsWith('/@docouno/')));
    process.stdout.write(
      `PASS: canonical registry init/update/ci, real exports/gates, integrity and missing-version negatives; ${requests.length} fixture requests.\n`,
    );
  });
} finally {
  rmSync(workspace, { recursive: true, force: true });
}
