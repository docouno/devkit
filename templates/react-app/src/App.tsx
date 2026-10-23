import { useEffect, useRef, useState } from 'react';
import { fetchGreeting } from './api.js';

type RemoteState =
  | { phase: 'idle' }
  | { phase: 'loading' }
  | { phase: 'ready'; message: string }
  | { phase: 'error'; message: string };

function ApiGreeting({ name }: { name: string }) {
  const [state, setState] = useState<RemoteState>({ phase: 'idle' });
  const active = useRef<AbortController | null>(null);
  useEffect(() => () => active.current?.abort(), []);
  function requestGreeting() {
    active.current?.abort();
    const controller = new AbortController();
    active.current = controller;
    setState({ phase: 'loading' });
    fetchGreeting(name, { signal: controller.signal }).then(
      (reply) => {
        if (!controller.signal.aborted) {
          setState({ phase: 'ready', message: reply.message });
        }
      },
      (error: unknown) => {
        if (!controller.signal.aborted) {
          setState({
            phase: 'error',
            message: error instanceof Error ? error.message : 'API request failed.',
          });
        }
      },
    );
  }
  return (
    <section className="api-panel" aria-label="API greeting">
      <h2>API connection</h2>
      <p>This action calls the configured server. Failures are shown explicitly.</p>
      <button type="button" onClick={requestGreeting} disabled={state.phase === 'loading'}>
        {state.phase === 'loading' ? 'Requesting…' : 'Ask API'}
      </button>
      {state.phase === 'ready' && <p role="status">{state.message}</p>}
      {state.phase === 'error' && <p role="alert">{state.message}</p>}
    </section>
  );
}

export function App({
  apiEnabled = import.meta.env.VITE_API_ENABLED === 'true',
}: {
  apiEnabled?: boolean;
}) {
  const [name, setName] = useState('world');
  const [count, setCount] = useState(0);
  return (
    <main>
      <p className="eyebrow">Devkit · React + TypeScript</p>
      <h1>A small app, ready to change.</h1>
      <p>Start with a working interaction, typed code and checks that run independently.</p>
      <section className="card" aria-label="Local interaction">
        <label htmlFor="name">Name</label>
        <input
          id="name"
          value={name}
          maxLength={80}
          onChange={(event) => setName(event.target.value)}
        />
        <output data-testid="local-greeting">Hello, {name.trim() || 'world'}!</output>
        <button type="button" onClick={() => setCount((value) => value + 1)}>
          Increment
        </button>
        <span data-testid="count">Clicks: {count}</span>
      </section>
      {apiEnabled && <ApiGreeting name={name} />}
    </main>
  );
}
