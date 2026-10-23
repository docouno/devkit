import { randomUUID } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, readdirSync, renameSync, rmSync } from 'node:fs';
import { basename, dirname, join, posix } from 'node:path';
import { templates, templateId, componentPath, nodeComponents, profiles } from './catalog.ts';
import type { TemplateId } from './catalog.ts';
import {
  digest,
  fail,
  identity,
  json,
  object,
  readRelease,
  safePath,
  slug,
  text,
  writeAtomic,
  writeJson,
} from './files.ts';
import { renderProjectMake, renderAppCompose } from './project-integration.ts';
import { renderWorkspaceCompose } from './workspace-compose.ts';
import { checksumPath, manifestPath, projectRoot } from './project-state.ts';
import {
  captureProductGraph,
  assertProductGraphsPreserved,
  readNativeGraph,
  regenerateWorkspaceLock,
  seedWorkspaceLock,
} from './native-graph.ts';
import type { LockSeed } from './native-graph.ts';
import {
  deriveComponentDependencies,
  nativeManifest,
  collectNativeInputClosure,
  assertPackageLockMode,
} from './native-workspace.ts';
import type {
  ComponentPath,
  ComponentRole,
  ComponentState,
  FileIdentity,
  PackageMode,
  ProjectState,
  Release,
  ToolingProfile,
} from './types.ts';

export type Template = TemplateId | 'node' | 'react' | 'rust' | 'mixed';
export interface ComponentSpec {
  name: string;
  template: string;
  uses?: string[];
}
export interface InitOptions {
  template?: Template;
  apps?: ComponentSpec[];
  preset?: string;
  packageMode: PackageMode;
  name?: string;
  registry?: string;
}
export function releaseIdentity(root: string): string {
  return digest(readFileSync(join(root, 'release.json')));
}
export function releaseBytes(root: string, release: Release, name: string): Buffer {
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
export function templateDirectory(id: TemplateId): string {
  return id;
}
export function environmentSpec(release: Release, mode: PackageMode): string {
  return mode === 'registry'
    ? release.environment.version
    : `file:../../.devkit/npm/${basename(release.environment.archive)}`;
}
export function profileTools(
  root: string,
  release: Release,
  profile: ToolingProfile,
  mode: PackageMode,
): Record<string, string> {
  if (profile === 'rust') {
    return {};
  }
  const directory = templateDirectory(profiles[profile].template);
  const manifest = object(
    JSON.parse(releaseBytes(root, release, `templates/${directory}/package.json`).toString()),
    'template manifest',
  );
  const tools = Object.fromEntries(
    Object.entries(object(manifest.devDependencies, 'template devDependencies'))
      .filter(([name]) => !['react', 'react-dom'].includes(name))
      .map(([name, value]) => [name, text(value, name)]),
  );
  if (!Object.hasOwn(tools, release.environment.name)) {
    fail('Template is missing environment package');
  }
  tools[release.environment.name] = environmentSpec(release, mode);
  return tools;
}
export function templateTools(
  root: string,
  release: Release,
  template: string,
  mode: PackageMode,
): Record<string, string> {
  return profileTools(root, release, templates[templateId(template)].toolingProfile, mode);
}
export function syncNodeWorkspace(root: string, state: ProjectState, name?: string): void {
  const paths = nodeComponents(state).map(([path]) => path);
  if (!paths.length) {
    return;
  }
  assertPackageLockMode(root);
  const file = join(root, 'package.json');
  const manifest = existsSync(file)
    ? object(json(file), 'root package')
    : { name: name ?? 'devkit-project', private: true };
  const names = new Set<string>();
  for (const path of paths) {
    const packageName = text(nativeManifest(root, path).name, `package name ${path}`);
    if (names.has(packageName)) {
      fail(`Duplicate npm workspace name: ${packageName}`);
    }
    names.add(packageName);
  }
  manifest.workspaces = paths;
  writeJson(file, manifest);
}
export function renderComponent(
  root: string,
  release: Release,
  destination: string,
  projectName: string,
  spec: ComponentSpec,
  role: ComponentRole,
  mode: PackageMode,
  components: Record<ComponentPath, ComponentState>,
): { path: ComponentPath; state: ComponentState; seed?: LockSeed } {
  const id = templateId(spec.template),
    definition = templates[id];
  if (definition.role !== role) {
    fail(`Template ${id} does not create a ${role}`);
  }
  const path = componentPath(role, spec.name),
    directory = templateDirectory(id),
    prefix = `templates/${directory}/`;
  if (existsSync(join(destination, path))) {
    fail(`Component path already exists: ${path}`);
  }
  const uses = spec.uses ?? [];
  if (uses.length > 1 || (uses.length && !['react-web', 'electron-react'].includes(id))) {
    fail('This template supports one shared client binding');
  }
  const client = uses.length ? componentPath('library', uses[0]!) : undefined;
  if (client && components[client]?.toolingProfile !== 'react-library') {
    fail(`Shared client is not a registered React library: ${client}`);
  }
  const clientName = client
    ? text(nativeManifest(destination, client).name, 'client package name')
    : '';
  for (const [file, expected] of Object.entries(release.files)) {
    if (!file.startsWith(prefix) || file.slice(prefix.length).startsWith('variants/')) {
      continue;
    }
    writeAtomic(
      safePath(destination, `${path}/${file.slice(prefix.length)}`),
      releaseBytes(root, release, file),
      expected.mode,
    );
  }
  if (client) {
    const variantPrefix = `${prefix}variants/shared/`;
    const descriptor = object(
      JSON.parse(releaseBytes(root, release, `${variantPrefix}variant.json`).toString()),
      'shared variant',
    );
    if (!Array.isArray(descriptor.remove)) {
      fail('Variant requires an explicit remove list');
    }
    for (const item of descriptor.remove) {
      rmSync(safePath(destination, `${path}/${text(item, 'removed path')}`), {
        recursive: true,
        force: true,
      });
    }
    for (const [file, expected] of Object.entries(release.files)) {
      const filesPrefix = `${variantPrefix}files/`;
      if (!file.startsWith(filesPrefix)) {
        continue;
      }
      writeAtomic(
        safePath(destination, `${path}/${file.slice(filesPrefix.length)}`),
        releaseBytes(root, release, file)
          .toString()
          .replaceAll('__DEVKIT_CLIENT_PACKAGE__', clientName),
        expected.mode,
      );
    }
  }
  const state: ComponentState = {
    role,
    toolchain: definition.toolchain,
    toolingProfile: definition.toolingProfile,
    origin: { template: id, version: release.version },
    toolDependencies: profileTools(root, release, definition.toolingProfile, mode),
    dependencies: [],
  };
  if (role === 'app') {
    state.development = {
      owner: 'managed',
      kind: id === 'electron-react' ? 'desktop' : 'compose',
      service: spec.name,
    };
  }
  if (definition.toolchain === 'rust') {
    return { path, state };
  }
  const packagePath = join(destination, path, 'package.json'),
    manifest = object(json(packagePath), 'component package');
  manifest.name = `@${projectName}/${role === 'app' ? 'app' : 'package'}-${spec.name}`;
  manifest.devDependencies = {
    ...object(manifest.devDependencies, 'dev dependencies'),
    ...state.toolDependencies,
  };
  if (client) {
    object(manifest.devDependencies, 'dev dependencies')[clientName] =
      `file:${posix.relative(path, client)}`;
  }
  writeJson(packagePath, manifest);
  const lockPath = join(destination, path, 'package-lock.json');
  if (!existsSync(lockPath)) {
    fail(`Template lacks canonical native lock: ${id}`);
  }
  const lock = object(json(lockPath), 'template lock');
  const packages = object(lock.packages, 'template lock packages');
  packages[''] = manifest;
  lock.name = manifest.name;
  return { path, state, seed: { prefix: path, lock } };
}
export function writeManaged(
  root: string,
  release: Release,
  destination: string,
  mode: PackageMode,
  state: ProjectState,
): Record<string, FileIdentity> {
  const managed: Record<string, FileIdentity> = {};
  for (const [name, expected] of Object.entries(release.files)) {
    if (name.startsWith('runtime/')) {
      const target = `.devkit/${name}`;
      writeAtomic(safePath(destination, target), releaseBytes(root, release, name), expected.mode);
      managed[target] = expected;
    }
  }
  for (const required of [
    'devkit.mk',
    'devkit.sh',
    'guard.sh',
    'toolchains.env',
    'compose.tools.yaml',
  ]) {
    if (!managed[`.devkit/runtime/${required}`]) {
      fail(`Missing release runtime/${required}`);
    }
  }
  if (mode === 'local') {
    const name = `.devkit/npm/${basename(release.environment.archive)}`;
    writeAtomic(
      safePath(destination, name),
      releaseBytes(root, release, release.environment.archive),
    );
    managed[name] = identity(safePath(destination, name))!;
  }
  for (const [path, component] of Object.entries(state.components)) {
    if (component.toolchain === 'rust') {
      const source = `templates/${templateDirectory('rust-api')}/rust-toolchain.toml`,
        target = `${path}/rust-toolchain.toml`;
      writeAtomic(safePath(destination, target), releaseBytes(root, release, source));
      managed[target] = identity(safePath(destination, target))!;
    }
  }
  for (const [name, content] of [
    [
      '.devkit/tooling-profiles.json',
      JSON.stringify(
        {
          schemaVersion: 1,
          version: release.version,
          packageMode: mode,
          profiles: Object.fromEntries(
            [
              ...new Set(
                Object.values(state.components).map((component) => component.toolingProfile),
              ),
            ]
              .sort()
              .map((profile) => [profile, profileTools(root, release, profile, mode)]),
          ),
        },
        null,
        2,
      ) + '\n',
    ],
    ['.devkit/project.mk', renderProjectMake(state)],
    ['.devkit/compose.apps.yaml', renderAppCompose(state)],
    [
      '.devkit/compose.workspace.json',
      renderWorkspaceCompose(
        state,
        existsSync(join(destination, 'package-lock.json'))
          ? collectNativeInputClosure(destination, state)
          : {
              schemaVersion: 1,
              files: {},
              directories: ['', ...nodeComponents(state).map(([path]) => path)],
              digest: '',
            },
      ),
    ],
  ]) {
    writeAtomic(safePath(destination, name!), content!);
    managed[name!] = identity(safePath(destination, name!))!;
  }
  const checksums = Object.entries(managed)
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([name, entry]) => `${entry.sha256}  ${name}\n`)
    .join('');
  writeAtomic(safePath(destination, checksumPath), checksums);
  managed[checksumPath] = identity(safePath(destination, checksumPath))!;
  return managed;
}
export async function finishNativeWorkspace(
  root: string,
  state: ProjectState,
  seeds: LockSeed[],
  registry?: string,
): Promise<void> {
  if (!nodeComponents(state).length) {
    return;
  }
  syncNodeWorkspace(root, state);
  seedWorkspaceLock(root, state, seeds);
  const before = captureProductGraph(root, state);
  await regenerateWorkspaceLock(root, registry);
  assertProductGraphsPreserved(before, readNativeGraph(root));
  const deps = deriveComponentDependencies(root, state);
  for (const [path, component] of Object.entries(state.components)) {
    component.dependencies = deps[path as ComponentPath]!;
  }
  for (const [path] of nodeComponents(state)) {
    rmSync(join(root, path, 'package-lock.json'), { force: true });
  }
  collectNativeInputClosure(root, state);
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
  if (
    [
      options.apps !== undefined,
      options.preset !== undefined,
      options.template !== undefined,
    ].filter(Boolean).length !== 1
  ) {
    fail('Choose exactly one of --app, --preset or --template');
  }
  const release = readRelease(releaseRoot),
    accepted = releaseIdentity(releaseRoot),
    target = projectRoot(targetPath),
    parent = dirname(target);
  if (!existsSync(parent)) {
    fail('Target parent directory must already exist');
  }
  if (!['local', 'registry'].includes(options.packageMode)) {
    fail('Invalid package source');
  }
  const name = slug(
    options.name ??
      basename(target)
        .toLowerCase()
        .replace(/[^a-z0-9-]+/g, '-')
        .replace(/^-+|-+$/g, ''),
  );
  emptyTarget(target);
  const state: ProjectState = {
    schemaVersion: 2,
    version: release.version,
    releaseIdentity: accepted,
    generation: randomUUID(),
    packageMode: options.packageMode,
    managed: {},
    components: {},
  };
  let specs: { role: ComponentRole; spec: ComponentSpec }[];
  if (options.preset) {
    if (options.preset !== 'fullstack-desktop' || options.apps || options.template) {
      fail('Unknown or combined preset selection');
    }
    specs = [
      { role: 'library', spec: { name: 'client', template: 'react-client' } },
      { role: 'app', spec: { name: 'api', template: 'rust-api' } },
      { role: 'app', spec: { name: 'web', template: 'react-web', uses: ['client'] } },
      { role: 'app', spec: { name: 'desktop', template: 'electron-react', uses: ['client'] } },
    ];
  } else if (options.apps) {
    specs = options.apps.map((spec) => ({ role: 'app', spec }));
  } else if (options.template === 'mixed') {
    specs = [
      { role: 'app', spec: { name: 'api', template: 'rust-api' } },
      { role: 'app', spec: { name: 'web', template: 'react-web' } },
    ];
  } else if (options.template) {
    const id = templateId(options.template);
    specs = [
      {
        role: 'app',
        spec: {
          name: id === 'node-api' ? 'main' : id === 'rust-api' ? 'api' : 'web',
          template: id,
        },
      },
    ];
  } else {
    fail('Init requires --app or --preset');
  }
  if (!specs.some((item) => item.role === 'app')) {
    fail('Init requires an application');
  }
  const stage = join(parent, `.devkit-init-${randomUUID()}`),
    seeds: LockSeed[] = [];
  try {
    mkdirSync(stage);
    for (const item of specs) {
      const rendered = renderComponent(
        releaseRoot,
        release,
        stage,
        name,
        item.spec,
        item.role,
        options.packageMode,
        state.components,
      );
      state.components[rendered.path] = rendered.state;
      if (rendered.seed) {
        seeds.push(rendered.seed);
      }
    }
    if (options.preset === 'fullstack-desktop' || options.template === 'mixed') {
      for (const path of ['apps/web', 'apps/desktop'] as ComponentPath[]) {
        if (state.components[path]?.development) {
          state.components[path].development.api = 'apps/api';
        }
      }
    }
    syncNodeWorkspace(stage, state, name);
    state.managed = writeManaged(releaseRoot, release, stage, options.packageMode, state);
    await finishNativeWorkspace(stage, state, seeds, options.registry);
    state.managed = writeManaged(releaseRoot, release, stage, options.packageMode, state);
    writeAtomic(join(stage, 'Makefile'), 'include .devkit/runtime/devkit.mk\n');
    writeAtomic(
      join(stage, 'compose.yaml'),
      '# Product-owned development overrides.\nservices: {}\n',
    );
    writeAtomic(
      join(stage, '.gitignore'),
      '**/node_modules/\n**/dist/\n**/target/\n**/release/\n.devkit/cache/\n.devkit/state/\n.env\n.env.*\n!.env.example\n',
    );
    writeAtomic(
      join(stage, 'README.md'),
      `# ${name}\n\nRun \`make setup\`, then \`make check build\`. Apps and packages use their native commands through the root Makefile. Use the accepted release's \`devkit add-app\` to add an app. Product source, root Make and Compose overrides belong to this project.\n`,
    );
    writeJson(join(stage, manifestPath), state);
    rmSync(join(stage, '.devkit/cache'), { recursive: true, force: true });
    readRelease(releaseRoot);
    if (releaseIdentity(releaseRoot) !== accepted) {
      fail('Release changed during generation');
    }
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
