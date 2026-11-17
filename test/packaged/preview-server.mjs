// Test-only upstream and failing reverse proxy; production Vite plugin is consumed unchanged.
import fs from 'node:fs';
import http from 'node:http';
import https from 'node:https';
import net from 'node:net';
import { createRequire } from 'node:module';
import { pathToFileURL } from 'node:url';
import { execFileSync } from 'node:child_process';
const require = createRequire('/workspace/apps/web/package.json');
const viteEntry = require.resolve('vite');
const { createServer } = await import(pathToFileURL(viteEntry));
const { devkitWorkspaceSource } = await import(pathToFileURL(require.resolve('@docouno/env/vite')));
const { WebSocketServer } = await import(pathToFileURL(createRequire(viteEntry).resolve('ws')));
const root = '/tmp/devkit-preview-app';
fs.mkdirSync(root, { recursive: true });
fs.writeFileSync(
  root + '/index.html',
  '<!doctype html><html><body><h1>starting</h1><script type="module" src="/main.js"></script></body></html>',
);
const content = (revision) =>
  `import './style.css';window.live=(window.live||0)+1;document.querySelector('h1').textContent='v${revision}';if(import.meta.hot)import.meta.hot.accept();`;
let revision = 1,
  vite,
  drop = 'none',
  base = '/',
  backendRequests = 0,
  hmrAttempts = 0,
  pingAttempts = 0;
fs.writeFileSync(root + '/main.js', content(revision));
fs.writeFileSync(root + '/style.css', 'h1{color:rgb(1, 2, 3)}');
const echo = new WebSocketServer({ noServer: true });
const rejected = new WebSocketServer({ noServer: true });
const backend = http.createServer((q, s) => {
  backendRequests++;
  s.end('api');
});
backend.on('upgrade', (q, s, h) => {
  backendRequests++;
  echo.handleUpgrade(q, s, h, (ws) => ws.on('message', (data) => ws.send(data)));
});
await new Promise((done) => backend.listen(9090, '0.0.0.0', done));
process.env.DEVKIT_PREVIEW_URL = 'https://preview.test';
async function start() {
  vite = await createServer({
    configFile: false,
    root,
    base,
    cacheDir: '/tmp/devkit-preview-cache',
    plugins: [devkitWorkspaceSource(root)],
    server: {
      host: '0.0.0.0',
      port: 5173,
      strictPort: true,
      ...(base === '/' ? {} : { hmr: { path: '/socket/' } }),
      proxy: { '/api': 'http://127.0.0.1:9090' },
    },
    logLevel: 'warn',
  });
  await vite.listen();
}
await start();
const handler = async (q, s) => {
  const url = new URL(q.url, 'http://test');
  if (url.pathname === '/__control') {
    drop = url.searchParams.get('drop') || 'none';
    base = url.searchParams.get('base') || '/';
    await vite.close();
    hmrAttempts = 0;
    pingAttempts = 0;
    await start();
    s.end('ready');
    return;
  }
  if (url.pathname === '/__restart') {
    s.end('restarting');
    await vite.close();
    setTimeout(() => start(), 700);
    return;
  }
  if (url.pathname === '/__edit') {
    revision++;
    fs.writeFileSync(root + '/main.js', content(revision));
    fs.writeFileSync(root + '/style.css', 'h1{color:rgb(4, 5, 6)}');
    s.end(String(revision));
    return;
  }
  if (url.pathname === '/__trace') {
    s.end(JSON.stringify({ hmrAttempts, pingAttempts, backendRequests }));
    return;
  }
  const upstream = http.request(
    { host: '127.0.0.1', port: 5173, path: q.url, headers: q.headers },
    (response) => {
      s.writeHead(response.statusCode, response.headers);
      response.pipe(s);
    },
  );
  upstream.on('error', () => {
    s.writeHead(502);
    s.end();
  });
  q.pipe(upstream);
};
function upgrade(q, s, h) {
  const protocol = q.headers['sec-websocket-protocol'];
  if (protocol === 'vite-hmr') {
    hmrAttempts++;
    if (drop === 'once') {
      drop = 'none';
      s.destroy();
      return;
    }
    if (drop === 'always') {
      s.destroy();
      return;
    }
    if (drop === 'openclose' || drop === 'reject-then-openclose') {
      if (drop === 'reject-then-openclose' && hmrAttempts === 1) {
        s.destroy();
        return;
      }
      rejected.handleUpgrade(q, s, h, (ws) =>
        setTimeout(() => ws.close(1011, 'upstream unavailable'), 30),
      );
      return;
    }
  }
  if (protocol === 'vite-ping') pingAttempts++;
  const upstream = net.connect(5173, '127.0.0.1', () => {
    upstream.write(
      `${q.method} ${q.url} HTTP/${q.httpVersion}\r\n${q.rawHeaders.reduce((raw, value, index, all) => (index % 2 === 0 ? raw + value + ': ' + all[index + 1] + '\r\n' : raw), '')}\r\n`,
    );
    if (h.length) upstream.write(h);
    upstream.pipe(s);
    s.pipe(upstream);
  });
  s.on('close', () => upstream.destroy());
  upstream.on('error', () => s.destroy());
}
execFileSync(
  'openssl',
  [
    'req',
    '-x509',
    '-newkey',
    'rsa:2048',
    '-nodes',
    '-days',
    '1',
    '-subj',
    '/CN=preview.test',
    '-keyout',
    '/tmp/preview-key.pem',
    '-out',
    '/tmp/preview-cert.pem',
  ],
  { stdio: 'ignore' },
);
for (const [server, port] of [
  [http.createServer(handler), 18112],
  [
    https.createServer(
      {
        key: fs.readFileSync('/tmp/preview-key.pem'),
        cert: fs.readFileSync('/tmp/preview-cert.pem'),
      },
      handler,
    ),
    443,
  ],
]) {
  server.on('upgrade', upgrade);
  await new Promise((done) => server.listen(port, '0.0.0.0', done));
}
fs.writeFileSync(
  '/evidence/server-ready.json',
  JSON.stringify({
    vite: require('vite/package.json').version,
    environment: require.resolve('@docouno/env/vite'),
  }),
);
