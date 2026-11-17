import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { fileURLToPath, pathToFileURL } from 'node:url';
const pnpmRoot = fileURLToPath(new URL('../../../node_modules/.pnpm', import.meta.url));
import test from 'node:test';
import { devkitWorkspaceSource } from '../src/vite.js';
import { exactHost, previewHosts, hostPolicy } from '../src/preview.js';
import { hmrRecovery } from '../src/hmr.js';

test('producer and consumer share browser-equivalent canonical host vectors', () => {
  const corpus = JSON.parse(
    fs.readFileSync(new URL('../../../test/fixtures/preview-hosts.json', import.meta.url), 'utf8'),
  );
  for (const [raw, canonical] of corpus.hosts) {
    assert.equal(exactHost(raw), canonical, raw);
  }
  for (const raw of corpus.invalidHosts) {
    assert.throws(() => exactHost(raw), raw);
  }
  for (const [raw, canonical] of corpus.urls) {
    assert.deepEqual(previewHosts({ DEVKIT_PREVIEW_URL: raw }), [canonical], raw);
  }
  for (const raw of corpus.invalidUrls) {
    assert.throws(() => previewHosts({ DEVKIT_PREVIEW_URL: raw }), raw);
  }
});

function environment(values, run) {
  const before = { ...process.env };
  for (const key of Object.keys(process.env)) {
    if (key.startsWith('DEVKIT_') || key === 'VITE_ALLOWED_HOSTS' || key === 'VITEST') {
      delete process.env[key];
    }
  }
  Object.assign(process.env, values);
  try {
    return run();
  } finally {
    for (const key of Object.keys(process.env)) {
      if (!(key in before)) {
        delete process.env[key];
      }
    }
    Object.assign(process.env, before);
  }
}
test('serve preview works without a project or packages, merges hosts and guards automatic websocket proxy', () => {
  environment(
    {
      DEVKIT_PREVIEW_URL: 'https://review.example.test',
      DEVKIT_ALLOWED_HOSTS: '.team.example.test',
      DEVKIT_API_TARGET: 'http://api:8080',
    },
    () => {
      const plugin = devkitWorkspaceSource('/no-project');
      plugin.config({}, { command: 'serve', mode: 'custom' });
      const config = { server: { allowedHosts: ['owned.example.test'] } };
      plugin.configResolved(config);
      plugin.configureServer.handler({ config });
      assert.deepEqual(config.server.allowedHosts, [
        'owned.example.test',
        'review.example.test',
        '.team.example.test',
      ]);
      const allowsHost = hostPolicy(config.server.allowedHosts);
      assert.equal(allowsHost('review.example.test:443'), true);
      assert.equal(allowsHost('evil.example.test'), false);
      assert.equal(allowsHost('sub.team.example.test'), true);
      const api = config.server.proxy['/api'];
      assert.equal(api.ws, true);
      assert.equal(api.changeOrigin, true);
      let forwarded = 0;
      const proxy = {
        ws() {
          forwarded++;
          assert.equal(this, proxy);
        },
      };
      api.configure(proxy);
      let rejected = '';
      proxy.ws(
        { headers: { host: 'evil.example.test' } },
        {
          end(value) {
            rejected = value;
          },
        },
      );
      assert.match(rejected, /403/u);
      assert.equal(forwarded, 0);
      proxy.ws({ headers: { host: 'review.example.test' } }, {});
      assert.equal(forwarded, 1);
      const custom = { target: 'http://custom:8080', ws: false, configure() {} };
      const original = { '/api': custom, '/other': { target: 'http://other' } };
      const preserved = { server: { proxy: original, allowedHosts: true } };
      plugin.configResolved(preserved);
      plugin.configureServer.handler({ config: preserved });
      assert.equal(preserved.server.proxy['/api'], custom);
      assert.equal(preserved.server.proxy['/other'], original['/other']);
      assert.equal(preserved.server.allowedHosts, true);
    },
  );
});
test('build and Vitest skip all preview reads and patches regardless of mode', () => {
  environment(
    { DEVKIT_PREVIEW_HOSTS_FILE: '/unreadable', DEVKIT_PREVIEW_HOSTS_SHA256: 'bad' },
    () => {
      const plugin = devkitWorkspaceSource('/no-project');
      assert.equal(plugin.config({}, { command: 'build', mode: 'development' }), undefined);
      assert.equal(plugin.transform('', '/vite/dist/client/client.mjs'), null);
      process.env.VITEST = 'true';
      assert.equal(plugin.config({}, { command: 'serve', mode: 'custom' }), undefined);
    },
  );
});
test('manual host inputs reject URL, wildcard and oversized lists', () => {
  for (const raw of ['*', 'https://host.test', 'host.test:1234', 'host.test,', 'a'.repeat(65537)]) {
    assert.throws(() => previewHosts({ DEVKIT_ALLOWED_HOSTS: raw }));
  }
  assert.deepEqual(
    previewHosts({
      DEVKIT_ALLOWED_HOSTS: '.team.test, exact.test',
      DEVKIT_PREVIEW_URL: 'https://review.test:443',
    }),
    ['review.test', '.team.test', 'exact.test'],
  );
  assert.throws(() => previewHosts({ DEVKIT_PREVIEW_HOSTS_FILE: '/etc/passwd' }));
});
test('literal metadata carrier verifies digest, schema, nofollow file and bounded host set', (context) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'devkit-preview-'));
  try {
    const directory = path.join(root, 'devkit');
    fs.mkdirSync(directory);
    const source = path.join(directory, 'preview-hosts.json');
    const literal = '/run/devkit/preview-hosts.json';
    const stat = fs.lstatSync;
    const open = fs.openSync;
    const remap = (value) =>
      value === '/run'
        ? root
        : value === '/run/devkit'
          ? directory
          : value === literal
            ? source
            : value;
    context.mock.method(fs, 'lstatSync', (value, options) => stat(remap(value), options));
    context.mock.method(fs, 'openSync', (value, flags, ...rest) => {
      if (value === literal) {
        assert.ok(flags & fs.constants.O_NOFOLLOW);
      }
      return open(remap(value), flags, ...rest);
    });
    const bytes = Buffer.from(
      JSON.stringify({ schemaVersion: 1, service: 'web', hosts: ['a.test', 'b.test'] }),
    );
    fs.writeFileSync(source, bytes);
    const sha = createHash('sha256').update(bytes).digest('hex');
    const env = {
      DEVKIT_PREVIEW_HOSTS_FILE: literal,
      DEVKIT_PREVIEW_HOSTS_SHA256: sha,
      DEVKIT_SERVICE: 'web',
    };
    assert.deepEqual(previewHosts(env), ['a.test', 'b.test']);
    assert.deepEqual(previewHosts({ ...env, DEVKIT_PREVIEW_HOSTS: '*' }), ['a.test', 'b.test']);
    assert.throws(() => previewHosts({ ...env, DEVKIT_SERVICE: 'other' }));
    assert.throws(() => previewHosts({ ...env, DEVKIT_PREVIEW_HOSTS_SHA256: '0'.repeat(64) }));
    for (const host of ['127.1', '0x7f000001', '::ffff:127.0.0.1', '0:0:0:0:0:0:0:1']) {
      const noncanonical = Buffer.from(
        JSON.stringify({ schemaVersion: 1, service: 'web', hosts: [host] }),
      );
      fs.writeFileSync(source, noncanonical);
      assert.throws(
        () =>
          previewHosts({
            ...env,
            DEVKIT_PREVIEW_HOSTS_SHA256: createHash('sha256').update(noncanonical).digest('hex'),
          }),
        host,
      );
    }
    fs.unlinkSync(source);
    fs.symlinkSync('/etc/passwd', source);
    assert.throws(() => previewHosts(env));
  } finally {
    context.mock.restoreAll();
    fs.rmSync(root, { recursive: true, force: true });
  }
});
test('HMR seam is guarded by exact Vite package, version, fragment and routing', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'devkit-hmr-'));
  try {
    const packageRoot = path.join(root, 'vite');
    const client = path.join(packageRoot, 'dist/client/client.mjs');
    fs.mkdirSync(path.dirname(client), { recursive: true });
    fs.writeFileSync(path.join(packageRoot, 'package.json'), JSON.stringify({ version: '8.3.8' }));
    const candidates = fs.readdirSync(pnpmRoot).filter((name) => name.startsWith('vite@'));
    const source = fs.readFileSync(
      path.join(pnpmRoot, candidates[0], 'node_modules/vite/dist/client/client.mjs'),
      'utf8',
    );
    const shim = hmrRecovery();
    let warnings = 0;
    const warn = () => warnings++;
    shim.configure({}, true);
    assert.match(shim.transform(source, client, warn).code, /devkit:hmr-recovery:/u);
    assert.equal(shim.transform(source, `${client}?query`, warn), null);
    assert.equal(
      shim.transform(
        source.replace('await wsTransport.connect(handlers);', 'await changed();'),
        client,
        warn,
      ),
      null,
    );
    assert.equal(shim.transform('unknown', client, warn), null);
    assert.equal(warnings, 1);
    for (const server of [{ hmr: false }, { hmr: { host: 'custom' } }, { ws: { server: {} } }]) {
      shim.configure({ server }, true);
      assert.equal(shim.transform(source, client, warn), null);
    }
    shim.configure({ server: { hmr: { path: '/custom', timeout: 123, overlay: false } } }, true);
    assert.ok(shim.transform(source, client, warn));
    shim.configure({ server: { ws: { path: '/custom', timeout: 123 } } }, true);
    assert.ok(shim.transform(source, client, warn));
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('final resolved hosts and routing govern HTTPS and automatic websocket behavior', () => {
  environment({ DEVKIT_API_TARGET: 'http://api:8080' }, () => {
    const plugin = devkitWorkspaceSource('/no-project');
    plugin.config(
      { server: { allowedHosts: ['early.test'] } },
      { command: 'serve', mode: 'development' },
    );
    const config = {
      server: {
        allowedHosts: ['early.test', 'added.test'],
        ws: { host: 'custom.test', clientPort: 12345 },
      },
    };
    plugin.configResolved(config);
    plugin.configureServer.handler({ config });
    let forwarded = 0;
    const proxy = {
      ws() {
        forwarded++;
      },
    };
    config.server.proxy['/api'].configure(proxy);
    proxy.ws({ headers: { host: 'added.test' } }, {});
    assert.equal(forwarded, 1);
    let denied = '';
    proxy.ws(
      { headers: { host: 'hostile.test' } },
      {
        end(value) {
          denied = value;
        },
      },
    );
    assert.match(denied, /403/u);
    assert.equal(forwarded, 1);
    assert.equal(plugin.transform('', '/vite/dist/client/client.mjs'), null);
    let middleware;
    plugin.configureServer.handler({
      config: { server: { https: {}, allowedHosts: ['Later.test'], ws: {} } },
      middlewares: {
        use(handler) {
          middleware = handler;
        },
      },
    });
    let passed = false;
    middleware({ headers: { host: 'Later.test' } }, {}, () => {
      passed = true;
    });
    assert.equal(passed, true);
  });
});

test('a later Vite config hook owns its custom proxy options', async () => {
  const entry = fs.readdirSync(pnpmRoot).find((name) => name.startsWith('vite@'));
  const { createServer } = await import(
    pathToFileURL(path.join(pnpmRoot, entry, 'node_modules/vite/dist/node/index.js')).href
  );
  const headers = { 'x-custom': 'yes' };
  let options;
  const proxy = {
    target: 'http://127.0.0.1:8080',
    headers,
    configure(_instance, received) {
      options = received;
    },
  };
  const server = await createServer({
    configFile: false,
    server: { middlewareMode: true, watch: null, proxy: { '/api': 'http://api:8080' } },
    plugins: [
      devkitWorkspaceSource('/no-project'),
      {
        name: 'consumer-proxy',
        config: () => ({ server: { proxy: { '/api': proxy } } }),
      },
    ],
  });
  try {
    assert.equal(options, server.config.server.proxy['/api']);
    assert.equal(options.headers, headers);
    assert.equal(options.ws, undefined);
    assert.equal(options.changeOrigin, undefined);
  } finally {
    await server.close();
  }
});
