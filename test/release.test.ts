import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { assertRuntimeNotices } from '../tooling/release.ts';

await test('release requires all third-party runtime notices independently of its file map', () => {
  const root = mkdtempSync(join(tmpdir(), 'devkit-notices-'));
  const names = ['Apache-2.0.txt', 'moby-profiles.NOTICE', 'yaml.txt', 'smol-toml.txt'];
  const path = (name: string) => join(root, 'licenses', name);
  try {
    mkdirSync(join(root, 'licenses'));
    for (const name of names) {
      writeFileSync(path(name), 'Required third-party notice\n');
    }
    assert.doesNotThrow(() => assertRuntimeNotices(root));
    for (const name of names) {
      rmSync(path(name));
      assert.throws(() => assertRuntimeNotices(root), new RegExp(name.replaceAll('.', '\\.')));
      writeFileSync(path(name), 'Required third-party notice\n');
    }
    writeFileSync(path('yaml.txt'), '');
    assert.throws(() => assertRuntimeNotices(root), /empty runtime notice: yaml.txt/);
    rmSync(path('yaml.txt'));
    symlinkSync(path('smol-toml.txt'), path('yaml.txt'));
    assert.throws(() => assertRuntimeNotices(root), /runtime notice: yaml.txt/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
