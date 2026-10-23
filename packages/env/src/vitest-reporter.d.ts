interface RunnableTestCase {
  result(): { readonly state: string };
}

interface RunnableTestModule {
  readonly children: {
    allTests(): Iterable<RunnableTestCase>;
  };
}

type RunEndReason = 'passed' | 'interrupted' | 'failed';

export default class RunnableTestsReporter {
  onTestRunEnd(
    modules: ReadonlyArray<RunnableTestModule>,
    errors: ReadonlyArray<unknown>,
    reason: RunEndReason,
  ): void;
}
