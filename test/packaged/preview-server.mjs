// Test-only upstream and failing reverse proxy; production Vite plugin is consumed unchanged.
import fs from 'node:fs';
import http from 'node:http';
import https from 'node:https';
import net from 'node:net';
import { createRequire } from 'node:module';
import { pathToFileURL } from 'node:url';
import { createHash } from 'node:crypto';
const require = createRequire('/workspace/apps/web/package.json');
const viteEntry = require.resolve('vite');
const { createServer } = await import(pathToFileURL(viteEntry));
const { devkitWorkspaceSource } = await import(pathToFileURL(require.resolve('@docouno/env/vite')));
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
// The fixture needs only short echo/control frames. Keep it independent of
// unpublished Vite internals and packages outside the released consumer graph.
function acceptSocket(q, s) {
  s.on('error', () => s.destroy());
  const accept = createHash('sha1')
    .update(q.headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11')
    .digest('base64');
  s.write(
    'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ' +
      accept +
      (q.headers['sec-websocket-protocol']
        ? '\r\nSec-WebSocket-Protocol: ' + q.headers['sec-websocket-protocol']
        : '') +
      '\r\n\r\n',
  );
}
function echoSocket(q, s, head) {
  acceptSocket(q, s);
  let buffered = Buffer.alloc(0);
  const consume = (chunk) => {
    buffered = Buffer.concat([buffered, chunk]);
    if (buffered.length > 1024) return s.destroy();
    while (buffered.length >= 2) {
      const length = buffered[1] & 127;
      if (!(buffered[1] & 128) || length > 125) return s.destroy();
      if (buffered.length < 6 + length) return;
      const opcode = buffered[0] & 15;
      const payload = Buffer.from(buffered.subarray(6, 6 + length));
      for (let i = 0; i < length; i++) payload[i] ^= buffered[2 + (i % 4)];
      buffered = buffered.subarray(6 + length);
      if (opcode === 8) return s.end(Buffer.from([0x88, 0]));
      if ([1, 2, 9].includes(opcode))
        s.write(
          Buffer.concat([Buffer.from([0x80 | (opcode === 9 ? 10 : opcode), length]), payload]),
        );
    }
  };
  s.on('data', consume);
  if (head.length) consume(head);
}
const backend = http.createServer((q, s) => {
  backendRequests++;
  s.end('api');
});
backend.on('upgrade', (q, s, h) => {
  backendRequests++;
  echoSocket(q, s, h);
});
await new Promise((done) => backend.listen(9090, '0.0.0.0', done));
process.env.DEVKIT_PREVIEW_URL = 'https://preview.test';
async function start() {
  fs.writeFileSync(
    root + '/worker.js',
    `import '${base}@vite/client';postMessage({worker:typeof document==='undefined',nativeSocket:WebSocket.toString().includes('[native code]')});`,
  );
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
      acceptSocket(q, s);
      setTimeout(() => s.end(Buffer.from([0x88, 2, 3, 243])), 30);
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
for (const [server, port] of [
  [http.createServer(handler), 18112],
  [
    https.createServer(
      {
        key: fs.readFileSync('/evidence/preview-key.pem'),
        cert: fs.readFileSync('/evidence/preview-cert.pem'),
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
