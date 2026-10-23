export default class RunnableTestsReporter {
  onTestRunEnd(modules, errors, reason) {
    if (reason !== 'passed' || errors.length > 0) {
      return;
    }
    for (const module of modules) {
      for (const test of module.children.allTests()) {
        if (['passed', 'failed'].includes(test.result().state)) {
          return;
        }
      }
    }
    throw new Error(
      'No runnable Vitest cases finished; empty, skipped and TODO-only suites are not accepted.',
    );
  }
}
