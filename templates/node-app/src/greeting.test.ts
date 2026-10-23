import assert from 'node:assert/strict';
import test from 'node:test';
import { greeting } from './greeting.ts';

await test('normalizes a name and gives an explicit default', () => {
  assert.deepEqual(greeting('  Ada  '), { message: 'Hello, Ada!' });
  assert.deepEqual(greeting('  '), { message: 'Hello, world!' });
  assert.deepEqual(greeting(undefined), { message: 'Hello, world!' });
});

await test('rejects invalid input instead of coercing it silently', () => {
  assert.throws(() => greeting(42));
  assert.throws(() => greeting('x'.repeat(81)));
});
