import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { join } from 'node:path';

const publicRegistry = 'https://registry.npmjs.org/';

/** Separate, empty configs: npm 11 refuses to load /dev/null as both files. */
export function npmEnvironment(directory, registry = publicRegistry, inherited = process.env) {
  mkdirSync(directory, { recursive: true });
  const userconfig = join(directory, 'user.npmrc');
  const globalconfig = join(directory, 'global.npmrc');
  writeFileSync(userconfig, '', { mode: 0o600 });
  writeFileSync(globalconfig, '', { mode: 0o600 });
  const clean = Object.fromEntries(
    Object.entries(inherited).filter(([key]) => !/^(?:npm_|node_)/i.test(key)),
  );
  return {
    ...clean,
    HOME: directory,
    npm_config_userconfig: userconfig,
    npm_config_globalconfig: globalconfig,
    npm_config_cache: join(directory, 'cache'),
    npm_config_registry: registry,
    npm_config_update_notifier: 'false',
    npm_config_fund: 'false',
    npm_config_audit: 'false',
    npm_config_ignore_scripts: 'true',
    npm_config_fetch_retries: '1',
    npm_config_fetch_timeout: '30000',
    npm_config_package_lock: 'true',
    npm_config_workspaces: 'false',
  };
}

/** Author-only native npm subprocess; bounded output/time and no shell. */
export async function runNpm(args, { cwd, env, signal, timeout = 180_000 }) {
  signal?.throwIfAborted();
  return await new Promise((resolve, reject) => {
    const child = spawn('npm', args, { cwd, env, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '',
      stderr = '',
      failure;
    const stop = (error) => {
      failure ??= error;
      child.kill('SIGTERM');
      killTimer ??= setTimeout(() => child.kill('SIGKILL'), 1000);
      killTimer.unref();
    };
    let killTimer;
    const timer = setTimeout(() => stop(new Error('npm operation timed out')), timeout);
    const abort = () => stop(signal.reason ?? new Error('npm operation cancelled'));
    signal?.addEventListener('abort', abort, { once: true });
    const append = (kind, chunk) => {
      if (kind === 'stdout') {
        stdout += chunk;
      } else {
        stderr += chunk;
      }
      if (stdout.length + stderr.length > 8 * 1024 * 1024) {
        stop(new Error('npm output exceeded 8 MiB'));
      }
    };
    child.stdout.setEncoding('utf8').on('data', (chunk) => append('stdout', chunk));
    child.stderr.setEncoding('utf8').on('data', (chunk) => append('stderr', chunk));
    const cleanup = () => {
      clearTimeout(timer);
      clearTimeout(killTimer);
      signal?.removeEventListener('abort', abort);
    };
    child.once('error', (error) => {
      cleanup();
      reject(error);
    });
    child.once('close', (code, terminated) => {
      cleanup();
      if (failure) {
        reject(failure);
      } else if (code !== 0 || terminated !== null) {
        reject(new Error(`npm ${args[0]} failed (${terminated ?? code}): ${stderr.trim()}`));
      } else {
        resolve({ stdout, stderr });
      }
    });
    if (signal?.aborted) {
      abort();
    }
  });
}

/** Serves only this env package; all external package requests go to public npm. */
export async function withEnvRegistry(manifest, archive, action) {
  if (manifest.name !== '@docouno/env') {
    throw new Error('Registry accepts only @docouno/env');
  }
  const bytes = readFileSync(archive);
  if (bytes.length > 64 * 1024 * 1024) {
    throw new Error('Environment archive exceeds the 64 MiB author registry limit');
  }
  const integrity = `sha512-${createHash('sha512').update(bytes).digest('base64')}`;
  const path = `/@docouno/env/-/env-${manifest.version}.tgz`;
  let origin = '',
    count = 0;
  const server = createServer({ requestTimeout: 15_000, headersTimeout: 5000 }, (req, res) => {
    try {
      if (++count > 8192 || (req.url?.length ?? 0) > 4096) {
        res.writeHead(429).end();
        return;
      }
      if (!['GET', 'HEAD'].includes(req.method)) {
        res.writeHead(405).end();
        return;
      }
      const url = new URL(req.url ?? '/', 'http://fixture.invalid');
      const pathname = decodeURIComponent(url.pathname);
      let body,
        type = 'application/json';
      if (pathname === path) {
        body = bytes;
        type = 'application/octet-stream';
      } else if (pathname === '/@docouno/env' || pathname === `/@docouno/env/${manifest.version}`) {
        const version = {
          ...manifest,
          dist: {
            tarball: `${origin}${path}`,
            integrity,
            shasum: createHash('sha1').update(bytes).digest('hex'),
          },
        };
        body = JSON.stringify(
          pathname === '/@docouno/env'
            ? {
                name: manifest.name,
                'dist-tags': { latest: manifest.version },
                versions: { [manifest.version]: version },
              }
            : version,
        );
      } else if (pathname.startsWith('/@docouno/')) {
        res.writeHead(404).end();
        return;
      } else {
        res
          .writeHead(302, {
            location: `${publicRegistry.slice(0, -1)}${url.pathname}${url.search}`,
          })
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
  try {
    return await action(`${origin}/`);
  } finally {
    server.closeAllConnections();
    await new Promise((resolve, reject) =>
      server.close((error) => (error ? reject(error) : resolve())),
    );
  }
}
