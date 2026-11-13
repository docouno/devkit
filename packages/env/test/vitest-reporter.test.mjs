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

test('onInit bounds maxWorkers and fork heap without breaking threads or user serialization', () => {
  const savedWorkers = process.env.DEVKIT_TEST_WORKERS;
  const savedHeap = process.env.DEVKIT_WORKER_MAX_OLD_SPACE_SIZE;
  try {
    process.env.DEVKIT_TEST_WORKERS = '2';
    process.env.DEVKIT_WORKER_MAX_OLD_SPACE_SIZE = '384';

    const reporter = new RunnableTestsReporter();
    const forksTarget = { config: { maxWorkers: 8, pool: 'forks' } };
    reporter.onInit(forksTarget);
    assert.equal(forksTarget.config.maxWorkers, 2);
    assert.deepEqual(forksTarget.config.execArgv, ['--max-old-space-size=384']);

    const explicitSingle = { config: { maxWorkers: 1, pool: 'vmForks' } };
    process.env.DEVKIT_TEST_WORKERS = '4';
    reporter.onInit(explicitSingle);
    assert.equal(explicitSingle.config.maxWorkers, 1);
    assert.deepEqual(explicitSingle.config.execArgv, ['--max-old-space-size=384']);

    const noParallelism = { config: { fileParallelism: false, maxWorkers: 1, pool: 'forks' } };
    reporter.onInit(noParallelism);
    assert.equal(noParallelism.config.fileParallelism, false);
    assert.equal(noParallelism.config.maxWorkers, 1);

    const threadsTarget = { config: { pool: 'threads', maxWorkers: 8 } };
    reporter.onInit(threadsTarget);
    assert.equal(threadsTarget.config.maxWorkers, 4);
    assert.equal(threadsTarget.config.execArgv, undefined);
  } finally {
    if (savedWorkers === undefined) {
      delete process.env.DEVKIT_TEST_WORKERS;
    } else {
      process.env.DEVKIT_TEST_WORKERS = savedWorkers;
    }
    if (savedHeap === undefined) {
      delete process.env.DEVKIT_WORKER_MAX_OLD_SPACE_SIZE;
    } else {
      process.env.DEVKIT_WORKER_MAX_OLD_SPACE_SIZE = savedHeap;
    }
  }
});
