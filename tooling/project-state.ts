import { lstatSync, readFileSync, readdirSync, readlinkSync } from 'node:fs';
import { isAbsolute, join, parse, resolve } from 'node:path';
import {
  digest,
  fail,
  identity,
  json,
  object,
  relativePath,
  safePath,
  same,
  slug,
  text,
  version,
} from './files.ts';
import type { AppState, FileIdentity, ProjectState } from './types.ts';

export const manifestPath = '.devkit/manifest.json';
export const pendingPath = '.devkit/update-pending.json';
export const checksumPath = '.devkit/runtime.sha256';

/** Reject existing symlink ancestors before creating state or writing managed files. */
export function projectRoot(path: string): string {
  const result = resolve(path);
  let current = parse(result).root;
  for (const part of result.slice(current.length).split('/').filter(Boolean)) {
    current = join(current, part);
    try {
      if (lstatSync(current).isSymbolicLink()) {
        fail(`Refusing a symlink in project path: ${current}`);
      }
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== 'ENOENT') {
        throw error;
      }
    }
  }
  return result;
}

export function fileIdentity(value: unknown, label: string): FileIdentity {
  const raw = object(value, label);
  if (
    typeof raw.sha256 !== 'string' ||
    !/^[a-f0-9]{64}$/.test(raw.sha256) ||
    typeof raw.mode !== 'number' ||
    !Number.isInteger(raw.mode) ||
    raw.mode < 0 ||
    raw.mode > 0o777
  ) {
    fail(`Invalid file identity: ${label}`);
  }
  return { sha256: raw.sha256, mode: raw.mode };
}

export function managedPath(name: string): string {
  relativePath(name);
  if (
    name.startsWith('.devkit/runtime/') ||
    /^\.devkit\/npm\/[a-z0-9.-]+\.tgz$/.test(name) ||
    name === checksumPath ||
    /^apps\/[a-z][a-z0-9-]*\/rust-toolchain\.toml$/.test(name)
  ) {
    return name;
  }
  fail(`Unsupported managed path: ${name}`);
}

export function projectState(root: string): ProjectState {
  const raw = object(json(safePath(root, manifestPath)), 'project manifest');
  if (raw.schemaVersion !== 1) {
    fail('Unsupported project schemaVersion');
  }
  if (raw.packageMode !== 'local' && raw.packageMode !== 'registry') {
    fail('Invalid project packageMode');
  }
  const releaseIdentity = text(raw.releaseIdentity, 'releaseIdentity');
  const generation = text(raw.generation, 'generation');
  if (!/^[a-f0-9]{64}$/.test(releaseIdentity) || !/^[a-f0-9-]{36}$/.test(generation)) {
    fail('Invalid project release identity/generation');
  }
  const managed: Record<string, FileIdentity> = Object.create(null) as Record<string, FileIdentity>;
  for (const [name, value] of Object.entries(object(raw.managed, 'managed files'))) {
    managed[managedPath(name)] = fileIdentity(value, name);
  }
  if (
    !Object.hasOwn(managed, checksumPath) ||
    !Object.hasOwn(managed, '.devkit/runtime/toolchains.env')
  ) {
    fail('Project manifest is missing required managed runtime identities');
  }
  const apps: Record<string, AppState> = Object.create(null) as Record<string, AppState>;
  for (const [name, value] of Object.entries(object(raw.apps, 'registered apps'))) {
    slug(name);
    const app = object(value, `app ${name}`);
    if (app.kind !== 'node' && app.kind !== 'rust') {
      fail(`Invalid app kind: ${name}`);
    }
    const template = text(app.template, `template of ${name}`);
    if (
      !['node-app', 'react-app', 'rust-app'].includes(template) ||
      (app.kind === 'rust') !== (template === 'rust-app')
    ) {
      fail(`Invalid template of ${name}`);
    }
    const toolDependencies: Record<string, string> = Object.create(null) as Record<string, string>;
    for (const [dependency, spec] of Object.entries(
      object(app.toolDependencies, `tool dependencies of ${name}`),
    )) {
      if (!/^(?:@[a-z0-9-]+\/)?[a-z0-9][a-z0-9._-]*$/.test(dependency)) {
        fail(`Invalid tool dependency: ${dependency}`);
      }
      toolDependencies[dependency] = text(spec, dependency);
    }
    apps[name] = { kind: app.kind, template, toolDependencies };
  }
  if (Object.keys(apps).length === 0) {
    fail('Project has no registered apps');
  }
  return {
    schemaVersion: 1,
    version: version(raw.version),
    releaseIdentity,
    generation,
    packageMode: raw.packageMode,
    managed,
    apps,
  };
}

export function assertManaged(root: string, state: ProjectState): void {
  for (const [name, expected] of Object.entries(state.managed)) {
    if (!same(identity(safePath(root, name)), expected)) {
      fail(`Managed file conflict: ${name}`);
    }
  }
}

export function assertNoPending(root: string): void {
  if (identity(safePath(root, pendingPath)) !== null) {
    fail('An update is incomplete; run devkit recover first');
  }
}

export function excludedSnapshotPath(name: string): boolean {
  const parts = name.split('/');
  return (
    parts.some((part) => ['.git', 'node_modules', 'target', '.artifacts'].includes(part)) ||
    /^\.devkit\/(?:state|cache)(?:\/|$)/.test(name)
  );
}

/** Includes user source and native path dependency inputs, without following symlinks. */
export function sourceSignature(root: string): string {
  const records: unknown[] = [];
  function visit(directory: string, prefix: string): void {
    for (const name of readdirSync(directory).sort()) {
      const relative = prefix ? `${prefix}/${name}` : name;
      if (excludedSnapshotPath(relative)) {
        continue;
      }
      const path = join(directory, name);
      const info = lstatSync(path);
      if (info.isSymbolicLink()) {
        records.push([relative, 'link', readlinkSync(path)]);
      } else if (info.isDirectory()) {
        records.push([relative, 'directory']);
        visit(path, relative);
      } else if (info.isFile()) {
        records.push([relative, info.mode & 0o777, digest(readFileSync(path))]);
      } else {
        fail(`Unsupported project input: ${relative}`);
      }
    }
  }
  visit(root, '');
  return digest(JSON.stringify(records));
}

export function within(root: string, path: string): string {
  if (isAbsolute(path)) {
    fail(`Expected a project-relative path: ${path}`);
  }
  return safePath(root, path);
}
