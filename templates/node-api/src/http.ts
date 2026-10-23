import { createServer } from 'node:http';
import type { Server, ServerResponse } from 'node:http';
import { z } from 'zod';
import { greeting } from './greeting.ts';

function send(response: ServerResponse, status: number, body: Record<string, unknown>): void {
  response.writeHead(status, {
    'content-type': 'application/json; charset=utf-8',
    'cache-control': 'no-store',
  });
  response.end(`${JSON.stringify(body)}\n`);
}

export function createApplication(): Server {
  return createServer((request, response) => {
    let url: URL;
    try {
      url = new URL(request.url ?? '/', 'http://localhost');
    } catch {
      send(response, 400, { error: 'Invalid request target.' });
      return;
    }
    if (!['/health', '/api/greeting'].includes(url.pathname)) {
      send(response, 404, { error: 'Not found.' });
      return;
    }
    if (request.method !== 'GET') {
      response.setHeader('allow', 'GET');
      send(response, 405, { error: 'Method not allowed.' });
      return;
    }
    if (url.pathname === '/health') {
      send(response, 200, { status: 'ok' });
      return;
    }
    try {
      send(response, 200, greeting(url.searchParams.get('name') ?? undefined));
    } catch (error) {
      if (error instanceof z.ZodError) {
        send(response, 400, { error: 'Name must contain at most 80 characters.' });
      } else {
        send(response, 500, { error: 'Unable to produce a greeting.' });
      }
    }
  });
}
