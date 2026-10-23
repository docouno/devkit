import { randomUUID } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, readdirSync, renameSync, rmSync } from 'node:fs';
import { basename, dirname, join } from 'node:path';
import {
  copyTree,
  digest,
  fail,
  fileMap,
  identity,
  json,
  object,
  readRelease,
  safePath,
  same,
  slug,
  text,
  writeAtomic,
  writeJson,
} from './files.ts';
import { npmEnvironment, run } from './process.ts';
import { checksumPath, manifestPath, projectRoot } from './project-state.ts';
import type { AppState, FileIdentity, PackageMode, ProjectState, Release } from './types.ts';

export type Template = 'node' | 'react' | 'rust' | 'mixed';
export interface InitOptions {
  template: Template;
  packageMode: PackageMode;
  name?: string;
  registry?: string;
}
export const templates: Record<Template, Record<string, string>> = {
  node: { main: 'node-app' },
  react: { web: 'react-app' },
  rust: { api: 'rust-app' },
  mixed: { web: 'react-app', api: 'rust-app' },
};

function releaseBytes(root: string, release: Release, name: string): Buffer {
  const expected = release.files[name];
  if (!expected) {
    fail(`Release is missing ${name}`);
  }
  const bytes = readFileSync(safePath(root, name));
  if (digest(bytes) !== expected.sha256) {
    fail(`Release payload changed: ${name}`);
  }
  return bytes;
}

export function releaseIdentity(root: string): string {
  return digest(readFileSync(join(root, 'release.json')));
}
export function environmentSpec(release: Release, mode: PackageMode): string {
  return mode === 'registry'
    ? release.environment.version
    : `file:../../.devkit/npm/${basename(release.environment.archive)}`;
}

export function templateTools(
  releaseRoot: string,
  release: Release,
  template: string,
  mode: PackageMode,
): Record<string, string> {
  if (template === 'rust-app') {
    return {};
  }
  const manifest = object(
    JSON.parse(
      releaseBytes(releaseRoot, release, `templates/${template}/package.json`).toString('utf8'),
    ) as unknown,
    'template package manifest',
  );
  const dependencies: Record<string, string> = {};
  for (const [name, spec] of Object.entries(
    object(manifest.devDependencies, 'template devDependencies'),
  )) {
    dependencies[name] = text(spec, name);
  }
  if (!Object.hasOwn(dependencies, release.environment.name)) {
    fail('Template is missing the environment package');
  }
  dependencies[release.environment.name] = environmentSpec(release, mode);
  return dependencies;
}

export function writeManaged(
  releaseRoot: string,
  release: Release,
  destination: string,
  mode: PackageMode,
  apps: Record<string, AppState>,
): Record<string, FileIdentity> {
  const managed: Record<string, FileIdentity> = {};
  for (const [name, expected] of Object.entries(release.files)) {
    if (!name.startsWith('runtime/')) {
      continue;
    }
    const target = `.devkit/${name}`;
    writeAtomic(
      safePath(destination, target),
      releaseBytes(releaseRoot, release, name),
      expected.mode,
    );
    managed[target] = expected;
  }
  for (const required of [
    'devkit.mk',
    'devkit.sh',
    'guard.sh',
    'toolchains.env',
    'compose.tools.yaml',
  ]) {
    if (!Object.hasOwn(managed, `.devkit/runtime/${required}`)) {
      fail(`Release is missing required runtime/${required}`);
    }
  }
  if (mode === 'local') {
    const name = `.devkit/npm/${basename(release.environment.archive)}`;
    writeAtomic(
      safePath(destination, name),
      releaseBytes(releaseRoot, release, release.environment.archive),
    );
    managed[name] = identity(safePath(destination, name))!;
  }
  for (const [name, app] of Object.entries(apps)) {
    if (app.kind !== 'rust') {
      continue;
    }
    const target = `apps/${name}/rust-toolchain.toml`;
    const source = `templates/${app.template}/rust-toolchain.toml`;
    const expected = release.files[source];
    if (!expected) {
      fail(`Release is missing ${source}`);
    }
    writeAtomic(
      safePath(destination, target),
      releaseBytes(releaseRoot, release, source),
      expected.mode,
    );
    managed[target] = expected;
  }
  const checksums = Object.entries(managed)
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([name, expected]) => `${expected.sha256}  ${name}\n`)
    .join('');
  writeAtomic(safePath(destination, checksumPath), checksums);
  managed[checksumPath] = identity(safePath(destination, checksumPath))!;
  return managed;
}

export async function regenerateLock(root: string, app: string, registry?: string): Promise<void> {
  const manifestPath = safePath(root, `apps/${app}/package.json`);
  const before = identity(manifestPath);
  await run(
    'npm',
    [
      'install',
      '--package-lock-only',
      '--package-lock=true',
      '--legacy-peer-deps=false',
      '--force=false',
      '--ignore-scripts',
      '--include=dev',
      '--strict-peer-deps',
      '--workspaces=false',
    ],
    {
      cwd: safePath(root, `apps/${app}`),
      env: npmEnvironment(join(root, '.devkit/cache/npm'), registry),
      quiet: true,
    },
  );
  if (!same(identity(manifestPath), before)) {
    fail(`npm changed the native manifest of ${app} during lock staging`);
  }
  if (identity(safePath(root, `apps/${app}/package-lock.json`)) === null) {
    fail(`npm did not produce a lockfile for ${app}`);
  }
}

function compose(apps: Record<string, AppState>, mixed: boolean): string {
  let result =
    '# Product-owned development services. Tool pins come from the accepted runtime.\nservices:\n';
  for (const [name, app] of Object.entries(apps)) {
    const rust = app.kind === 'rust';
    const port = app.template === 'react-app' ? 5173 : 8080;
    result += `  ${name}:\n    image: "\${${rust ? 'DEVKIT_RUST_IMAGE:?Run make setup' : 'DEVKIT_NODE_IMAGE:?'}}"\n    init: true\n    user: "\${DEVKIT_UID:?}:\${DEVKIT_GID:?}"\n    working_dir: /workspace/apps/${name}\n    command: ${rust ? '["cargo", "run", "--locked"]' : '["npm", "run", "dev"]'}\n    ports:\n      - "127.0.0.1::${port}"\n    environment:\n      HOME: /tmp/devkit-home\n`;
    if (rust) {
      result += `      CARGO_HOME: /cache/cargo\n      CARGO_TARGET_DIR: /workspace/apps/${name}/target\n      RUSTUP_AUTO_INSTALL: "0"\n`;
    }
    if (mixed && name === 'web') {
      result += '      VITE_API_ENABLED: "true"\n      DEVKIT_API_TARGET: http://api:8080\n';
    }
    result += `    volumes:\n      - type: bind\n        source: "\${DEVKIT_PROJECT_ROOT:?}"\n        target: /workspace\n        read_only: true\n        bind: {create_host_path: false, selinux: z}\n`;
    const mounts = rust
      ? [
          [`apps/${name}/target`, `/workspace/apps/${name}/target`],
          ['.devkit/cache/cargo', '/cache/cargo'],
        ]
      : [
          [`apps/${name}/node_modules`, `/workspace/apps/${name}/node_modules`],
          [`apps/${name}/dist`, `/workspace/apps/${name}/dist`],
        ];
    for (const [source, target] of mounts) {
      result += `      - type: bind\n        source: "\${DEVKIT_PROJECT_ROOT:?}/${source}"\n        target: ${target}\n        bind: {create_host_path: false, selinux: z}\n`;
    }
  }
  return result;
}

function emptyTarget(target: string): void {
  if (existsSync(target) && readdirSync(target).some((entry) => entry !== '.git')) {
    fail('Target must be new or empty (existing .git metadata is preserved)');
  }
}

export async function initProject(
  releaseRoot: string,
  targetPath: string,
  options: InitOptions,
): Promise<ProjectState> {
  const release = readRelease(releaseRoot);
  const acceptedIdentity = releaseIdentity(releaseRoot);
  if (!Object.hasOwn(templates, options.template)) {
    fail(`Unknown template: ${options.template}`);
  }
  if (!['local', 'registry'].includes(options.packageMode)) {
    fail(`Unknown package source: ${options.packageMode}`);
  }
  const target = projectRoot(targetPath);
  const parent = dirname(target);
  if (!existsSync(parent)) {
    fail('Target parent directory must already exist');
  }
  const suggested = basename(target)
    .toLowerCase()
    .replace(/[^a-z0-9-]+/g, '-')
    .replace(/^-+|-+$/g, '');
  const name = slug(options.name ?? suggested);
  emptyTarget(target);
  const stage = join(parent, `.devkit-init-${randomUUID()}`);
  const apps: Record<string, AppState> = {};
  try {
    mkdirSync(stage);
    for (const [appName, template] of Object.entries(templates[options.template])) {
      const source = safePath(releaseRoot, `templates/${template}`);
      if (!existsSync(source)) {
        fail(`Release is missing template ${template}`);
      }
      const destination = safePath(stage, `apps/${appName}`);
      copyTree(source, destination);
      const copied = fileMap(destination);
      const expectedNames = Object.keys(release.files)
        .filter((name) => name.startsWith(`templates/${template}/`))
        .map((name) => name.slice(`templates/${template}/`.length))
        .sort();
      if (JSON.stringify(Object.keys(copied).sort()) !== JSON.stringify(expectedNames)) {
        fail(`Template changed during generation: ${template}`);
      }
      for (const [name, actual] of Object.entries(copied)) {
        if (!same(actual, release.files[`templates/${template}/${name}`] ?? null)) {
          fail(`Template changed during generation: ${template}/${name}`);
        }
      }
      if (options.template === 'mixed' && template === 'react-app') {
        const entryPath = safePath(destination, 'src/main.tsx');
        if (identity(entryPath) === null) {
          fail('Mixed template contract changed: missing react-app/src/main.tsx');
        }
        const entry = readFileSync(entryPath, 'utf8');
        const expected = '    <App />\n';
        if (entry.split(expected).length !== 2) {
          fail('Mixed template contract changed: expected exactly one standalone <App /> entry');
        }
        writeAtomic(entryPath, entry.replace(expected, '    <App apiEnabled />\n'));
      }
      const kind = template === 'rust-app' ? 'rust' : 'node';
      const toolDependencies = templateTools(releaseRoot, release, template, options.packageMode);
      apps[appName] = { kind, template, toolDependencies };
      if (kind === 'node') {
        const packagePath = join(destination, 'package.json');
        const manifest = object(json(packagePath), 'package manifest');
        manifest.name = `${name}-${appName}`;
        manifest.devDependencies = toolDependencies;
        writeJson(packagePath, manifest);
        if (options.packageMode === 'registry') {
          const lockPath = join(destination, 'package-lock.json');
          const lock = object(json(lockPath), 'canonical package lock');
          const packages = object(lock.packages, 'canonical lock packages');
          const root = object(packages[''], 'canonical lock root');
          lock.name = manifest.name;
          root.name = manifest.name;
          root.devDependencies = toolDependencies;
          writeJson(lockPath, lock);
        }
      }
    }
    const managed = writeManaged(releaseRoot, release, stage, options.packageMode, apps);
    for (const [appName, app] of Object.entries(apps)) {
      if (app.kind === 'node' && options.packageMode === 'local') {
        await regenerateLock(stage, appName, options.registry);
      }
    }
    writeAtomic(
      join(stage, 'Makefile'),
      `DEVKIT_APPS := ${Object.keys(apps).join(' ')}\ninclude .devkit/runtime/devkit.mk\n`,
    );
    writeAtomic(join(stage, 'compose.yaml'), compose(apps, options.template === 'mixed'));
    writeAtomic(
      join(stage, '.gitignore'),
      '**/node_modules/\n**/dist/\n**/target/\n.devkit/cache/\n.devkit/state/\n.env\n.env.*\n!.env.example\n',
    );
    writeAtomic(
      join(stage, 'README.md'),
      `# ${name}\n\nRun \`make setup\`, then \`make check build\`. Development: \`make dev\`, \`make status\`, \`make logs\`, \`make down\`.\n\nThe accepted toolkit identity and package source are recorded in \`.devkit/manifest.json\`; \`make doctor\` checks the prepared environment. Toolkit archives in local mode are part of this repository.\n\nApps are declared in the root Makefile. Register an added native app with the matching release's \`devkit register\` before adding it to DEVKIT_APPS. Root Makefile, Compose and app source belong to this project.\n`,
    );
    const state: ProjectState = {
      schemaVersion: 1,
      version: release.version,
      releaseIdentity: acceptedIdentity,
      generation: randomUUID(),
      packageMode: options.packageMode,
      apps,
      managed,
    };
    writeJson(join(stage, manifestPath), state);
    rmSync(join(stage, '.devkit/cache'), { recursive: true, force: true });
    // Revalidate an archive that could have changed while native locks were staged.
    readRelease(releaseRoot);
    if (releaseIdentity(releaseRoot) !== acceptedIdentity) {
      fail('Release changed while generating the project');
    }
    // The payload is complete and verified before the first target write.
    fileMap(stage);
    emptyTarget(target);
    if (!existsSync(target)) {
      renameSync(stage, target);
    } else {
      for (const entry of readdirSync(stage)) {
        renameSync(join(stage, entry), join(target, entry));
      }
    }
    return state;
  } finally {
    rmSync(stage, { recursive: true, force: true });
  }
}
