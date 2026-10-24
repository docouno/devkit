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
const releases = ['/release-v1', '/release-v2'];
const metadata = releases.map((root) => read(join(root, 'release.json')));
assert.notEqual(metadata[0].version, metadata[1].version);
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
            ? { name: '@docouno/env', 'dist-tags': { latest: metadata[1].version }, versions }
            : versions[path.slice('/@docouno/env/'.length)],
        );
      } else if (path.startsWith('/@docouno/')) {
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
  server.maxConnections = 16;
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
  function npm(args, cwd = target, expected) {
    const result = spawnSync('npm', args, {
      cwd,
      env,
      encoding: 'utf8',
      timeout: 180000,
      maxBuffer: 8 * 1024 * 1024,
    });
    process.stdout.write(
      `npm ${args.join(' ')}: ${result.status}\n${result.stdout}${result.stderr}\n`,
    );
    if (expected) {
      assert.notEqual(result.status, 0);
      assert.match(result.stdout + result.stderr, expected);
    } else {
      assert.equal(result.status, 0, result.error?.message);
    }
  }
  const lock = join(target, 'package-lock.json');
  // The private registry seam can produce fixture origins after native npm
  // resolution. Restore only that exact origin, as the former registry test did.
  // The public CLI never supplies this override and is tested after publication.
  if (mode === 'second') {
    const value = read(lock);
    for (const entry of Object.values(value.packages)) {
      if (typeof entry.resolved === 'string' && entry.resolved.startsWith(registry)) {
        entry.resolved = `https://registry.npmjs.org/${entry.resolved.slice(registry.length)}`;
      }
    }
    writeFileSync(lock, `${JSON.stringify(value, null, 2)}\n`);
  }
  const original = readFileSync(lock);
  assert.doesNotMatch(original.toString(), /127\.0\.0\.1|localhost|file:|\/workspace\//);
  if (mode === 'first' || mode === 'second') {
    npm(['ci', '--include=dev', '--strict-peer-deps']);
    assert.deepEqual(readFileSync(lock), original);
    npm(['run', '--workspace=apps/main', 'check']);
    npm(['run', '--workspace=apps/main', 'build']);
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
    assert.equal(JSON.parse(probe.stdout).version, metadata[mode === 'first' ? 0 : 1].version);
  } else if (mode === 'negative') {
    const value = read(lock);
    const locations = Object.keys(value.packages).filter((p) =>
      p.endsWith('node_modules/@docouno/env'),
    );
    assert.equal(locations.length, 1);
    value.packages[locations[0]].integrity = `sha512-${Buffer.alloc(64).toString('base64')}`;
    writeFileSync(lock, JSON.stringify(value));
    npm(['ci', '--include=dev', '--strict-peer-deps', '--fetch-retries=0'], target, /EINTEGRITY/);
    writeFileSync(lock, original);
    npm(
      ['install', '--package-lock-only', '--ignore-scripts', '@docouno/env@99.99.99'],
      join(target, 'apps/main'),
      /ETARGET|No matching version/,
    );
    const unknown = await fetch(registry + '@docouno/unsupplied');
    assert.equal(unknown.status, 404);
    const requests = read(join(proof, 'requests.json'));
    for (const item of metadata) {
      assert(requests.includes(`/@docouno/env/-/env-${item.version}.tgz`));
    }
    assert(requests.some((path) => !path.startsWith('/@docouno/')));
  } else {
    throw Error('Expected serve, first, second or negative');
  }
  assert(existsSync(lock));
  process.stdout.write(`PASS ${mode}\n`);
}
