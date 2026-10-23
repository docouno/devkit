import type { Reporter, TestModule, TestRunEndReason } from 'vitest/node';

export default class RunnableTestsReporter implements Reporter {
  onTestRunEnd(
    modules: ReadonlyArray<TestModule>,
    errors: ReadonlyArray<unknown>,
    reason: TestRunEndReason,
  ): void;
}
