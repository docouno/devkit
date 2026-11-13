import os from 'node:os';
import process from 'node:process';

function parsePositiveInt(raw) {
  if (typeof raw !== 'string' || !/^[1-9]\d*$/u.test(raw.trim())) {
    return undefined;
  }
  const parsed = Number.parseInt(raw.trim(), 10);
  return Number.isSafeInteger(parsed) && parsed >= 1 ? parsed : undefined;
}

function applyConfigBounds(cfg, workerCap, workerHeapMb) {
  if (!cfg || typeof cfg !== 'object') {
    return;
  }
  if (cfg.fileParallelism !== false) {
    cfg.maxWorkers =
      typeof cfg.maxWorkers === 'number' && cfg.maxWorkers >= 1
        ? Math.min(cfg.maxWorkers, workerCap)
        : workerCap;
  }
  const pool = cfg.pool ?? 'forks';
  if (workerHeapMb && (pool === 'forks' || pool === 'vmForks')) {
    const current = Array.isArray(cfg.execArgv) ? cfg.execArgv : [];
    const hasHeapCap = current.some(
      (arg) => typeof arg === 'string' && arg.startsWith('--max-old-space-size'),
    );
    if (!hasHeapCap) {
      cfg.execArgv = [...current, `--max-old-space-size=${workerHeapMb}`];
    }
  }
}

export default class RunnableTestsReporter {
  onInit(vitest) {
    this.runnableFiles = 0;
    this.runnableTests = 0;
    try {
      const fallbackParallelism =
        typeof os.availableParallelism === 'function' ? os.availableParallelism() : 2;
      const workerCap =
        parsePositiveInt(process.env.DEVKIT_TEST_WORKERS) ??
        Math.min(Math.max(fallbackParallelism - 1, 1), 4);
      const workerHeapMb = parsePositiveInt(process.env.DEVKIT_WORKER_MAX_OLD_SPACE_SIZE);
      const targets = [vitest, ...(Array.isArray(vitest?.projects) ? vitest.projects : [])];
      for (const target of targets) {
        applyConfigBounds(target?.config, workerCap, workerHeapMb);
        applyConfigBounds(target?.serializedConfig, workerCap, workerHeapMb);
      }
    } catch {
      // Best-effort runtime bounds; never block test execution.
    }
  }

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
