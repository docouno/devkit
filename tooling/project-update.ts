import { randomUUID } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, rmdirSync } from 'node:fs';
import { basename, join } from 'node:path';
import { componentPath, nodeComponents, profiles, templateId, templates } from './catalog.ts';
import {
  copyTree,
  fail,
  fileMap,
  identity,
  json,
  object,
  readRelease,
  relativePath,
  safePath,
  same,
  text,
  writeAtomic,
  writeJson,
} from './files.ts';
import {
  assertManaged,
  assertNoPending,
  fileIdentity,
  managedPath,
  manifestPath,
  pendingPath,
  projectRoot,
  readProjectState,
  snapshotExcluder,
  sourceSignature,
} from './project-state.ts';
import {
  profileTools,
  releaseIdentity,
  renderComponent,
  syncNodeWorkspace,
  writeManaged,
} from './project.ts';
import type { ComponentSpec } from './project.ts';
import {
  assertNpmrcCompatibility,
  assertProductGraphsPreserved,
  captureProductGraph,
  readNativeGraph,
  regenerateWorkspaceLock,
  seedWorkspaceLock,
} from './native-graph.ts';
import type { LockSeed } from './native-graph.ts';
import {
  assertNativeComponents,
  assertComponentManifests,
  assertPackageLockMode,
  assertToolProfiles,
  collectNativeInputClosure,
  deriveComponentDependencies,
  nativeManifest,
} from './native-workspace.ts';
import { assertComposeInventory } from './compose-inputs.ts';
import { renderWorkspaceCompose } from './workspace-compose.ts';
import { normalizeLegacyLockIdentities } from './lock-identities.ts';
import type {
  AnyProjectState,
  ComponentPath,
  ComponentRole,
  ComponentState,
  ComposeInventory,
  FileIdentity,
  PackageMode,
  PlannedFile,
  ProjectState,
  ToolingProfile,
  UpdateJournal,
} from './types.ts';

export interface UpdateHooks {
  afterStage?: () => void;
  afterWrite?: (count: number) => void;
}
export interface UpdateOptions {
  packageMode?: PackageMode;
  registry?: string;
  hooks?: UpdateHooks;
  composeInventory?: ComposeInventory;
}
function transactionPath(name: string, created: string[] = []): string {
  relativePath(name);
  if (
    name === manifestPath ||
    ['package.json', 'package-lock.json'].includes(name) ||
    /^(apps|packages)\/[a-z][a-z0-9-]*\/package(?:-lock)?\.json$/.test(name) ||
    created.some((dir) => name.startsWith(`${dir}/`))
  ) {
    return name;
  }
  return managedPath(name);
}
function optionalIdentity(value: unknown, label: string): FileIdentity | null {
  return value === null ? null : fileIdentity(value, label);
}
function journal(root: string): UpdateJournal {
  const raw = object(json(safePath(root, pendingPath)), 'update journal');
  if (![1, 2].includes(Number(raw.schemaVersion)) || !Array.isArray(raw.files)) {
    fail('Unsupported update journal');
  }
  const stage = text(raw.stage, 'update stage');
  if (!/^\.devkit\/state\/update-[a-f0-9-]{36}$/.test(stage)) {
    fail('Invalid update stage');
  }
  const created =
    raw.createdDirectories === undefined
      ? []
      : Array.isArray(raw.createdDirectories)
        ? raw.createdDirectories.map((item) => text(item, 'created directory'))
        : fail('Invalid created directories');
  for (const dir of created) {
    if (!/^(apps|packages)\/[a-z][a-z0-9-]*$/.test(dir)) {
      fail('Invalid created component directory');
    }
  }
  const names = new Set<string>();
  const parents =
    raw.createdParents === undefined
      ? []
      : Array.isArray(raw.createdParents)
        ? raw.createdParents.map((value) => text(value, 'created parent'))
        : fail('Invalid created parents');
  if (
    parents.some((name) => !['apps', 'packages'].includes(name)) ||
    new Set(parents).size !== parents.length
  ) {
    fail('Invalid created parent directories');
  }
  const files = raw.files.map((value, index): PlannedFile => {
    const file = object(value, 'journal file'),
      path = transactionPath(text(file.path, 'target'), created);
    if (names.has(path)) {
      fail(`Duplicate journal target: ${path}`);
    }
    names.add(path);
    const before = optionalIdentity(file.before, path),
      after = optionalIdentity(file.after, path);
    if (same(before, after)) {
      fail(`Unchanged journal target: ${path}`);
    }
    if (created.some((dir) => path.startsWith(`${dir}/`)) && before !== null) {
      fail('New component journal overwrites existing source');
    }
    const backup = before === null ? null : `${stage}/backup/${index}`,
      staged = after === null ? null : `${stage}/staged/${index}`;
    if (file.backup !== backup || file.staged !== staged) {
      fail(`Invalid saved path: ${path}`);
    }
    return { path, before, after, backup, staged };
  });
  if (!files.length || !names.has(manifestPath)) {
    fail('Journal is missing accepted manifest');
  }
  return {
    schemaVersion: raw.schemaVersion as 1 | 2,
    stage,
    files,
    createdDirectories: created,
    createdParents: parents,
  };
}
function verifySaved(root: string, file: PlannedFile): void {
  if (file.backup !== null && !same(identity(safePath(root, file.backup)), file.before)) {
    fail(`Update backup integrity mismatch: ${file.path}`);
  }
  if (file.staged !== null && !same(identity(safePath(root, file.staged)), file.after)) {
    fail(`Update staged integrity mismatch: ${file.path}`);
  }
}
function pruneEmpty(directory: string): void {
  if (!existsSync(directory)) {
    return;
  }
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    if (entry.isDirectory() && !entry.isSymbolicLink()) {
      pruneEmpty(join(directory, entry.name));
    }
  }
  if (!readdirSync(directory).length) {
    rmdirSync(directory);
  }
}
export function recoverProject(target: string): boolean {
  const root = projectRoot(target);
  safePath(root, '.devkit/state/task.lock');
  const pending = identity(safePath(root, pendingPath));
  if (pending === null) {
    return false;
  }
  const tx = journal(root);
  for (const dir of tx.createdDirectories ?? []) {
    if (existsSync(safePath(root, dir))) {
      for (const name of Object.keys(fileMap(safePath(root, dir)))) {
        if (!tx.files.some((file) => file.path === `${dir}/${name}`)) {
          fail(`Recovery conflict: unknown file ${dir}/${name}`);
        }
      }
    }
  }
  for (const file of tx.files) {
    verifySaved(root, file);
    const actual = identity(safePath(root, file.path));
    if (!same(actual, file.before) && !same(actual, file.after)) {
      fail(`Recovery conflict: ${file.path}; restore a known before/after state`);
    }
  }
  if (!same(identity(safePath(root, pendingPath)), pending)) {
    fail('Update journal changed during recovery');
  }
  for (const file of [...tx.files].reverse()) {
    if (same(identity(safePath(root, file.path)), file.before)) {
      continue;
    }
    if (file.backup === null) {
      rmSync(safePath(root, file.path), { force: true });
    } else {
      writeAtomic(
        safePath(root, file.path),
        readFileSync(safePath(root, file.backup)),
        file.before!.mode,
      );
    }
  }
  for (const dir of tx.createdDirectories ?? []) {
    pruneEmpty(safePath(root, dir));
  }
  for (const parent of tx.createdParents ?? []) {
    const path = safePath(root, parent);
    if (existsSync(path) && readdirSync(path).length === 0) {
      rmdirSync(path);
    }
  }
  rmSync(safePath(root, pendingPath));
  rmSync(safePath(root, tx.stage), { recursive: true });
  return true;
}
function commitUpdate(
  root: string,
  stageName: string,
  stagedRoot: string,
  next: ProjectState,
  old: AnyProjectState,
  expectedSource: string,
  created: string[],
  options: UpdateOptions,
): void {
  const paths = new Set([
    ...Object.keys(old.managed),
    ...Object.keys(next.managed),
    manifestPath,
    'package.json',
    'package-lock.json',
  ]);
  for (const path of Object.keys(next.components)) {
    if (next.components[path as ComponentPath]!.toolchain === 'node') {
      paths.add(`${path}/package.json`);
      paths.add(`${path}/package-lock.json`);
    }
  }
  for (const dir of created) {
    for (const file of Object.keys(fileMap(safePath(stagedRoot, dir)))) {
      paths.add(`${dir}/${file}`);
    }
  }
  const raw = object(json(safePath(root, manifestPath)), 'old manifest');
  const previous = old.schemaVersion === 2 ? object(raw.components, 'old components') : {};
  const merged: Record<string, unknown> = {
    ...raw,
    ...next,
    components: Object.fromEntries(
      Object.entries(next.components).map(([path, component]) => [
        path,
        { ...object(previous[path] ?? {}, path), ...component },
      ]),
    ),
  };
  delete merged.apps;
  writeJson(safePath(stagedRoot, manifestPath), merged);
  const files: PlannedFile[] = [];
  for (const name of [...paths].sort((a, b) =>
    a === manifestPath ? 1 : b === manifestPath ? -1 : a.localeCompare(b),
  )) {
    transactionPath(name, created);
    const before = identity(safePath(root, name)),
      after = identity(safePath(stagedRoot, name));
    if (same(before, after)) {
      continue;
    }
    const index = files.length,
      backup = before === null ? null : `${stageName}/backup/${index}`,
      staged = after === null ? null : `${stageName}/staged/${index}`;
    if (backup) {
      writeAtomic(safePath(root, backup), readFileSync(safePath(root, name)), before!.mode);
    }
    if (staged) {
      writeAtomic(safePath(root, staged), readFileSync(safePath(stagedRoot, name)), after!.mode);
    }
    files.push({ path: name, before, after, backup, staged });
  }
  options.hooks?.afterStage?.();
  if (sourceSignature(root, old) !== expectedSource) {
    fail('Project changed while staging the update; retry after reviewing concurrent edits');
  }
  if (options.composeInventory) {
    assertComposeInventory(root, options.composeInventory);
  }
  for (const file of files) {
    if (!same(identity(safePath(root, file.path)), file.before)) {
      fail(`Concurrent update conflict: ${file.path}`);
    }
  }
  assertNoPending(root);
  const createdParents = [...new Set(created.map((path) => path.split('/')[0]!))].filter(
    (path) => !existsSync(safePath(root, path)),
  );
  writeJson(safePath(root, pendingPath), {
    schemaVersion: 2,
    stage: stageName,
    files,
    createdDirectories: created,
    createdParents,
  });
  try {
    for (const [index, file] of files.entries()) {
      if (!same(identity(safePath(root, file.path)), file.before)) {
        fail(`Concurrent update conflict: ${file.path}`);
      }
      verifySaved(root, file);
      if (file.staged === null) {
        rmSync(safePath(root, file.path), { force: true });
      } else {
        writeAtomic(
          safePath(root, file.path),
          readFileSync(safePath(root, file.staged)),
          file.after!.mode,
        );
      }
      options.hooks?.afterWrite?.(index + 1);
    }
    rmSync(safePath(root, pendingPath));
  } catch (error) {
    try {
      recoverProject(root);
    } catch (rollback) {
      fail(
        `${String(error)}; automatic rollback was blocked: ${String(rollback)}. Run devkit recover.`,
      );
    }
    throw error;
  }
}
function migrateState(old: AnyProjectState): ProjectState {
  if (old.schemaVersion === 2) {
    return structuredClone(old);
  }
  const components: ProjectState['components'] = {};
  for (const [name, app] of Object.entries(old.apps)) {
    const id = templateId(app.template),
      definition = templates[id];
    components[componentPath('app', name)] = {
      role: 'app',
      toolchain: app.kind,
      toolingProfile: definition.toolingProfile,
      origin: { template: app.template, version: old.version },
      toolDependencies: app.toolDependencies,
      dependencies: [],
      development: { owner: 'project', kind: 'compose', service: name },
    };
  }
  return {
    schemaVersion: 2,
    version: old.version,
    releaseIdentity: old.releaseIdentity,
    generation: old.generation,
    packageMode: old.packageMode,
    managed: old.managed,
    components,
  };
}
function verifyOldTools(root: string, state: ProjectState): void {
  assertComponentManifests(root, state);
  for (const [path, component] of Object.entries(state.components)) {
    if (component.toolchain === 'rust') {
      for (const file of ['Cargo.toml', 'Cargo.lock']) {
        if (identity(safePath(root, `${path}/${file}`)) === null) {
          fail(`Missing native ${path}/${file}`);
        }
      }
      continue;
    }
    const manifest = nativeManifest(root, path),
      deps = object(manifest.devDependencies, 'native devDependencies');
    for (const [name, value] of Object.entries(component.toolDependencies)) {
      if (deps[name] !== value) {
        fail(`Tool dependency conflict: ${path}/package.json devDependencies.${name}`);
      }
    }
  }
}
function updateTools(
  root: string,
  path: ComponentPath,
  old: ComponentState,
  next: ComponentState,
): void {
  const manifest = nativeManifest(root, path),
    deps = object(manifest.devDependencies, 'native devDependencies');
  for (const name of Object.keys(old.toolDependencies)) {
    delete deps[name];
  }
  for (const [name, value] of Object.entries(next.toolDependencies)) {
    if (
      (Object.hasOwn(deps, name) && deps[name] !== value) ||
      (object(manifest.dependencies ?? {}, 'dependencies')[name] !== undefined &&
        object(manifest.dependencies, 'dependencies')[name] !== value)
    ) {
      fail(`New tool dependency conflicts with project dependency: ${path}/${name}`);
    }
    deps[name] = value;
  }
  writeJson(safePath(root, `${path}/package.json`), manifest);
}
type Change =
  | { kind: 'add'; role: ComponentRole; spec: ComponentSpec }
  | { kind: 'register'; role: ComponentRole; name: string; profile: ToolingProfile };
async function mutate(
  releaseRoot: string,
  target: string,
  options: UpdateOptions,
  change?: Change,
): Promise<ProjectState> {
  const release = readRelease(releaseRoot),
    root = projectRoot(target);
  safePath(root, '.devkit/state/task.lock');
  assertNoPending(root);
  const old = readProjectState(root);
  assertManaged(root, old);
  if (options.composeInventory) {
    assertComposeInventory(root, options.composeInventory);
  }
  const next = migrateState(old);
  if (old.schemaVersion === 1) {
    for (const [path] of nodeComponents(next)) {
      if (identity(safePath(root, `${path}/npm-shrinkwrap.json`)) !== null) {
        fail(`Legacy npm-shrinkwrap.json conflicts with package-lock migration: ${path}`);
      }
    }
  }
  if (
    nodeComponents(next).length ||
    (change?.kind === 'add' && templates[templateId(change.spec.template)].toolchain === 'node') ||
    (change?.kind === 'register' && change.profile !== 'rust')
  ) {
    assertPackageLockMode(root);
  }
  if (old.schemaVersion === 2) {
    assertToolProfiles(root, old);
  }
  verifyOldTools(root, next);
  if (old.schemaVersion === 2 && nodeComponents(old).length) {
    const actual = nativeManifest(root, '').workspaces;
    const expected = nodeComponents(old).map(([path]) => path);
    const adopting =
      change?.kind === 'register' ? componentPath(change.role, change.name) : undefined;
    if (
      JSON.stringify(actual) !== JSON.stringify(expected) &&
      (!adopting || JSON.stringify(actual) !== JSON.stringify([...expected, adopting].sort()))
    ) {
      fail('Root npm workspace membership conflicts with accepted registration');
    }
  }
  if (change && releaseIdentity(releaseRoot) !== old.releaseIdentity) {
    fail('Add/register requires the exact accepted release');
  }
  if (old.version === release.version && old.releaseIdentity !== releaseIdentity(releaseRoot)) {
    fail('A different release has the same version; use a distinctly versioned release');
  }
  const mode = options.packageMode ?? old.packageMode;
  if (!['local', 'registry'].includes(mode)) {
    fail('Invalid package source');
  }
  if (
    !change &&
    old.schemaVersion === 2 &&
    old.releaseIdentity === releaseIdentity(releaseRoot) &&
    mode === old.packageMode
  ) {
    try {
      assertNativeComponents(root, old);
      if (
        readFileSync(safePath(root, '.devkit/compose.workspace.json'), 'utf8') ===
        renderWorkspaceCompose(old, collectNativeInputClosure(root, old))
      ) {
        return old;
      }
    } catch (error) {
      if (!(error instanceof Error) || !error.message.includes('projection is stale')) {
        throw error;
      }
    }
  }
  const expectedSource = sourceSignature(root, old),
    stageName = `.devkit/state/update-${randomUUID()}`,
    stage = safePath(root, stageName),
    stagedRoot = join(stage, 'project');
  mkdirSync(stage, { recursive: true });
  try {
    copyTree(root, stagedRoot, { ignore: snapshotExcluder(old), preserveLinks: true });
    let seeds: LockSeed[] = [];
    const legacyLocks: Record<string, FileIdentity> = {};
    if (old.schemaVersion === 1 && nodeComponents(next).length) {
      if (existsSync(join(root, 'package-lock.json'))) {
        fail('Unmanaged root package-lock.json conflicts with migration');
      }
      if (existsSync(join(root, 'package.json'))) {
        const native = nativeManifest(root, '');
        if (
          native.workspaces !== undefined ||
          ['dependencies', 'devDependencies', 'optionalDependencies'].some(
            (field) => Object.keys(object(native[field] ?? {}, field)).length,
          )
        ) {
          fail('Unmanaged root npm graph conflicts with migration');
        }
      }
      assertNpmrcCompatibility(
        stagedRoot,
        nodeComponents(next).map(([path]) => path),
      );
      seeds = nodeComponents(next).map(([path]) => ({
        prefix: path,
        lock: json(safePath(stagedRoot, `${path}/package-lock.json`)),
      }));
      for (const [path] of nodeComponents(next)) {
        legacyLocks[`${path}/package-lock.json`] = identity(
          safePath(stagedRoot, `${path}/package-lock.json`),
        )!;
      }
    } else if (nodeComponents(next).length) {
      seeds.push({ prefix: '', lock: json(join(stagedRoot, 'package-lock.json')) });
    }
    const created: string[] = [];
    if (change?.kind === 'add') {
      const path = componentPath(change.role, change.spec.name);
      if (Object.hasOwn(next.components, path) || existsSync(safePath(root, path))) {
        fail(`Component path already exists: ${path}`);
      }
      if (change.role === 'app') {
        if (!options.composeInventory) {
          fail('Adding a managed service requires admitted Compose inventory');
        }
        if (options.composeInventory.services.includes(change.spec.name)) {
          fail(`Compose service conflict: ${change.spec.name}`);
        }
      }
      const name = existsSync(join(stagedRoot, 'package.json'))
        ? String(nativeManifest(stagedRoot, '').name)
            .replace(/^@/, '')
            .replace(/[^a-z0-9-]+/g, '-')
        : basename(root)
            .toLowerCase()
            .replace(/[^a-z0-9-]+/g, '-');
      const rendered = renderComponent(
        releaseRoot,
        release,
        stagedRoot,
        name,
        change.spec,
        change.role,
        mode,
        next.components,
      );
      next.components[rendered.path] = rendered.state;
      created.push(rendered.path);
      if (rendered.seed) {
        seeds.push(rendered.seed);
      }
    } else if (change?.kind === 'register') {
      const path = componentPath(change.role, change.name);
      if (Object.hasOwn(next.components, path)) {
        fail(`Component is already registered: ${path}`);
      }
      const tools = profileTools(releaseRoot, release, change.profile, mode);
      const toolchain = change.profile === 'rust' ? 'rust' : 'node';
      next.components[path] = {
        role: change.role,
        toolchain,
        toolingProfile: change.profile,
        origin: { template: 'adopted', version: release.version },
        toolDependencies: tools,
        dependencies: [],
        ...(change.role === 'app'
          ? {
              development: {
                owner: 'project' as const,
                kind: 'compose' as const,
                service: change.name,
              },
            }
          : {}),
      };
      verifyOldTools(stagedRoot, next);
      if (toolchain === 'node') {
        const lockPath = join(stagedRoot, path, 'package-lock.json');
        if (existsSync(lockPath)) {
          seeds.push({ prefix: path, lock: json(lockPath) });
        } else if (
          !existsSync(join(stagedRoot, 'package-lock.json')) ||
          !Object.hasOwn(
            object(
              object(json(join(stagedRoot, 'package-lock.json')), 'root lock').packages,
              'lock packages',
            ),
            path,
          )
        ) {
          fail('Adopted Node component requires admitted native lock membership');
        }
      }
    }
    syncNodeWorkspace(stagedRoot, next);
    if (nodeComponents(next).length) {
      seedWorkspaceLock(stagedRoot, next, seeds);
    }
    if (old.schemaVersion === 1 && nodeComponents(next).length) {
      const normalized = await normalizeLegacyLockIdentities(
        json(join(stagedRoot, 'package-lock.json')),
        {
          projectRoot: stagedRoot,
          cwd: stagedRoot,
          cache: join(stagedRoot, '.devkit/cache/npm'),
          ...(options.registry === undefined ? {} : { registry: options.registry }),
        },
      );
      writeJson(join(stagedRoot, 'package-lock.json'), normalized.lock);
      next.legacyIdentityNormalization = {
        schemaVersion: 1,
        basis: 'exact-version-registry-metadata-observed-at-update',
        source: {
          schemaVersion: 1,
          version: old.version,
          releaseIdentity: old.releaseIdentity,
          locks: legacyLocks,
        },
        normalizedRegistryEntries: normalized.normalizedRegistryEntries,
        localArchiveInputs: normalized.localArchives,
      };
    }
    const product = nodeComponents(next).length ? captureProductGraph(stagedRoot, next) : undefined;
    const oldComponents = structuredClone(next.components);
    for (const [path, component] of Object.entries(next.components) as [
      ComponentPath,
      ComponentState,
    ][]) {
      component.toolDependencies = profileTools(
        releaseRoot,
        release,
        component.toolingProfile,
        mode,
      );
      if (component.toolchain === 'node') {
        updateTools(stagedRoot, path, oldComponents[path]!, component);
      }
    }
    next.packageMode = mode;
    next.version = release.version;
    next.releaseIdentity = releaseIdentity(releaseRoot);
    next.generation = randomUUID();
    next.managed = writeManaged(releaseRoot, release, stagedRoot, mode, next);
    for (const name of Object.keys(old.managed)) {
      if (!Object.hasOwn(next.managed, name)) {
        rmSync(safePath(stagedRoot, name), { force: true });
      }
    }
    if (product) {
      await regenerateWorkspaceLock(stagedRoot, options.registry);
      assertProductGraphsPreserved(product, readNativeGraph(stagedRoot));
    }
    for (const [path] of nodeComponents(next)) {
      rmSync(safePath(stagedRoot, `${path}/package-lock.json`), { force: true });
    }
    const deps = deriveComponentDependencies(stagedRoot, next);
    for (const [path, component] of Object.entries(next.components)) {
      component.dependencies = deps[path as ComponentPath]!;
    }
    next.managed = writeManaged(releaseRoot, release, stagedRoot, mode, next);
    for (const name of Object.keys(next.managed)) {
      if (
        !Object.hasOwn(old.managed, name) &&
        identity(safePath(root, name)) !== null &&
        !(
          change?.kind === 'register' &&
          name === `${componentPath(change.role, change.name)}/rust-toolchain.toml` &&
          same(identity(safePath(root, name)), next.managed[name]!)
        )
      ) {
        fail(`New managed file conflicts with project content: ${name}`);
      }
    }
    assertNativeComponents(stagedRoot, next);
    collectNativeInputClosure(stagedRoot, next);
    readRelease(releaseRoot);
    if (releaseIdentity(releaseRoot) !== next.releaseIdentity) {
      fail('Release changed while staging');
    }
    commitUpdate(root, stageName, stagedRoot, next, old, expectedSource, created, options);
    return next;
  } finally {
    if (!existsSync(safePath(root, pendingPath))) {
      rmSync(stage, { recursive: true, force: true });
    }
  }
}
export async function updateProject(
  releaseRoot: string,
  target: string,
  options: UpdateOptions = {},
): Promise<ProjectState> {
  return mutate(releaseRoot, target, options);
}
export async function addProjectComponent(
  releaseRoot: string,
  target: string,
  role: ComponentRole,
  spec: ComponentSpec,
  options: UpdateOptions = {},
): Promise<ProjectState> {
  return mutate(releaseRoot, target, options, { kind: 'add', role, spec });
}
export async function registerProjectComponent(
  releaseRoot: string,
  target: string,
  role: ComponentRole,
  name: string,
  profile: ToolingProfile,
  options: UpdateOptions = {},
): Promise<ProjectState> {
  if (!profiles[profile] || !profiles[profile].roles.includes(role)) {
    fail(`Tooling profile ${profile} cannot register a ${role}`);
  }
  return mutate(releaseRoot, target, options, { kind: 'register', role, name, profile });
}
export async function registerProjectApp(
  releaseRoot: string,
  target: string,
  name: string,
  template: 'node' | 'react' | 'rust',
  options: UpdateOptions = {},
): Promise<ProjectState> {
  return registerProjectComponent(
    releaseRoot,
    target,
    'app',
    name,
    templates[templateId(template)].toolingProfile,
    options,
  );
}
