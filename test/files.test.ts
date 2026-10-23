import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import {
  chmodSync,
  existsSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { canonicalizeLock, withRegistry } from '../tooling/registry.ts';
import {
  copyTree,
  digest,
  fileMap,
  identity,
  json,
  readRelease,
  relativePath,
  safePath,
  same,
  slug,
  writeAtomic,
  writeJson,
} from '../tooling/files.ts';
import type { Release } from '../tooling/types.ts';

function temporary(): { root: string; close: () => void } {
  const root = mkdtempSync(join(tmpdir(), 'devkit-core-'));
  return { root, close: () => rmSync(root, { recursive: true, force: true }) };
}

function fixture(root: string): Release {
  mkdirSync(join(root, 'runtime'), { recursive: true });
  mkdirSync(join(root, 'npm'), { recursive: true });
  writeAtomic(join(root, 'runtime', 'devkit.sh'), '#!/bin/bash\nexit 0\n', 0o755);
  const archive = Buffer.from('loader identity fixture; not an npm installation proof');
  writeAtomic(join(root, 'npm', 'docouno-env-0.1.0.tgz'), archive);
  const release: Release = {
    schemaVersion: 1,
    version: '0.1.0',
    rustVersion: '1.99.1',
    environment: {
      name: '@docouno/env',
      version: '0.1.0',
      archive: 'npm/docouno-env-0.1.0.tgz',
      integrity: `sha512-${createHash('sha512').update(archive).digest('base64')}`,
    },
    files: fileMap(root),
  };
  writeJson(join(root, 'release.json'), release);
  return release;
}

await test('release identities bind the bytes, complete payload and npm integrity', () => {
  const temp = temporary();
  try {
    fixture(temp.root);
    assert.equal(readRelease(temp.root).version, '0.1.0');
    writeAtomic(join(temp.root, 'runtime', 'unexpected.txt'), 'extra');
    assert.throws(() => readRelease(temp.root), /file set/);
    rmSync(join(temp.root, 'runtime', 'unexpected.txt'));
    writeAtomic(join(temp.root, 'runtime', 'devkit.sh'), 'changed', 0o755);
    assert.throws(() => readRelease(temp.root), /integrity mismatch/);
  } finally {
    temp.close();
  }
});

await test('updating a generic file hash cannot conceal an npm tarball integrity mismatch', () => {
  const temp = temporary();
  try {
    const release = fixture(temp.root);
    const name = release.environment.archive;
    writeAtomic(join(temp.root, name), 'different archive');
    const changed = identity(join(temp.root, name));
    assert.ok(changed);
    release.files[name] = changed;
    writeJson(join(temp.root, 'release.json'), release);
    assert.throws(() => readRelease(temp.root), /archive integrity mismatch/);
  } finally {
    temp.close();
  }
});

await test('release cannot escape its root or substitute a symlink', () => {
  const temp = temporary();
  try {
    const release = fixture(temp.root);
    release.files['../outside'] = { sha256: '0'.repeat(64), mode: 0o644 };
    writeJson(join(temp.root, 'release.json'), release);
    assert.throws(() => readRelease(temp.root), /Unsafe release path/);
    delete release.files['../outside'];
    writeJson(join(temp.root, 'release.json'), release);
    rmSync(join(temp.root, 'runtime', 'devkit.sh'));
    symlinkSync('/etc/passwd', join(temp.root, 'runtime', 'devkit.sh'));
    assert.throws(() => readRelease(temp.root), /symlink/);
    assert.throws(() => safePath(temp.root, 'runtime/devkit.sh'), /symlink/);
  } finally {
    temp.close();
  }
});

await test('manifest schema, package version and modes are checked before consumption', () => {
  const temp = temporary();
  try {
    const release = fixture(temp.root);
    release.environment.version = '0.2.0';
    writeJson(join(temp.root, 'release.json'), release);
    assert.throws(() => readRelease(temp.root), /identity does not match/);
    release.environment.version = release.version;
    const file = release.files['runtime/devkit.sh'];
    assert.ok(file);
    file.mode = 0o777;
    writeJson(join(temp.root, 'release.json'), release);
    assert.throws(() => readRelease(temp.root), /Invalid file identity/);
    writeJson(join(temp.root, 'release.json'), { ...release, schemaVersion: 2 });
    assert.throws(() => readRelease(temp.root), /schemaVersion/);
  } finally {
    temp.close();
  }
});

await test('file identity includes permissions and atomic replacement retains requested mode', () => {
  const temp = temporary();
  try {
    const path = join(temp.root, 'private.json');
    writeAtomic(path, 'before', 0o600);
    const before = identity(path);
    assert.equal(before?.mode, 0o600);
    chmodSync(path, 0o644);
    assert.equal(same(before, identity(path)), false);
    writeAtomic(path, 'after', 0o600);
    assert.equal(readFileSync(path, 'utf8'), 'after');
    assert.equal(identity(path)?.mode, 0o600);
  } finally {
    temp.close();
  }
});

await test('payload copying excludes caches, rejects links, and can preserve opaque project links', () => {
  const temp = temporary();
  try {
    const source = join(temp.root, 'source');
    mkdirSync(join(source, 'node_modules'), { recursive: true });
    writeFileSync(join(source, 'node_modules', 'cache'), 'cache');
    writeFileSync(join(source, 'source.ts'), 'export {};');
    copyTree(source, join(temp.root, 'copy'), {
      ignore: (name) => name.split('/')[0] === 'node_modules',
    });
    assert.equal(existsSync(join(temp.root, 'copy', 'node_modules')), false);
    symlinkSync('source.ts', join(source, 'source-link'));
    assert.throws(() => copyTree(source, join(temp.root, 'bad')), /symlink/);
    copyTree(source, join(temp.root, 'project'), { preserveLinks: true });
    assert.equal(readFileSync(join(temp.root, 'project', 'source-link'), 'utf8'), 'export {};');
  } finally {
    temp.close();
  }
});

await test('names and release paths reject ambiguous or executable syntax', () => {
  assert.equal(slug('app-42'), 'app-42');
  for (const name of ['../app', 'a--b', 'a-', 'App', 'a b', 'a;echo', 'x'.repeat(64)]) {
    assert.throws(() => slug(name));
  }
  for (const name of ['/absolute', '../parent', 'a/../b', 'a//b', 'a\\b', 'a\nb']) {
    assert.throws(() => relativePath(name));
  }
  assert.equal(relativePath('.devkit/runtime/devkit.sh'), '.devkit/runtime/devkit.sh');
});

await test('bounded registry serves real metadata/bytes, refuses unsupplied own scope and redirects public requests', async () => {
  const temp = temporary();
  try {
    const archive = join(temp.root, 'example.tgz');
    const bytes = Buffer.from('fixture transport bytes');
    writeFileSync(archive, bytes);
    await withRegistry(
      [
        {
          manifest: {
            name: '@docouno/env',
            version: '0.1.0',
            peerDependencies: { typescript: '~6.0.3' },
          },
          archive,
        },
      ],
      async (registry, requests) => {
        const response = await fetch(`${registry}@docouno%2fenv`);
        assert.equal(response.status, 200);
        const metadata = (await response.json()) as {
          versions: Record<string, { dist: { tarball: string; integrity: string } }>;
        };
        const published = metadata.versions['0.1.0'];
        assert.ok(published);
        const data = await fetch(published.dist.tarball);
        assert.deepEqual(Buffer.from(await data.arrayBuffer()), bytes);
        assert.equal(
          published.dist.integrity,
          `sha512-${createHash('sha512').update(bytes).digest('base64')}`,
        );
        assert.equal((await fetch(`${registry}@docouno%2fmissing`)).status, 404);
        const publicResponse = await fetch(`${registry}is-number`, { redirect: 'manual' });
        assert.equal(publicResponse.status, 302);
        assert.equal(
          publicResponse.headers.get('location'),
          'https://registry.npmjs.org/is-number',
        );
        assert.ok(requests.includes('/@docouno/env'));
      },
    );
  } finally {
    temp.close();
  }
});

await test('canonical lock rewriting changes only fixture origins and rejects leftover local endpoints', () => {
  const lock = {
    packages: {
      'node_modules/@docouno/env': {
        resolved: 'http://127.0.0.1:4321/@docouno/env/-/env-0.1.0.tgz',
        integrity: 'sha512-proof',
      },
    },
  };
  const canonical = canonicalizeLock(lock, 'http://127.0.0.1:4321/');
  assert.equal(
    JSON.stringify(canonical),
    JSON.stringify({
      packages: {
        'node_modules/@docouno/env': {
          resolved: 'https://registry.npmjs.org/@docouno/env/-/env-0.1.0.tgz',
          integrity: 'sha512-proof',
        },
      },
    }),
  );
  assert.throws(
    () => canonicalizeLock({ resolved: 'http://localhost:1/stray' }, 'http://127.0.0.1:4321/'),
    /remains/,
  );
  assert.equal(digest('a').length, 64);
  assert.deepEqual(jsonFrom(canonical), canonical);
});

function jsonFrom(value: unknown): unknown {
  const temp = temporary();
  try {
    writeJson(join(temp.root, 'value.json'), value);
    return json(join(temp.root, 'value.json'));
  } finally {
    temp.close();
  }
}
