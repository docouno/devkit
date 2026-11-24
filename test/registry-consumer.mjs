// Author-only npm/registry checks. Project lifecycle is the native Rust fixture.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { join } from 'node:path';
import { npmEnvironment } from '../tooling/npm-inputs/npm.mjs';

const read = (path) => JSON.parse(readFileSync(path, 'utf8'));
const sri = (bytes) => `sha512-${createHash('sha512').update(bytes).digest('base64')}`;
const mode = process.argv[2];
const releases = ['/release-old', '/release-current'].filter(existsSync);
const metadata = releases.map((root) => read(join(root, 'release.json')));
const current = metadata.at(-1);
const proof = '/proof';
if (mode === 'serve') {
  const entries = releases.map((root, index) => {
    const archive = readFileSync(join(root, metadata[index].environment.archive));
    const packed = spawnSync(
      'tar',
      ['-xOf', join(root, metadata[index].environment.archive), 'package/package.json'],
      { encoding: 'utf8' },
    );
    assert.equal(packed.status, 0, packed.stderr);
    const manifest = JSON.parse(packed.stdout);
    assert.equal(manifest.name, '@docouno/env');
    assert.equal(manifest.version, metadata[index].version);
    assert.equal(sri(archive), metadata[index].environment.integrity);
    return { manifest, archive, path: `/@docouno/env/-/env-${manifest.version}.tgz` };
  });
  const published = new Date().toISOString();
  let origin = '',
    count = 0;
  const requests = [];
  const server = createServer({ requestTimeout: 15000, headersTimeout: 5000 }, (req, res) => {
    try {
      if (++count > 8192 || (req.url?.length ?? 0) > 4096) {
        res.writeHead(429).end();
        return;
      }
      if (!['GET', 'HEAD'].includes(req.method)) {
        res.writeHead(405).end();
        return;
      }
      const url = new URL(req.url, 'http://fixture.invalid');
      const path = decodeURIComponent(url.pathname);
      requests.push(path);
      writeFileSync(join(proof, 'requests.json'), JSON.stringify(requests));
      let body,
        type = 'application/json';
      const entry = entries.find((item) => item.path === path);
      if (entry) {
        body = entry.archive;
        type = 'application/octet-stream';
      } else if (
        path === '/@docouno/env' ||
        entries.some((item) => path === `/@docouno/env/${item.manifest.version}`)
      ) {
        const versions = Object.fromEntries(
          entries.map((item) => [
            item.manifest.version,
            {
              ...item.manifest,
              dist: {
                tarball: origin + item.path,
                integrity: sri(item.archive),
                shasum: createHash('sha1').update(item.archive).digest('hex'),
              },
            },
          ]),
        );
        body = JSON.stringify(
          path === '/@docouno/env'
            ? {
                name: '@docouno/env',
                'dist-tags': { latest: current.version },
                versions,
                time: {
                  [metadata[0].version]: '2020-01-01T00:00:00Z',
                  [current.version]: published,
                },
              }
            : versions[path.slice('/@docouno/env/'.length)],
        );
      } else if (
        path.startsWith('/@docouno/') ||
        path.startsWith('/-/npm/v1/attestations/@docouno/')
      ) {
        // The fixture releases have no public provenance. Let pnpm read their local publish times.
        res.writeHead(404).end();
        return;
      } else {
        res
          .writeHead(302, { location: `https://registry.npmjs.org${url.pathname}${url.search}` })
          .end();
        return;
      }
      res.writeHead(200, { 'content-type': type, 'content-length': Buffer.byteLength(body) });
      res.end(req.method === 'HEAD' ? undefined : body);
    } catch {
      res.writeHead(400).end();
    }
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  origin = `http://127.0.0.1:${server.address().port}`;
  writeFileSync(join(proof, 'registry-url'), `${origin}/`);
  const stop = () => {
    server.closeAllConnections();
    server.close();
  };
  process.once('SIGTERM', stop);
  process.once('SIGINT', stop);
} else {
  const registry = readFileSync(join(proof, 'registry-url'), 'utf8');
  const target = '/consumer';
  const env = npmEnvironment(join(proof, `npm-${mode}`), registry);
  delete env.npm_config_workspaces;
  function pnpm(args, cwd = target, expected) {
    const result = spawnSync('pnpm', args, {
      cwd,
      env,
      encoding: 'utf8',
      timeout: 180000,
      maxBuffer: 8 * 1024 * 1024,
    });
    process.stdout.write(
      `pnpm ${args.join(' ')}: ${result.status}\n${result.stdout}${result.stderr}\n`,
    );
    if (expected) {
      assert.notEqual(result.status, 0);
      assert.match(result.stdout + result.stderr, expected);
    } else {
      assert.equal(result.status, 0, result.error?.message);
    }
    return result.stdout + result.stderr;
  }
  const lock = join(target, 'pnpm-lock.yaml');
  const selected = `@docouno/env@${current.environment.version}`;
  if (mode === 'second' || mode === 'release-day') {
    assert(
      readFileSync(join(target, 'pnpm-workspace.yaml'), 'utf8').includes(selected),
      'Generated workspace must select the exact environment version',
    );
  }
  if (mode === 'first' || mode === 'second') {
    // Public v9 packages need only SRI; remove the exact local fixture tarball URL.
    const text = readFileSync(lock, 'utf8').replace(/,? ?tarball: ([^,}\n]+)/gu, (match, url) => {
      const value = url.trim().replace(/^['"]|['"]$/gu, '');
      if (value.startsWith(registry)) {
        return '';
      }
      assert(
        value.startsWith('https://registry.npmjs.org/'),
        `Unexpected fixture tarball ${value}`,
      );
      return match;
    });
    writeFileSync(lock, text);
  }
  const original = readFileSync(lock);
  assert.doesNotMatch(original.toString(), /127\.0\.0\.1|localhost|\bfile:|\/workspace\//);
  if (mode === 'first' || mode === 'second') {
    assert(!existsSync(join(target, 'package-lock.json')));
    assert(existsSync(join(target, 'pnpm-workspace.yaml')));
    pnpm([
      'install',
      '--frozen-lockfile',
      '--ignore-scripts',
      '--trust-lockfile',
      '--store-dir',
      '/proof/pnpm-store',
    ]);
    assert.deepEqual(readFileSync(lock), original);
    for (const task of ['check', 'build']) {
      pnpm(['--dir', 'apps/main', 'run', task]);
    }
    const probe = spawnSync(
      'node',
      [
        '--input-type=module',
        '-e',
        `import {createRequire} from 'node:module'; import {readFileSync} from 'node:fs'; import {dirname,join} from 'node:path'; const r=createRequire('/consumer/apps/main/package.json'); let p=dirname(r.resolve('@docouno/env/eslint')); process.stdout.write(readFileSync(join(p,'../package.json'),'utf8'));`,
      ],
      { cwd: target, env, encoding: 'utf8', timeout: 10000 },
    );
    assert.equal(probe.status, 0, probe.stderr);
    assert.equal(
      JSON.parse(probe.stdout).version,
      mode === 'first' ? metadata[0].version : current.version,
    );
  } else if (mode === 'release-day') {
    const workspace = join(target, 'pnpm-workspace.yaml');
    const settings = readFileSync(workspace, 'utf8');
    const app = join(target, 'apps/main');
    const manifest = join(app, 'package.json');
    assert(!Object.hasOwn(read(manifest).dependencies, 'clsx'));
    const add = ['add', 'clsx@2.1.1', '--lockfile-only', '--ignore-scripts'];

    // Check that the fresh release blocks this edit without its exact exception.
    writeFileSync(workspace, settings.replace(selected, 'other-package@1.0.0'));
    try {
      const rejected = pnpm(add, app, /minimumReleaseAge|minimum release age/iu);
      assert(rejected.includes(selected), 'The fresh environment version must trigger the failure');
    } finally {
      writeFileSync(workspace, settings);
    }
    assert(!Object.hasOwn(read(manifest).dependencies, 'clsx'));
    assert.deepEqual(readFileSync(lock), original);
    pnpm(add, app);
    assert(read(manifest).dependencies.clsx);
    assert.notDeepEqual(readFileSync(lock), original);
  } else if (mode === 'negative') {
    const lines = original.toString().split('\n');
    let inside = false,
      changed = false;
    for (let index = 0; index < lines.length; index++) {
      if (/^ {2}\S/u.test(lines[index])) {
        inside = lines[index].includes(selected);
      }
      if (inside && lines[index].includes('integrity: sha512-')) {
        lines[index] = lines[index].replace(
          /sha512-[A-Za-z0-9+/=]+/u,
          `sha512-${Buffer.alloc(64).toString('base64')}`,
        );
        changed = true;
        break;
      }
    }
    assert(changed, 'Missing env integrity');
    writeFileSync(lock, lines.join('\n'));
    // Relink from the populated store so only the tampered entry needs a tarball.
    const rejected = pnpm(
      [
        'install',
        '--frozen-lockfile',
        '--ignore-scripts',
        '--trust-lockfile',
        '--store-dir',
        '/proof/pnpm-store',
        '--virtual-store-dir',
        '/proof/pnpm-neg-modules',
        '--config.fetch-retries=0',
      ],
      target,
      /ERR_PNPM_TARBALL_INTEGRITY|EINTEGRITY/,
    );
    assert(
      rejected.replace(/\s/gu, '').includes(current.environment.integrity),
      'pnpm must report the actual integrity of the released environment tarball',
    );
    writeFileSync(lock, original);
    const missing = '@docouno/env@99.99.99';
    const unavailable = pnpm(
      [
        'add',
        '--lockfile-only',
        '--ignore-scripts',
        '--trust-lockfile',
        missing,
        '--config.fetch-retries=0',
      ],
      join(target, 'apps/main'),
      /ERR_PNPM_NO_MATCHING_VERSION|No matching version/,
    );
    assert(unavailable.includes(missing), 'pnpm must reject the requested environment version');
    const unknown = await fetch(registry + '@docouno/unsupplied');
    assert.equal(unknown.status, 404);
    const attestation = await fetch(`${registry}-/npm/v1/attestations/${selected}`, {
      redirect: 'manual',
    });
    assert.equal(attestation.status, 404, 'Fixture provenance must stay local');
    const requests = read(join(proof, 'requests.json'));
    for (const item of metadata) {
      assert(requests.includes(`/@docouno/env/-/env-${item.version}.tgz`));
    }
    assert(requests.some((path) => path.endsWith('.tgz') && !path.startsWith('/@docouno/')));
  } else {
    throw Error('Expected serve, first, second, negative or release-day');
  }
  assert(existsSync(lock));
  process.stdout.write(`PASS ${mode}\n`);
}
