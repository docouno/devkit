import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';
import { App } from './App.js';

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

it('works standalone without an API request', () => {
  const fetcher = vi.fn();
  vi.stubGlobal('fetch', fetcher);
  render(<App apiEnabled={false} />);
  fireEvent.change(screen.getByLabelText('Name'), { target: { value: ' Ada ' } });
  expect(screen.getByTestId('local-greeting').textContent).toBe('Hello, Ada!');
  fireEvent.click(screen.getByRole('button', { name: 'Increment' }));
  expect(screen.getByTestId('count').textContent).toBe('Clicks: 1');
  expect(screen.queryByRole('button', { name: 'Ask API' })).toBeNull();
  expect(fetcher).not.toHaveBeenCalled();
});

it('shows a validated API reply only when enabled', async () => {
  vi.stubGlobal('fetch', () =>
    Promise.resolve(new Response(JSON.stringify({ message: 'Hello from the API!' }))),
  );
  render(<App apiEnabled />);
  fireEvent.click(screen.getByRole('button', { name: 'Ask API' }));
  expect(await screen.findByText('Hello from the API!')).toBeDefined();
});

it('shows API errors instead of falling back to a local response', async () => {
  vi.stubGlobal('fetch', () => Promise.resolve(new Response('down', { status: 503 })));
  render(<App apiEnabled />);
  fireEvent.click(screen.getByRole('button', { name: 'Ask API' }));
  expect((await screen.findByRole('alert')).textContent).toContain('503');
});
