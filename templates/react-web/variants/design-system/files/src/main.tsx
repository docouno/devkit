import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import '__DEVKIT_UI_PACKAGE__/styles.css';
import { Playground } from './Playground.js';

const element = document.getElementById('root');
if (!element) {
  throw new Error('Missing application root.');
}

createRoot(element).render(
  <StrictMode>
    <Playground />
  </StrictMode>,
);
