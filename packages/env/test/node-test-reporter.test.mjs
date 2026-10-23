import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const reporter = fileURLToPath(new URL('../src/node-test-reporter.js', import.meta.url));

async function run(source) {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'devkit native tests '));
  const file = path.join(directory, 'probe.test.mjs');
  await fs.writeFile(file, source);
  try {
    const env = { ...process.env };
    // The outer runner's worker marker disables the nested CLI's normal runner.
    delete env.NODE_TEST_CONTEXT;
    return spawnSync(process.execPath, ['--test', `--test-reporter=${reporter}`, file], {
      encoding: 'utf8',
      timeout: 15_000,
      env,
    });
  } finally {
    await fs.rm(directory, { recursive: true, force: true });
  }
}

test('registered native tests pass and retain the normal output', async () => {
  const result = await run("import test from 'node:test';\nawait test('real case', () => {});\n");
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /real case/u);
});

for (const [name, source] of [
  ['empty module', 'export {};\n'],
  ['skipped suite', "import test from 'node:test';\nawait test.skip('skipped', () => {});\n"],
  ['TODO suite', "import test from 'node:test';\nawait test.todo('pending');\n"],
]) {
  test(`${name} does not count as runnable native coverage`, async () => {
    const result = await run(source);
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /No runnable node:test/u);
  });
}

test('native assertion failure remains a failure', async () => {
  const result = await run(
    "import test from 'node:test';\nimport assert from 'node:assert/strict';\nawait test('broken case', () => assert.equal(1, 2));\n",
  );
  assert.notEqual(result.status, 0);
  assert.match(`${result.stdout}${result.stderr}`, /broken case|AssertionError/u);
});
