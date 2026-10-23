import { createHash } from 'node:crypto';
import {
  chmodSync,
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  renameSync,
  rmSync,
} from 'node:fs';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import {
  copyTree,
  digest,
  fail,
  fileMap,
  json,
  object,
  readRelease,
  text,
  version,
  writeAtomic,
  writeJson,
} from './files.ts';
import { npmEnvironment, run } from './process.ts';
import { canonicalizeLock, withRegistry } from './registry.ts';
import { assertCompleteLockIdentities, hydrateLockIdentities } from './lock-identities.ts';
import type { Release } from './types.ts';

const templateNames = [
  'node-api',
  'react-web',
  'rust-api',
  'electron-react',
  'react-client',
] as const;
const requiredRuntime = [
  'devkit.mk',
  'devkit.sh',
  'guard.sh',
  'compose.tools.yaml',
  'rust.Dockerfile',
  'rust-check.sh',
  'desktop.Dockerfile',
  'desktop-seccomp.json',
  'desktop-dev.sh',
];

export function assertRuntimeNotices(runtimeRoot: string): void {
  for (const name of ['Apache-2.0.txt', 'moby-profiles.NOTICE', 'yaml.txt', 'smol-toml.txt']) {
    const path = join(runtimeRoot, 'licenses', name);
    if (!existsSync(path) || !lstatSync(path).isFile() || readFileSync(path).length === 0) {
      fail(`Missing or empty runtime notice: ${name}`);
    }
  }
}

function isBuildOutput(name: string): boolean {
  return (
    name
      .split('/')
      .some((part) => ['node_modules', 'target', '.artifacts', '.tmp', '.git'].includes(part)) ||
    name === 'dist' ||
    name.startsWith('dist/')
  );
}

function toolchains(root: string): Map<string, string> {
  const values = new Map<string, string>();
  for (const line of readFileSync(join(root, 'toolchains.env'), 'utf8').split('\n')) {
    if (!line.trim() || line.trim().startsWith('#')) {
      continue;
    }
    const match = /^(DEVKIT_[A-Z_]+)=([A-Za-z0-9._/@:+-]+)$/.exec(line);
    if (!match?.[1] || !match[2] || values.has(match[1])) {
      fail(`Invalid toolchain declaration: ${line}`);
    }
    values.set(match[1], match[2]);
  }
  const node = values.get('DEVKIT_NODE_IMAGE');
  if (!node || !/@sha256:[a-f0-9]{64}$/.test(node)) {
    fail('Node image must be digest-pinned');
  }
  version(values.get('DEVKIT_NODE_VERSION'));
  version(values.get('DEVKIT_RUST_VERSION'));
  return values;
}

async function canonicalTemplateLock(
  appRoot: string,
  packageVersion: string,
  registry: string,
  cache: string,
): Promise<void> {
  const manifest = object(json(join(appRoot, 'package.json')), 'template package.json');
  const dependencies = object(manifest.devDependencies, 'template devDependencies');
  dependencies['@docouno/env'] = packageVersion;
  writeJson(join(appRoot, 'package.json'), manifest);
  const lockPath = join(appRoot, 'package-lock.json');
  if (existsSync(lockPath)) {
    const lock = object(json(lockPath), 'template lockfile');
    const packages = object(lock.packages, 'lock packages');
    // Refresh our own unpublished version's integrity after a source change.
    // Keep the native lock's other resolutions, rather than selecting all anew.
    delete packages['node_modules/@docouno/env'];
    const top = object(packages[''], 'lock root package');
    if (top.devDependencies !== undefined) {
      object(top.devDependencies, 'lock root devDependencies')['@docouno/env'] = packageVersion;
    }
    writeJson(lockPath, await hydrateLockIdentities(lock, { cwd: appRoot, cache, registry }));
  }
  await run(
    'npm',
    ['install', '--package-lock-only', '--ignore-scripts', '--include=dev', '--strict-peer-deps'],
    { cwd: appRoot, env: npmEnvironment(cache, registry) },
  );
  const canonical = canonicalizeLock(json(lockPath), registry);
  assertCompleteLockIdentities(canonical);
  const encoded = JSON.stringify(canonical);
  if (encoded.includes('"link":true') || /file:|\/workspace\/|\.artifacts\//.test(encoded)) {
    fail('A source/local link remains in a default template lock');
  }
  writeJson(lockPath, canonical);
}

export async function buildRelease(
  rootInput: string,
  releaseVersionInput?: string,
): Promise<string> {
  const root = resolve(rootInput);
  const releaseVersion = version(
    releaseVersionInput ?? readFileSync(join(root, 'VERSION'), 'utf8').trim(),
  );
  const settings = toolchains(root);
  for (const file of requiredRuntime) {
    if (!existsSync(join(root, 'runtime', file))) {
      fail(`Incomplete runtime: ${file}`);
    }
  }
  for (const file of ['cli.js', 'project.js', 'files.js', 'process.js', 'types.js']) {
    if (!existsSync(join(root, '.artifacts', 'cli', file))) {
      fail(`Build the factory first: missing ${file}`);
    }
  }
  if (!existsSync(join(root, '.artifacts', 'runtime-project.mjs'))) {
    fail('Build the runtime metadata helper first');
  }
  const artifacts = join(root, '.artifacts');
  mkdirSync(artifacts, { recursive: true });
  const stage = mkdtempSync(join(artifacts, 'release-stage-'));
  const payload = join(stage, `devkit-${releaseVersion}`);
  mkdirSync(payload);
  try {
    const envStage = join(stage, 'environment');
    copyTree(join(root, 'packages', 'env'), envStage, { ignore: isBuildOutput });
    const envManifest = object(json(join(envStage, 'package.json')), 'environment manifest');
    if (envManifest.name !== '@docouno/env') {
      fail('Unexpected environment package name');
    }
    envManifest.version = releaseVersion;
    writeJson(join(envStage, 'package.json'), envManifest);
    if (!existsSync(join(envStage, 'LICENSE'))) {
      writeAtomic(join(envStage, 'LICENSE'), readFileSync(join(root, 'LICENSE')));
    }
    mkdirSync(join(payload, 'npm'));
    const packed = await run(
      'npm',
      ['pack', '--ignore-scripts', '--json', '--pack-destination', join(payload, 'npm')],
      { cwd: envStage, env: npmEnvironment(join(stage, 'npm-cache')), quiet: true },
    );
    const result: unknown = JSON.parse(packed.stdout);
    if (!Array.isArray(result) || result.length !== 1) {
      fail('npm pack returned an unexpected result');
    }
    const entry = object(result[0], 'npm pack result');
    const filename = text(entry.filename, 'packed filename');
    if (basename(filename) !== filename || !filename.endsWith('.tgz')) {
      fail('Unsafe npm archive name');
    }
    const npmArchive = join(payload, 'npm', filename);
    const bytes = readFileSync(npmArchive);
    const integrity = `sha512-${createHash('sha512').update(bytes).digest('base64')}`;
    if (entry.integrity !== integrity) {
      fail('npm pack integrity differs from its actual archive');
    }

    copyTree(join(root, 'runtime'), join(payload, 'runtime'), { ignore: isBuildOutput });
    writeAtomic(
      join(payload, 'runtime', 'project.mjs'),
      readFileSync(join(root, '.artifacts', 'runtime-project.mjs')),
    );
    writeAtomic(join(payload, 'runtime', 'LICENSE'), readFileSync(join(root, 'LICENSE')));
    for (const dependency of ['yaml', 'smol-toml']) {
      writeAtomic(
        join(payload, 'runtime', 'licenses', `${dependency}.txt`),
        readFileSync(join(root, 'node_modules', dependency, 'LICENSE')),
      );
    }
    assertRuntimeNotices(join(payload, 'runtime'));
    const recipe = digest(readFileSync(join(payload, 'runtime', 'rust.Dockerfile')));
    settings.set('DEVKIT_VERSION', releaseVersion);
    settings.set('DEVKIT_RUST_RECIPE_HASH', recipe);
    settings.set(
      'DEVKIT_DESKTOP_RECIPE_HASH',
      digest(readFileSync(join(payload, 'runtime', 'desktop.Dockerfile'))),
    );
    writeAtomic(
      join(payload, 'runtime', 'toolchains.env'),
      [...settings].map(([key, value]) => `${key}=${value}`).join('\n') + '\n',
    );

    copyTree(join(root, '.artifacts', 'cli'), join(payload, 'cli'), {
      ignore: (name) =>
        ['release.js', 'registry.js'].includes(name) || /\.(?:map|d\.ts)$/.test(name),
    });
    writeAtomic(join(payload, 'devkit'), readFileSync(join(root, 'tooling', 'launcher.sh')), 0o755);
    writeAtomic(
      join(payload, 'compose.factory.yaml'),
      readFileSync(join(root, 'tooling', 'compose.factory.yaml')),
    );
    writeAtomic(join(payload, 'LICENSE'), readFileSync(join(root, 'LICENSE')));

    for (const template of templateNames) {
      copyTree(join(root, 'templates', template), join(payload, 'templates', template), {
        ignore: isBuildOutput,
      });
      writeAtomic(
        join(payload, 'templates', template, 'LICENSE'),
        readFileSync(join(root, 'LICENSE')),
      );
    }
    await withRegistry([{ manifest: envManifest, archive: npmArchive }], async (registry) => {
      for (const template of ['node-api', 'react-web', 'electron-react', 'react-client']) {
        await canonicalTemplateLock(
          join(payload, 'templates', template),
          releaseVersion,
          registry,
          join(stage, 'template-cache'),
        );
      }
    });

    const rustPin = readFileSync(
      join(payload, 'templates', 'rust-api', 'rust-toolchain.toml'),
      'utf8',
    );
    if (!rustPin.includes(`channel = "${settings.get('DEVKIT_RUST_VERSION')}"`)) {
      fail('Rust template pin differs from the release toolchain');
    }
    for (const [name] of Object.entries(fileMap(payload))) {
      chmodSync(join(payload, name), name === 'devkit' || name.endsWith('.sh') ? 0o755 : 0o644);
    }
    const manifest: Release = {
      schemaVersion: 1,
      version: releaseVersion,
      rustVersion: version(settings.get('DEVKIT_RUST_VERSION')),
      environment: {
        name: '@docouno/env',
        version: releaseVersion,
        archive: `npm/${filename}`,
        integrity,
      },
      files: fileMap(payload),
    };
    writeJson(join(payload, 'release.json'), manifest);
    readRelease(payload);

    let bootstrap = readFileSync(join(root, 'tooling', 'bootstrap.sh'), 'utf8');
    const replacements: Record<string, string> = {
      '@@BOOTSTRAP_VERSION@@': releaseVersion,
      '@@NODE_IMAGE@@': settings.get('DEVKIT_NODE_IMAGE')!,
      '@@NODE_VERSION@@': settings.get('DEVKIT_NODE_VERSION')!,
      '@@GUARD@@': readFileSync(join(root, 'runtime', 'guard.sh'), 'utf8'),
    };
    for (const [token, value] of Object.entries(replacements)) {
      if (bootstrap.split(token).length !== 2) {
        fail(`Bootstrap must contain exactly one ${token}`);
      }
      bootstrap = bootstrap.replace(token, () => value);
    }
    if (/@@[A-Z_]+@@/u.test(bootstrap)) {
      fail('Unresolved bootstrap placeholder');
    }
    const stagedBootstrap = join(stage, `devkit-${releaseVersion}.sh`);
    writeAtomic(stagedBootstrap, bootstrap, 0o755);

    const stagedArchive = join(stage, `devkit-${releaseVersion}.tar.gz`);
    await run(
      'tar',
      [
        '--sort=name',
        '--mtime=@0',
        '--owner=0',
        '--group=0',
        '--numeric-owner',
        '-czf',
        stagedArchive,
        '-C',
        stage,
        `devkit-${releaseVersion}`,
      ],
      { cwd: root },
    );
    const output = join(artifacts, 'releases', releaseVersion);
    mkdirSync(dirname(output), { recursive: true });
    if (existsSync(output)) {
      const backup = join(stage, 'previous-preview');
      renameSync(output, backup);
      try {
        renameSync(payload, output);
      } catch (error) {
        renameSync(backup, output);
        throw error;
      }
      process.stdout.write(
        `Replaced local preview ${releaseVersion}; existing consumers retain their accepted identity.\n`,
      );
    } else {
      renameSync(payload, output);
    }
    const archive = join(artifacts, `devkit-${releaseVersion}.tar.gz`);
    renameSync(stagedArchive, archive);
    writeAtomic(`${archive}.sha256`, `${digest(readFileSync(archive))}  ${basename(archive)}\n`);
    const bootstrapPath = join(artifacts, `devkit-${releaseVersion}.sh`);
    renameSync(stagedBootstrap, bootstrapPath);
    writeAtomic(
      `${bootstrapPath}.sha256`,
      `${digest(readFileSync(bootstrapPath))}  ${basename(bootstrapPath)}\n`,
    );
    process.stdout.write(`Release ${releaseVersion}: ${output}\nArchive: ${archive}\n`);
    return output;
  } finally {
    rmSync(stage, { recursive: true, force: true });
  }
}

async function main(): Promise<void> {
  const { values } = parseArgs({ options: { version: { type: 'string' } }, strict: true });
  const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
  // Emitted helper lives in .artifacts/cli; the source helper is in tooling.
  const repo = existsSync(join(root, 'VERSION'))
    ? root
    : resolve(dirname(fileURLToPath(import.meta.url)), '..');
  await buildRelease(repo, values.version);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((error: unknown) => {
    process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
    process.exitCode = 1;
  });
}
