import { Readable } from 'node:stream';
import { spec as Spec } from 'node:test/reporters';

async function* checked(events) {
  let runnable = 0;
  let summarySeen = false;
  for await (const event of events) {
    if (event.type === 'test:summary') {
      const { counts, file, success } = event.data;
      if (file) {
        if (![counts.tests, counts.skipped, counts.todo].every(Number.isInteger)) {
          throw new Error('Unsupported node:test summary; runnable test coverage is unknown.');
        }
        runnable += counts.tests - counts.skipped - counts.todo;
      } else {
        summarySeen = true;
        if (success && runnable < 1) {
          throw new Error(
            'No runnable node:test cases were registered; empty, skipped and TODO-only suites are not accepted.',
          );
        }
      }
    }
    yield event;
  }
  if (!summarySeen) {
    throw new Error('No final node:test summary was received.');
  }
}

export default async function* nodeTestReporter(events) {
  yield* Readable.from(checked(events)).compose(new Spec());
}
