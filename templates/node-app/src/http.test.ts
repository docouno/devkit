import assert from 'node:assert/strict';
import { once } from 'node:events';
import { createConnection } from 'node:net';
import test from 'node:test';
import { createApplication } from './http.ts';

await test('serves the real API and exposes failure statuses', async () => {
  const server = createApplication();
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const address = server.address();
  assert.ok(address && typeof address !== 'string');
  const root = `http://127.0.0.1:${address.port}`;
  try {
    const health = await fetch(`${root}/health`);
    assert.equal(health.status, 200);
    assert.deepEqual(await health.json(), { status: 'ok' });
    const named = await fetch(`${root}/api/greeting?name=%20Ada%20`);
    assert.equal(named.status, 200);
    assert.deepEqual(await named.json(), { message: 'Hello, Ada!' });
    const empty = await fetch(`${root}/api/greeting?name=%20`);
    assert.deepEqual(await empty.json(), { message: 'Hello, world!' });
    const invalid = await fetch(`${root}/api/greeting?name=${'x'.repeat(81)}`);
    assert.equal(invalid.status, 400);
    const method = await fetch(`${root}/api/greeting`, { method: 'POST' });
    assert.equal(method.status, 405);
    assert.equal((await fetch(`${root}/missing`)).status, 404);
  } finally {
    await new Promise<void>((resolve, reject) => {
      server.close((error) => (error ? reject(error) : resolve()));
    });
  }
});

await test('a malformed raw request target returns 400 and keeps the server responsive', async () => {
  const server = createApplication();
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const address = server.address();
  assert.ok(address && typeof address !== 'string');
  try {
    const response = await new Promise<string>((resolve, reject) => {
      const socket = createConnection({ host: '127.0.0.1', port: address.port }, () => {
        socket.end('GET // HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n');
      });
      socket.setEncoding('utf8');
      socket.setTimeout(5_000, () => socket.destroy(new Error('Raw HTTP request timed out.')));
      let raw = '';
      socket.on('data', (chunk: string) => {
        raw += chunk;
      });
      socket.once('end', () => resolve(raw));
      socket.once('error', reject);
    });
    assert.match(response, /^HTTP\/1\.1 400 Bad Request\r\n/u);
    assert.deepEqual(await (await fetch(`http://127.0.0.1:${address.port}/health`)).json(), {
      status: 'ok',
    });
  } finally {
    await new Promise<void>((resolve, reject) => {
      server.close((error) => (error ? reject(error) : resolve()));
    });
  }
});
