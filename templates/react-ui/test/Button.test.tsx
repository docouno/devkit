import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, expect, it } from 'vitest';
import { Button } from '../src/index.js';

afterEach(cleanup);

it('renders label and forwards click events', () => {
  let clicked = 0;
  render(
    <Button
      variant="secondary"
      onClick={() => {
        clicked += 1;
      }}
    >
      Save changes
    </Button>,
  );
  const button = screen.getByRole('button', { name: 'Save changes' });
  expect(button.getAttribute('data-variant')).toBe('secondary');
  fireEvent.click(button);
  expect(clicked).toBe(1);
});
