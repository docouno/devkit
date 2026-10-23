import { randomUUID } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import {
  copyTree,
  fail,
  identity,
  json,
  object,
  readRelease,
  relativePath,
  safePath,
  same,
  slug,
  text,
  writeAtomic,
  writeJson,
} from './files.ts';
import {
  assertManaged,
  assertNoPending,
  excludedSnapshotPath,
  fileIdentity,
  managedPath,
  manifestPath,
  pendingPath,
  projectRoot,
  projectState,
  sourceSignature,
} from './project-state.ts';
import { regenerateLock, releaseIdentity, templateTools, writeManaged } from './project.ts';
import type {
  AppState,
  FileIdentity,
  PackageMode,
  PlannedFile,
  ProjectState,
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
}

function optionalIdentity(value: unknown, label: string): FileIdentity | null {
  return value === null ? null : fileIdentity(value, label);
}
function transactionPath(name: string): string {
  relativePath(name);
  if (name === manifestPath || /^apps\/[a-z][a-z0-9-]*\/package(?:-lock)?\.json$/.test(name)) {
    return name;
  }
  return managedPath(name);
}

function journal(root: string): UpdateJournal {
  const raw = object(json(safePath(root, pendingPath)), 'update journal');
  if (raw.schemaVersion !== 1 || !Array.isArray(raw.files)) {
    fail('Unsupported update journal');
  }
  const stage = text(raw.stage, 'update stage');
  if (!/^\.devkit\/state\/update-[a-f0-9-]{36}$/.test(stage)) {
    fail('Invalid update stage path');
  }
  safePath(root, stage);
  const names = new Set<string>();
  const files = raw.files.map((value, index): PlannedFile => {
    const file = object(value, 'journal file');
    const path = transactionPath(text(file.path, 'journal target'));
    if (names.has(path)) {
      fail(`Duplicate journal target: ${path}`);
    }
    names.add(path);
    const before = optionalIdentity(file.before, `before ${path}`);
    const after = optionalIdentity(file.after, `after ${path}`);
    if (same(before, after)) {
      fail(`Unchanged journal target: ${path}`);
    }
    const backup = before === null ? null : `${stage}/backup/${index}`;
    const staged = after === null ? null : `${stage}/staged/${index}`;
    if (file.backup !== backup || file.staged !== staged) {
      fail(`Invalid saved path: ${path}`);
    }
    return { path, before, after, backup, staged };
  });
  if (files.length === 0 || !names.has(manifestPath)) {
    fail('Update journal is missing the accepted manifest');
  }
  return { schemaVersion: 1, stage, files };
}

function verifySaved(root: string, file: PlannedFile): void {
  if (file.backup !== null && !same(identity(safePath(root, file.backup)), file.before)) {
    fail(`Update backup integrity mismatch: ${file.path}`);
  }
  if (file.staged !== null && !same(identity(safePath(root, file.staged)), file.after)) {
    fail(`Update staged integrity mismatch: ${file.path}`);
  }
}

/** A pending transaction is always recovered to its pre-update state. */
export function recoverProject(target: string): boolean {
  const root = projectRoot(target);
  safePath(root, '.devkit/state/task.lock');
  const pending = identity(safePath(root, pendingPath));
  if (pending === null) {
    return false;
  }
  const transaction = journal(root);
  // Validate every file first: an editor's unknown bytes must never be overwritten.
  for (const file of transaction.files) {
    verifySaved(root, file);
    const actual = identity(safePath(root, file.path));
    if (!same(actual, file.before) && !same(actual, file.after)) {
      fail(
        `Recovery conflict: ${file.path}; preserve your edits and restore a known before/after state`,
      );
    }
  }
  if (!same(identity(safePath(root, pendingPath)), pending)) {
    fail('Update journal changed during recovery');
  }
  for (const file of [...transaction.files].reverse()) {
    const path = safePath(root, file.path);
    const actual = identity(path);
    if (same(actual, file.before)) {
      continue;
    }
    if (!same(actual, file.after)) {
      fail(`Recovery conflict: ${file.path}`);
    }
    if (file.backup === null) {
      rmSync(path, { force: true });
    } else {
      writeAtomic(path, readFileSync(safePath(root, file.backup)), file.before!.mode);
    }
  }
  rmSync(safePath(root, pendingPath));
  rmSync(safePath(root, transaction.stage), { recursive: true });
  return true;
}

function assertRegisteredApps(root: string, state: ProjectState): void {
  const appsRoot = safePath(root, 'apps');
  for (const entry of readdirSync(appsRoot, { withFileTypes: true })) {
    if (!entry.isDirectory() && !entry.isSymbolicLink()) {
      continue;
    }
    const native = ['package.json', 'Cargo.toml'].some((file) =>
      existsSync(join(appsRoot, entry.name, file)),
    );
    if (native && !Object.hasOwn(state.apps, entry.name)) {
      fail(
        `Unregistered native app: apps/${entry.name}; run devkit register with the accepted release before updating`,
      );
    }
  }
}

function verifyAppOwnership(root: string, name: string, app: AppState): void {
  const appRoot = safePath(root, `apps/${name}`);
  if (!existsSync(appRoot)) {
    fail(`Registered app is missing: ${name}`);
  }
  if (app.kind === 'rust') {
    if (
      identity(safePath(root, `apps/${name}/Cargo.toml`)) === null ||
      identity(safePath(root, `apps/${name}/Cargo.lock`)) === null
    ) {
      fail(`Rust app ${name} requires native manifest and lock`);
    }
    if (existsSync(join(appRoot, 'package.json'))) {
      fail(`Ambiguous native manifest: ${name}`);
    }
    return;
  }
  if (existsSync(join(appRoot, 'Cargo.toml'))) {
    fail(`Ambiguous native manifest: ${name}`);
  }
  const manifest = object(json(safePath(root, `apps/${name}/package.json`)), `manifest of ${name}`);
  const dependencies = object(manifest.devDependencies, `devDependencies of ${name}`);
  for (const [dependency, expected] of Object.entries(app.toolDependencies)) {
    if (dependencies[dependency] !== expected) {
      fail(`Tool dependency conflict: apps/${name}/package.json devDependencies.${dependency}`);
    }
  }
  if (identity(safePath(root, `apps/${name}/package-lock.json`)) === null) {
    fail(`App ${name} is missing its package lock`);
  }
}

function nativeManifestTools(root: string, name: string, old: AppState, next: AppState): void {
  const path = safePath(root, `apps/${name}/package.json`);
  const manifest = object(json(path), `manifest of ${name}`);
  const dependencies = object(manifest.devDependencies, `devDependencies of ${name}`);
  for (const dependency of Object.keys(old.toolDependencies)) {
    delete dependencies[dependency];
  }
  for (const [dependency, spec] of Object.entries(next.toolDependencies)) {
    if (manifest.dependencies !== undefined) {
      const production = object(manifest.dependencies, `dependencies of ${name}`);
      if (Object.hasOwn(production, dependency) && production[dependency] !== spec) {
        fail(`Tool dependency conflicts with product dependency: ${name}/${dependency}`);
      }
    }
    if (
      !Object.hasOwn(old.toolDependencies, dependency) &&
      Object.hasOwn(dependencies, dependency) &&
      dependencies[dependency] !== spec
    ) {
      fail(`New tool dependency conflicts with project dependency: ${name}/${dependency}`);
    }
    dependencies[dependency] = spec;
  }
  writeJson(path, manifest);
}

function commitUpdate(
  root: string,
  stageName: string,
  stagedRoot: string,
  next: ProjectState,
  old: ProjectState,
  expectedSource: string,
  hooks?: UpdateHooks,
): void {
  const allPaths = new Set([
    ...Object.keys(old.managed),
    ...Object.keys(next.managed),
    manifestPath,
  ]);
  for (const [name, app] of Object.entries(next.apps)) {
    if (app.kind === 'node') {
      allPaths.add(`apps/${name}/package.json`);
      allPaths.add(`apps/${name}/package-lock.json`);
    }
  }
  // Preserve unknown state metadata, as well as native package fields.
  const oldRaw = object(json(safePath(root, manifestPath)), 'project manifest');
  const oldApps = object(oldRaw.apps, 'registered apps');
  writeJson(safePath(stagedRoot, manifestPath), {
    ...oldRaw,
    ...next,
    apps: Object.fromEntries(
      Object.entries(next.apps).map(([name, app]) => [
        name,
        { ...(oldApps[name] === undefined ? {} : object(oldApps[name], name)), ...app },
      ]),
    ),
  });
  const files: PlannedFile[] = [];
  for (const name of [...allPaths].sort((a, b) =>
    a === manifestPath ? 1 : b === manifestPath ? -1 : a.localeCompare(b),
  )) {
    transactionPath(name);
    const before = identity(safePath(root, name));
    const after = identity(safePath(stagedRoot, name));
    if (same(before, after)) {
      continue;
    }
    const index = files.length;
    const backup = before === null ? null : `${stageName}/backup/${index}`;
    const staged = after === null ? null : `${stageName}/staged/${index}`;
    if (backup !== null) {
      writeAtomic(safePath(root, backup), readFileSync(safePath(root, name)), before!.mode);
    }
    if (staged !== null) {
      writeAtomic(safePath(root, staged), readFileSync(safePath(stagedRoot, name)), after!.mode);
    }
    files.push({ path: name, before, after, backup, staged });
  }
  hooks?.afterStage?.();
  if (sourceSignature(root) !== expectedSource) {
    fail('Project changed while staging the update; retry after reviewing concurrent edits');
  }
  for (const file of files) {
    if (!same(identity(safePath(root, file.path)), file.before)) {
      fail(`Concurrent update conflict: ${file.path}`);
    }
  }
  assertNoPending(root);
  const transaction: UpdateJournal = {
    schemaVersion: 1,
    stage: stageName,
    files,
  };
  writeJson(safePath(root, pendingPath), transaction);
  try {
    for (const [index, file] of files.entries()) {
      const path = safePath(root, file.path);
      if (!same(identity(path), file.before)) {
        fail(`Concurrent update conflict: ${file.path}`);
      }
      verifySaved(root, file);
      if (file.staged === null) {
        rmSync(path, { force: true });
      } else {
        writeAtomic(path, readFileSync(safePath(root, file.staged)), file.after!.mode);
      }
      hooks?.afterWrite?.(index + 1);
    }
    rmSync(safePath(root, pendingPath));
  } catch (error) {
    try {
      recoverProject(root);
    } catch (recoveryError) {
      fail(
        `${String(error)}; automatic rollback was blocked: ${String(recoveryError)}. Run devkit recover after resolving the conflict.`,
      );
    }
    throw error;
  }
}

export async function updateProject(
  releaseRoot: string,
  target: string,
  options: UpdateOptions = {},
): Promise<ProjectState> {
  const release = readRelease(releaseRoot);
  const root = projectRoot(target);
  safePath(root, '.devkit/state/task.lock');
  assertNoPending(root);
  const old = projectState(root);
  assertManaged(root, old);
  assertRegisteredApps(root, old);
  for (const [name, app] of Object.entries(old.apps)) {
    verifyAppOwnership(root, name, app);
  }
  const nextIdentity = releaseIdentity(releaseRoot);
  const mode = options.packageMode ?? old.packageMode;
  if (mode !== 'local' && mode !== 'registry') {
    fail('Invalid package source');
  }
  if (old.version === release.version && old.releaseIdentity !== nextIdentity) {
    fail('A different release has the same version; use a distinctly versioned release');
  }
  if (old.releaseIdentity === nextIdentity && mode === old.packageMode) {
    return old;
  }
  const expectedSource = sourceSignature(root);
  const stageName = `.devkit/state/update-${randomUUID()}`;
  const stage = safePath(root, stageName);
  const stagedRoot = join(stage, 'project');
  mkdirSync(stage, { recursive: true });
  try {
    copyTree(root, stagedRoot, {
      ignore: excludedSnapshotPath,
      preserveLinks: true,
    });
    const apps: Record<string, AppState> = {};
    for (const [name, app] of Object.entries(old.apps)) {
      apps[name] = {
        ...app,
        toolDependencies: templateTools(releaseRoot, release, app.template, mode),
      };
    }
    for (const name of Object.keys(old.managed)) {
      rmSync(safePath(stagedRoot, name), { force: true });
    }
    const managed = writeManaged(releaseRoot, release, stagedRoot, mode, apps);
    for (const name of Object.keys(managed)) {
      if (!Object.hasOwn(old.managed, name) && identity(safePath(root, name)) !== null) {
        fail(`New managed file conflicts with project content: ${name}`);
      }
    }
    for (const [name, app] of Object.entries(apps)) {
      if (app.kind === 'node') {
        nativeManifestTools(stagedRoot, name, old.apps[name]!, app);
        await regenerateLock(stagedRoot, name, options.registry);
      }
    }
    const next: ProjectState = {
      schemaVersion: 1,
      version: release.version,
      releaseIdentity: nextIdentity,
      generation: randomUUID(),
      packageMode: mode,
      managed,
      apps,
    };
    readRelease(releaseRoot);
    if (releaseIdentity(releaseRoot) !== nextIdentity) {
      fail('Release changed while staging the update');
    }
    commitUpdate(root, stageName, stagedRoot, next, old, expectedSource, options.hooks);
    return next;
  } finally {
    // On process interruption this block does not run; pending state retains its backups.
    if (!existsSync(safePath(root, pendingPath))) {
      rmSync(stage, { recursive: true, force: true });
    }
  }
}

/** Adopt an existing native app; adding it to DEVKIT_APPS remains a project Make edit. */
export async function registerProjectApp(
  releaseRoot: string,
  target: string,
  name: string,
  template: 'node' | 'react' | 'rust',
  options: UpdateOptions = {},
): Promise<ProjectState> {
  slug(name);
  if (!['node', 'react', 'rust'].includes(template)) {
    fail(`Unknown app template: ${template}`);
  }
  const release = readRelease(releaseRoot);
  const root = projectRoot(target);
  safePath(root, '.devkit/state/task.lock');
  assertNoPending(root);
  const old = projectState(root);
  assertManaged(root, old);
  if (old.releaseIdentity !== releaseIdentity(releaseRoot)) {
    fail("Register apps using the project's exact accepted release");
  }
  if (Object.hasOwn(old.apps, name)) {
    fail(`App is already registered: ${name}`);
  }
  for (const [appName, app] of Object.entries(old.apps)) {
    verifyAppOwnership(root, appName, app);
  }
  const templateName = `${template === 'react' ? 'react' : template === 'rust' ? 'rust' : 'node'}-app`;
  const app: AppState = {
    kind: template === 'rust' ? 'rust' : 'node',
    template: templateName,
    toolDependencies: templateTools(releaseRoot, release, templateName, old.packageMode),
  };
  verifyAppOwnership(root, name, app);
  if (app.kind === 'node') {
    const manifest = object(
      json(safePath(root, `apps/${name}/package.json`)),
      `manifest of ${name}`,
    );
    const scripts = object(manifest.scripts, `scripts of ${name}`);
    for (const script of ['check', 'fix', 'test', 'build', 'dev']) {
      text(scripts[script], `native script ${script}`);
    }
  } else {
    const expected = release.files[`templates/${templateName}/rust-toolchain.toml`];
    if (
      !expected ||
      !same(identity(safePath(root, `apps/${name}/rust-toolchain.toml`)), expected)
    ) {
      fail(`App ${name} must use the accepted Rust toolchain pin`);
    }
  }
  const expectedSource = sourceSignature(root);
  const stageName = `.devkit/state/update-${randomUUID()}`;
  const stage = safePath(root, stageName);
  const stagedRoot = join(stage, 'project');
  mkdirSync(stage, { recursive: true });
  try {
    copyTree(root, stagedRoot, {
      ignore: excludedSnapshotPath,
      preserveLinks: true,
    });
    const apps = { ...old.apps, [name]: app };
    const managed = writeManaged(releaseRoot, release, stagedRoot, old.packageMode, apps);
    if (app.kind === 'node') {
      await regenerateLock(stagedRoot, name, options.registry);
    }
    readRelease(releaseRoot);
    if (releaseIdentity(releaseRoot) !== old.releaseIdentity) {
      fail('Release changed while registering the app');
    }
    const next: ProjectState = {
      ...old,
      generation: randomUUID(),
      apps,
      managed,
    };
    commitUpdate(root, stageName, stagedRoot, next, old, expectedSource, options.hooks);
    return next;
  } finally {
    if (!existsSync(safePath(root, pendingPath))) {
      rmSync(stage, { recursive: true, force: true });
    }
  }
}
