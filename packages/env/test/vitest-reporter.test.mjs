import assert from 'node:assert/strict';
import test from 'node:test';
import RunnableTestsReporter from '../src/vitest-reporter.js';

function modules(...states) {
  return [
    {
      children: {
        *allTests() {
          for (const state of states) {
            yield { result: () => ({ state }) };
          }
        },
      },
    },
  ];
}

test('native failed and interrupted Vitest runs retain their primary failure', () => {
  const reporter = new RunnableTestsReporter();
  // Incomplete native data is deliberately not inspected on these failure paths.
  assert.doesNotThrow(() => reporter.onTestRunEnd(undefined, [], 'failed'));
  assert.doesNotThrow(() => reporter.onTestRunEnd(undefined, [], 'interrupted'));
  assert.doesNotThrow(() => reporter.onTestRunEnd(undefined, [new Error('native')], 'passed'));
});

test('only completed native Vitest cases count and individual skips remain valid', () => {
  const reporter = new RunnableTestsReporter();
  assert.throws(() => reporter.onTestRunEnd([], [], 'passed'), /No runnable Vitest/u);
  for (const states of [[], ['skipped'], ['pending'], ['skipped', 'pending']]) {
    assert.throws(
      () => reporter.onTestRunEnd(modules(...states), [], 'passed'),
      /No runnable Vitest/u,
    );
  }
  assert.doesNotThrow(() => reporter.onTestRunEnd(modules('skipped', 'passed'), [], 'passed'));
});
