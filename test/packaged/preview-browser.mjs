// Real Chromium renderer in a separate namespace from the Vite/proxy container.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import { spawn } from 'node:child_process';
const address = process.env.PROBE_PROXY_ADDRESS;
fs.mkdirSync(process.env.HOME, { recursive: true });
const delay = (ms) => new Promise((done) => setTimeout(done, ms));
const traces = [],
  results = [],
  diagnostics = [];
let currentCase;
const request = (route, host) =>
  new Promise((done, reject) => {
    const q = http.get(
      { host: address, port: 18112, path: route, headers: { host: host || 'preview.test' } },
      (s) => {
        let body = '';
        s.on('data', (data) => (body += data));
        s.on('end', () => done({ status: s.statusCode, body }));
      },
    );
    q.on('error', reject);
  });
const sticky = http.createServer((q, s) => {
  const upstream = http.request(
    { host: address, port: 18112, path: q.url, headers: q.headers },
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
});
sticky.on('upgrade', (q, s, h) => {
  const upstream = net.connect(18112, address, () => {
    upstream.write(
      `${q.method} ${q.url} HTTP/${q.httpVersion}\r\n${q.rawHeaders.reduce((raw, value, index, all) => (index % 2 === 0 ? raw + value + ': ' + all[index + 1] + '\r\n' : raw), '')}\r\n`,
    );
    if (h.length) upstream.write(h);
    upstream.pipe(s);
    s.pipe(upstream);
  });
  s.on('error', () => s.destroy());
  s.on('close', () => upstream.destroy());
  upstream.on('error', () => s.destroy());
});
await new Promise((done) => sticky.listen(18113, '127.0.0.1', done));
const child = spawn(
  'xvfb-run',
  [
    '-a',
    '/electron/electron',
    '/harness/web-window.cjs',
    '--remote-debugging-port=9222',
    '--ignore-certificate-errors',
  ],
  { env: { ...process.env, PROBE_BROWSER_URL: 'about:blank' }, detached: true },
);
child.stdout.on('data', (data) => traces.push(data.toString()));
child.stderr.on('data', (data) => traces.push(data.toString()));
let socket;
try {
  let target;
  for (let i = 0; i < 150; i++) {
    try {
      target = (await (await fetch('http://127.0.0.1:9222/json/list')).json()).find(
        (t) => t.type === 'page',
      );
      if (target) break;
    } catch {}
    await delay(100);
  }
  assert.ok(target, 'Electron page did not start: ' + traces.join('').slice(-2000));
  socket = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((done, reject) => {
    socket.addEventListener('open', done, { once: true });
    socket.addEventListener('error', reject, { once: true });
  });
  let sequence = 0;
  const pending = new Map();
  let loads = 0,
    connected = 0,
    stoppedErrors = 0;
  const errors = [];
  socket.addEventListener('message', (event) => {
    const message = JSON.parse(event.data);
    const call = pending.get(message.id);
    if (call) {
      clearTimeout(call.timer);
      pending.delete(message.id);
      if (message.error) call.reject(Error(JSON.stringify(message.error)));
      else call.done(message.result);
    }
    if (message.method === 'Page.loadEventFired') loads++;
    if (
      ['Network.webSocketCreated', 'Runtime.consoleAPICalled', 'Runtime.exceptionThrown'].includes(
        message.method,
      ) &&
      diagnostics.length < 1000
    )
      diagnostics.push({ method: message.method, params: message.params });
    if (
      message.method === 'Network.webSocketFrameReceived' &&
      message.params.response.payloadData.includes('"type":"connected"')
    )
      connected++;
    if (
      message.method === 'Runtime.consoleAPICalled' &&
      message.params.args?.some(
        (arg) =>
          typeof arg.value === 'string' && arg.value.startsWith('[devkit] HMR recovery stopped'),
      )
    )
      stoppedErrors++;
    if (message.method === 'Runtime.exceptionThrown')
      errors.push(message.params.exceptionDetails.text);
  });
  const send = (method, params = {}) =>
    new Promise((done, reject) => {
      const id = ++sequence;
      const timer = setTimeout(() => reject(Error('CDP timeout ' + method)), 5000);
      pending.set(id, { done, reject, timer });
      socket.send(JSON.stringify({ id, method, params }));
    });
  const evaluate = async (expression) => {
    const result = await send('Runtime.evaluate', {
      expression,
      returnByValue: true,
      awaitPromise: true,
    });
    if (result.exceptionDetails) throw Error(JSON.stringify(result.exceptionDetails));
    return result.result.value;
  };
  for (const method of ['Page.enable', 'Runtime.enable', 'Network.enable']) await send(method);
  // Both HTTP and WebSocket hostile Host must be rejected before backend IO.
  assert.equal((await request('/api/hostile', 'hostile.example.test')).status, 403);
  const denied = await new Promise((done, reject) => {
    const s = net.connect(18112, address, () =>
      s.write(
        'GET /api/echo HTTP/1.1\r\nHost: hostile.example.test\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n',
      ),
    );
    s.setTimeout(3000, () => {
      s.destroy();
      reject(Error('Host guard timeout'));
    });
    s.once('data', (data) => {
      done(data.toString());
      s.destroy();
    });
    s.on('error', reject);
  });
  assert.match(denied, /403/u);
  assert.equal(JSON.parse((await request('/__trace')).body).backendRequests, 0);
  for (const origin of ['http://127.0.0.1:18113', 'https://preview.test']) {
    for (const drop of [
      'none',
      'once',
      'stale',
      'always',
      'openclose',
      'reject-then-openclose',
      'base',
      'corrupt',
      'unavailable',
    ]) {
      currentCase = { origin, drop };
      await send('Page.navigate', { url: 'about:blank' });
      await delay(50);
      await request(
        '/__control?drop=' +
          (drop === 'base' ? 'once' : ['corrupt', 'unavailable'].includes(drop) ? 'always' : drop) +
          (drop === 'base' ? '&base=/base/' : ''),
      );
      loads = 0;
      connected = 0;
      stoppedErrors = 0;
      let injection;
      if (['corrupt', 'unavailable'].includes(drop)) {
        const source =
          drop === 'unavailable'
            ? `Object.defineProperty(window,'sessionStorage',{get(){throw new Error('test storage unavailable')}});`
            : `sessionStorage.setItem('devkit:hmr-recovery:'+(location.protocol==='https:'?'wss':'ws')+'://'+location.hostname+':'+location.port+'/', '{invalid');`;
        injection = (await send('Page.addScriptToEvaluateOnNewDocument', { source })).identifier;
      }
      await send('Page.navigate', { url: origin + (drop === 'base' ? '/base/' : '') });
      await delay(1700);
      if (drop === 'none') {
        await request('/__restart');
        await delay(1800);
        assert.ok(loads >= 2 && connected >= 2, 'Ordinary restart did not reconnect');
      }
      const before = loads;
      const successful = ['none', 'once', 'stale', 'base'].includes(drop);
      if (successful) {
        assert.ok(connected > 0, `No actual connected frame: ${origin} ${drop}`);
        if (drop === 'stale') {
          const trace = JSON.parse((await request('/__trace')).body);
          assert.ok(
            trace.rotatedToken && trace.staleStatus === 400,
            'Vite did not reject the original token after restart',
          );
          assert.ok(
            loads >= 2 && trace.hmrAttempts >= 2,
            'Browser never retried with the new client',
          );
        }
        const api = await evaluate(`(async () => {
          const response = await fetch('/api/health');
          return [response.status, await response.text()];
        })()`);
        assert.deepEqual(api, [200, 'api']);
        const echo = await evaluate(`new Promise((resolve, reject) => {
          const url = new URL('/api/echo', location.href);
          url.protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
          const socket = new WebSocket(url);
          const timer = setTimeout(() => { socket.close(); reject(new Error('API WS echo timeout')); }, 3000);
          socket.onopen = () => socket.send('genuine');
          socket.onmessage = (event) => { clearTimeout(timer); socket.close(); resolve(event.data); };
          socket.onerror = () => { clearTimeout(timer); reject(new Error('API WS error')); };
        })`);
        assert.equal(echo, 'genuine');
        const color = "getComputedStyle(document.querySelector('h1')).color";
        const previousColor = await evaluate(color);
        const revision = (await request('/__edit')).body;
        await delay(650);
        assert.equal(await evaluate("document.querySelector('h1')?.textContent"), 'v' + revision);
        const nextColor = await evaluate(color);
        assert.notEqual(nextColor, previousColor);
        assert.equal(nextColor, `rgb(${(Number(revision) % 250) + 1}, 2, 3)`);
        assert.equal(loads, before, 'Hot JS/CSS edit navigated the page');
        assert.equal(
          await evaluate(
            "Object.keys(sessionStorage).filter(k=>k.startsWith('devkit:hmr-recovery:')).length",
          ),
          0,
        );
      } else {
        await delay(600);
        assert.equal(loads, before, 'Permanent denial caused an unbounded reload loop');
        assert.ok(loads <= 2, 'More than one automatic reload per token');
        assert.equal(stoppedErrors, 1, 'Recovery must stop with one actionable error');
        if (['corrupt', 'unavailable'].includes(drop))
          assert.equal(loads, 1, 'Unusable storage must not trigger reload');
      }
      let worker;
      if (drop === 'none') {
        worker = await evaluate(
          `new Promise((resolve,reject)=>{const worker=new Worker(new URL('worker.js',location.href),{type:'module'});const timeout=setTimeout(()=>{worker.terminate();reject(new Error('Worker timeout'));},4000);worker.onmessage=event=>{clearTimeout(timeout);worker.terminate();resolve(event.data)};worker.onerror=event=>{clearTimeout(timeout);worker.terminate();reject(new Error(event.message))};})`,
        );
        assert.deepEqual(worker, { worker: true, nativeSocket: true });
      }
      results.push({
        origin,
        drop,
        loads,
        connected,
        stoppedErrors,
        worker,
        hotUpdateWithoutNavigation: successful,
        browserApiHttpAndWs: successful,
      });
      console.log(JSON.stringify(results.at(-1)));
      // Navigation does not imply a fresh session: clear only between independent cases.
      if (drop !== 'unavailable') await evaluate('sessionStorage.clear()');
      if (injection)
        await send('Page.removeScriptToEvaluateOnNewDocument', { identifier: injection });
    }
  }
  assert.deepEqual(errors, []);
  fs.writeFileSync('/evidence/result.json', JSON.stringify({ passed: true, results }, null, 2));
  console.log(JSON.stringify(results));
} catch (error) {
  console.error(error);
  fs.writeFileSync(
    '/evidence/result.json',
    JSON.stringify(
      { passed: false, error: String(error), currentCase, results, diagnostics },
      null,
      2,
    ),
  );
  process.exitCode = 1;
} finally {
  socket?.close();
  try {
    process.kill(-child.pid, 'SIGTERM');
  } catch {}
  sticky.close();
  fs.writeFileSync('/evidence/browser.log', traces.join(''));
}
