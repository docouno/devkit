import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import { fail, object, text } from './files.ts';

export interface RegistryPackage {
  manifest: Record<string, unknown>;
  archive: string;
}

/** A bounded local consumer fixture, not an npm publishing/auth server. */
export async function withRegistry<T>(
  packages: readonly RegistryPackage[],
  action: (registry: string, requests: readonly string[]) => Promise<T>,
): Promise<T> {
  const requests: string[] = [];
  const entries = packages.map((entry) => {
    const name = text(entry.manifest.name, 'fixture package name');
    const packageVersion = text(entry.manifest.version, 'fixture package version');
    if (!name.startsWith('@docouno/') || !/^@[a-z0-9-]+\/[a-z0-9-]+$/.test(name)) {
      fail(`Fixture accepts only the devkit npm scope: ${name}`);
    }
    const bytes = readFileSync(entry.archive);
    const basename = name.slice(name.indexOf('/') + 1);
    return {
      manifest: entry.manifest,
      name,
      version: packageVersion,
      path: `/${name}/-/${basename}-${packageVersion}.tgz`,
      bytes,
      integrity: `sha512-${createHash('sha512').update(bytes).digest('base64')}`,
      shasum: createHash('sha1').update(bytes).digest('hex'),
    };
  });
  let origin = '';
  const server = createServer((request, response) => {
    try {
      if (request.method !== 'GET' && request.method !== 'HEAD') {
        response.writeHead(405).end();
        return;
      }
      const url = new URL(request.url ?? '/', 'http://fixture.invalid');
      const path = decodeURIComponent(url.pathname);
      requests.push(path);
      const archive = entries.find((entry) => entry.path === path);
      if (archive) {
        response.writeHead(200, { 'content-type': 'application/octet-stream' });
        response.end(request.method === 'HEAD' ? undefined : archive.bytes);
        return;
      }
      const own = entries.filter((entry) => `/${entry.name}` === path);
      if (own.length > 0) {
        const versions: Record<string, unknown> = Object.create(null) as Record<string, unknown>;
        for (const entry of own) {
          versions[entry.version] = {
            ...entry.manifest,
            dist: {
              tarball: `${origin}${entry.path}`,
              integrity: entry.integrity,
              shasum: entry.shasum,
            },
          };
        }
        response.writeHead(200, { 'content-type': 'application/json' });
        response.end(
          JSON.stringify({
            name: own[0]?.name,
            'dist-tags': { latest: own.at(-1)?.version },
            versions,
            modified: '2026-01-01T00:00:00.000Z',
          }),
        );
        return;
      }
      if (path.startsWith('/@docouno/')) {
        response.writeHead(404, { 'content-type': 'application/json' });
        response.end(JSON.stringify({ error: 'No supplied toolkit artifact for this package' }));
        return;
      }
      // Full registry override is required to exercise canonical npmjs lock URLs.
      // Public package bodies still come directly from the public npm registry.
      response.writeHead(302, {
        location: `https://registry.npmjs.org${url.pathname}${url.search}`,
      });
      response.end();
    } catch {
      response.writeHead(400).end();
    }
  });
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => resolve());
  });
  const address = server.address() as AddressInfo;
  origin = `http://127.0.0.1:${address.port}`;
  try {
    return await action(`${origin}/`, requests);
  } finally {
    server.closeAllConnections();
    await new Promise<void>((resolve, reject) => {
      server.close((error) => (error ? reject(error) : resolve()));
    });
  }
}

export function canonicalizeLock(value: unknown, registry: string): Record<string, unknown> {
  const lock = object(value, 'package lock');
  function visit(node: unknown): void {
    if (Array.isArray(node)) {
      for (const child of node) {
        visit(child);
      }
    } else if (node !== null && typeof node === 'object') {
      const record = node as Record<string, unknown>;
      if (typeof record.resolved === 'string' && record.resolved.startsWith(registry)) {
        const resolved = new URL(record.resolved);
        record.resolved = `https://registry.npmjs.org${resolved.pathname}${resolved.search}`;
      }
      for (const child of Object.values(record)) {
        visit(child);
      }
    }
  }
  visit(lock);
  if (JSON.stringify(lock).includes('127.0.0.1') || JSON.stringify(lock).includes('localhost')) {
    fail('Test registry URL remains in a canonical lockfile');
  }
  return lock;
}
