import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { ClientApp } from '__DEVKIT_CLIENT_PACKAGE__';
import { greet } from './api.js';
import '__DEVKIT_CLIENT_PACKAGE__/style.css';
const element = document.getElementById('root');
if (!element) {
  throw new Error('Missing application root.');
}
const root = createRoot(element);
window.desktop.getCapabilities().then(
  ({ apiEnabled }) =>
    root.render(
      <StrictMode>
        <ClientApp platform="desktop" {...(apiEnabled ? { api: { greet } } : {})} />
      </StrictMode>,
    ),
  (error: unknown) =>
    root.render(
      <p role="alert">
        {error instanceof Error ? error.message : 'Desktop initialization failed.'}
      </p>,
    ),
);
