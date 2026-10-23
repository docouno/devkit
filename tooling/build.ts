import { existsSync, mkdirSync, mkdtempSync, readdirSync, renameSync, rmSync } from 'node:fs';
import { basename, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';

const root = resolve(fileURLToPath(new URL('..', import.meta.url)));
const artifacts = join(root, '.artifacts');
mkdirSync(artifacts, { recursive: true });
const stage = mkdtempSync(join(artifacts, 'build-stage-'));
const sources = join(root, 'tooling');
const authorEntries = ['release', 'registry'];
const excluded = new Set([
  'build.ts',
  'runtime-project.ts',
  'lock-identities.ts',
  ...authorEntries.map((name) => `${name}.ts`),
]);
const runtimeEntries = Object.fromEntries(
  readdirSync(sources)
    .filter((name) => name.endsWith('.ts') && !name.endsWith('.d.ts') && !excluded.has(name))
    .sort()
    .map((name) => [basename(name, '.ts'), join(sources, name)]),
);

try {
  const common = {
    bundle: true,
    platform: 'node' as const,
    target: 'node24',
    format: 'esm' as const,
    sourcemap: false,
    legalComments: 'inline' as const,
    logLevel: 'info' as const,
    banner: {
      js: "import { createRequire as __devkitCreateRequire } from 'node:module'; const require = __devkitCreateRequire(import.meta.url);",
    },
  };
  await build({
    ...common,
    entryPoints: runtimeEntries,
    outdir: join(stage, 'cli'),
    splitting: true,
  });
  // Author-only packaging/registry helpers never enter the shipped CLI graph.
  await build({
    ...common,
    entryPoints: Object.fromEntries(
      authorEntries.map((name) => [name, join(sources, `${name}.ts`)]),
    ),
    outdir: join(stage, 'cli-author'),
    splitting: true,
  });
  if (existsSync(join(sources, 'runtime-project.ts'))) {
    await build({
      ...common,
      entryPoints: [join(sources, 'runtime-project.ts')],
      outfile: join(stage, 'runtime-project.mjs'),
    });
  }
  for (const name of ['cli', 'cli-author']) {
    rmSync(join(artifacts, name), { recursive: true, force: true });
    renameSync(join(stage, name), join(artifacts, name));
  }
  if (existsSync(join(stage, 'runtime-project.mjs'))) {
    renameSync(join(stage, 'runtime-project.mjs'), join(artifacts, 'runtime-project.mjs'));
  }
} finally {
  rmSync(stage, { recursive: true, force: true });
}
