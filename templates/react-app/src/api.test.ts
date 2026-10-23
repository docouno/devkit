import { describe, expect, it } from 'vitest';
import { fetchGreeting } from './api.js';

describe('typed API boundary', () => {
  it('encodes the request and validates a real response shape', async () => {
    let requested = '';
    const fetcher: typeof fetch = (input) => {
      requested = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;
      return Promise.resolve(
        new Response(JSON.stringify({ message: 'Hello, Ada!' }), { status: 200 }),
      );
    };
    await expect(fetchGreeting('Ada & Bob', { fetcher })).resolves.toEqual({
      message: 'Hello, Ada!',
    });
    expect(requested).toBe('/api/greeting?name=Ada+%26+Bob');
  });

  it('rejects HTTP, JSON and schema failures', async () => {
    const unavailable: typeof fetch = () =>
      Promise.resolve(new Response('unavailable', { status: 503 }));
    const invalidJson: typeof fetch = () => Promise.resolve(new Response('<html>not JSON</html>'));
    const invalidShape: typeof fetch = () =>
      Promise.resolve(new Response(JSON.stringify({ message: 42 })));
    await expect(fetchGreeting('Ada', { fetcher: unavailable })).rejects.toThrow('503');
    await expect(fetchGreeting('Ada', { fetcher: invalidJson })).rejects.toThrow('invalid JSON');
    await expect(fetchGreeting('Ada', { fetcher: invalidShape })).rejects.toThrow(
      'invalid greeting',
    );
  });
});
