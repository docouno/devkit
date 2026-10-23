// Test-only driver. Runs in an owned container, never in application source.
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { extname, resolve, sep } from 'node:path';

await mkdir(process.env.HOME, { recursive: true });
const mode = process.env.PROBE_MODE;
const output = '/evidence';
const delay = (ms) => new Promise((done) => setTimeout(done, ms));
const traces = [];
const runtimeEvents = [];
let child;
let server;
let socket;
let failure;
let childExit;
try {
  if (mode === 'production-web') {
    server = createServer((request, response) => {
      (async () => {
        const url = new URL(request.url, 'http://localhost');
        if (url.pathname.startsWith('/api/')) {
          const remote = await fetch(new URL(request.url, process.env.PROBE_API_TARGET));
          response.writeHead(remote.status, { 'content-type': 'application/json' });
          response.end(Buffer.from(await remote.arrayBuffer()));
          return;
        }
        const file = resolve('/web', '.' + (url.pathname === '/' ? '/index.html' : url.pathname));
        if (!file.startsWith('/web' + sep)) throw new Error('Invalid static asset path');
        const content = await readFile(file);
        const types = { '.js': 'text/javascript', '.css': 'text/css', '.html': 'text/html' };
        response.writeHead(200, {
          'content-type': types[extname(file)] || 'application/octet-stream',
        });
        response.end(content);
      })().catch((error) => {
        response.writeHead(500);
        response.end(String(error));
      });
    });
    await new Promise((done) => server.listen(5199, '127.0.0.1', done));
  }
  if (mode !== 'development-desktop') {
    const binary = mode === 'packaged-desktop' ? '/app/app' : '/electron/electron';
    const args = mode === 'packaged-desktop' ? [] : ['/harness/web-window.cjs'];
    child = spawn('xvfb-run', ['-a', binary, ...args, '--remote-debugging-port=9222'], {
      detached: true,
      env: {
        ...process.env,
        PROBE_BROWSER_URL:
          mode === 'production-web' ? 'http://127.0.0.1:5199' : 'http://127.0.0.1:5173',
        ...(mode === 'packaged-desktop' ? { DEVKIT_API_TARGET: process.env.PROBE_API_TARGET } : {}),
      },
    });
    child.on('exit', (code, signal) => {
      childExit = { code, signal };
    });
    child.on('error', (error) => {
      childExit = { error: String(error) };
    });
    child.stdout.on('data', (data) => traces.push(data.toString()));
    child.stderr.on('data', (data) => traces.push(data.toString()));
  }
  let target;
  for (let n = 0; n < 200; n += 1) {
    if (childExit)
      throw new Error(
        'Electron helper exited before DevTools: ' +
          JSON.stringify(childExit) +
          '\n' +
          traces.join('').slice(-4000),
      );
    try {
      target = (await (await fetch('http://127.0.0.1:9222/json/list')).json()).find(
        (item) => item.type === 'page',
      );
      if (target) break;
    } catch {
      /* Startup has not exposed DevTools yet. */
    }
    await delay(100);
  }
  if (!target) throw new Error('No Electron page target');
  socket = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((done, reject) => {
    socket.addEventListener('open', done, { once: true });
    socket.addEventListener('error', reject, { once: true });
  });
  let sequence = 0;
  const pending = new Map();
  socket.addEventListener('message', (event) => {
    const message = JSON.parse(event.data);
    const call = pending.get(message.id);
    if (!call) {
      if (['Log.entryAdded', 'Runtime.exceptionThrown'].includes(message.method))
        runtimeEvents.push(message);
      return;
    }
    clearTimeout(call.timer);
    pending.delete(message.id);
    if (message.error) call.reject(new Error(JSON.stringify(message.error)));
    else call.done(message.result);
  });
  const send = (method, params = {}) =>
    new Promise((done, reject) => {
      const id = ++sequence;
      const timer = setTimeout(() => {
        pending.delete(id);
        reject(new Error('CDP timeout: ' + method));
      }, 5000);
      pending.set(id, { done, reject, timer });
      socket.send(JSON.stringify({ id, method, params }));
    });
  await send('Runtime.enable');
  await send('Log.enable');
  const evaluate = async (expression) => {
    const value = await send('Runtime.evaluate', {
      expression,
      returnByValue: true,
      awaitPromise: true,
    });
    if (value.exceptionDetails) throw new Error(JSON.stringify(value.exceptionDetails));
    return value.result.value;
  };
  const readState = () =>
    evaluate(
      `({heading:document.querySelector('h1')?.textContent,text:document.body?.innerText||'',counter:document.querySelector('[data-testid=count]')?.textContent,apiGreeting:document.querySelector('[role=status]')?.textContent,node:typeof process,require:typeof require,url:location.href,bridge:Object.keys(window.desktop||{})})`,
    );
  let before;
  for (let n = 0; n < 200; n += 1) {
    before = await readState();
    if (before.heading === process.env.PROBE_HEADING) break;
    await delay(100);
  }
  if (before.heading !== process.env.PROBE_HEADING)
    throw new Error('Unexpected initial UI: ' + JSON.stringify(before));
  if (before.node !== 'undefined' || before.require !== 'undefined')
    throw new Error('Node globals leaked to renderer');
  if (mode.includes('desktop') && before.bridge.join(',') !== 'getCapabilities,greet')
    throw new Error('Unexpected desktop bridge');
  const name = process.env.PROBE_NAME;
  await evaluate(
    `(() => { const input=document.getElementById('name'); Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(input,${JSON.stringify(name)}); input.dispatchEvent(new Event('input',{bubbles:true})); })()`,
  );
  await evaluate(
    `Array.from(document.querySelectorAll('button')).find(button=>button.textContent==='Increment').click()`,
  );
  await evaluate(
    `Array.from(document.querySelectorAll('button')).find(button=>button.textContent==='Ask API').click()`,
  );
  let initial;
  for (let n = 0; n < 200; n += 1) {
    initial = await readState();
    if (initial.apiGreeting === 'Hello, ' + name + '!' && initial.counter === 'Clicks: 1') break;
    await delay(100);
  }
  if (initial.apiGreeting !== 'Hello, ' + name + '!' || initial.counter !== 'Clicks: 1')
    throw new Error('Greeting or local interaction failed: ' + JSON.stringify(initial));
  await writeFile(output + '/ready.json', JSON.stringify({ mode, initial }, null, 2));
  let after = initial;
  if (process.env.PROBE_HMR_HEADING) {
    for (let n = 0; n < 400; n += 1) {
      after = await readState();
      if (after.heading === process.env.PROBE_HMR_HEADING) break;
      await delay(100);
    }
    if (after.heading !== process.env.PROBE_HMR_HEADING || after.counter !== 'Clicks: 1')
      throw new Error('HMR did not preserve live state: ' + JSON.stringify(after));
  }
  await delay(300);
  const layout = await send('Page.getLayoutMetrics');
  const size = layout.cssContentSize;
  const screenshot = await send('Page.captureScreenshot', {
    format: 'png',
    captureBeyondViewport: true,
    clip: { x: 0, y: 0, width: size.width, height: size.height, scale: 1 },
  });
  await writeFile(output + '/screenshot.png', Buffer.from(screenshot.data, 'base64'));
  await writeFile(output + '/result.json', JSON.stringify({ mode, initial, after }, null, 2));
  console.log(JSON.stringify({ mode, initial, after }));
} catch (error) {
  failure = error;
  console.error(error);
} finally {
  socket?.close();
  if (child?.pid) {
    try {
      process.kill(-child.pid, 'SIGTERM');
    } catch {
      /* Already exited. */
    }
  }
  if (server) await new Promise((done) => server.close(done));
  await writeFile(output + '/browser.log', traces.join(''));
  await writeFile(output + '/runtime-events.json', JSON.stringify(runtimeEvents, null, 2));
}
if (failure) process.exitCode = 1;
