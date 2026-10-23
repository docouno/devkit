import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { ClientApp } from './ClientApp.js';
import { greet } from './api.js';
import './style.css';
const element = document.getElementById('root');
if (!element) {
  throw new Error('Missing application root.');
}
const api = import.meta.env.VITE_API_ENABLED === 'true' ? { greet } : undefined;
createRoot(element).render(
  <StrictMode>
    <ClientApp platform="web" {...(api ? { api } : {})} />
  </StrictMode>,
);
