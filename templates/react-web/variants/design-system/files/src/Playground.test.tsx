import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, expect, it } from 'vitest';
import { Playground } from './Playground.js';

afterEach(cleanup);

it('renders UI library buttons and updates state', () => {
  render(<Playground />);
  const increment = screen.getByRole('button', { name: 'Increment (0)' });
  fireEvent.click(increment);
  expect(screen.getByRole('button', { name: 'Increment (1)' })).toBeDefined();
  fireEvent.click(screen.getByRole('button', { name: 'Reset' }));
  expect(screen.getByRole('button', { name: 'Increment (0)' })).toBeDefined();
});
