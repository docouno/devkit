import { existsSync, lstatSync, readFileSync, readdirSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { isDeepStrictEqual } from 'node:util';
import { parse as parseToml } from 'smol-toml';
import { nodeComponents } from './catalog.ts';
import { digest, fail, identity, inputPath, json, object, safePath, text } from './files.ts';
import type { ComponentPath, InputIdentitySet, ProjectState } from './types.ts';

export const dependencyFields = [
  'dependencies',
  'optionalDependencies',
  'peerDependencies',
  'devDependencies',
] as const;
export function nativeManifest(root: string, path: string): Record<string, unknown> {
  return object(
    json(inputPath(root, `${path ? `${path}/` : ''}package.json`)),
    `native manifest ${path}`,
  );
}
export function packageLock(root: string): Record<string, Record<string, unknown>> {
  assertPackageLockMode(root);
  const lock = object(json(safePath(root, 'package-lock.json')), 'workspace lock');
  if (lock.lockfileVersion !== 3) {
    fail('Expected native npm lockfileVersion 3');
  }
  const packages = Object.fromEntries(
    Object.entries(object(lock.packages, 'lock packages')).map(([name, value]) => [
      name,
      object(value, name),
    ]),
  );
  for (const [location, record] of Object.entries(packages)) {
    if (!location.includes('node_modules/') || record.link === true) {
      continue;
    }
    if (
      typeof record.resolved !== 'string' ||
      typeof record.integrity !== 'string' ||
      !/^(?:https?:\/\/|file:)/.test(record.resolved) ||
      !/^sha(?:1|256|384|512)-[A-Za-z0-9+/]+={0,2}$/.test(record.integrity)
    ) {
      fail(`Unprovable native artifact identity: ${location}`);
    }
  }
  return packages;
}
export function assertPackageLockMode(root: string): void {
  if (identity(inputPath(root, 'npm-shrinkwrap.json')) !== null) {
    fail('Root npm-shrinkwrap.json conflicts with the accepted package-lock.json mode');
  }
}
export function localPath(root: string, base: string, value: string): string {
  const target = inputPath(root, resolve(root, base, value));
  return relative(root, target).split('\\').join('/');
}
export function resolveLockNode(
  packages: Record<string, Record<string, unknown>>,
  location: string,
): string {
  const visited = new Set<string>();
  while (packages[location]?.link === true) {
    if (visited.has(location)) {
      fail(`Circular native link: ${location}`);
    }
    visited.add(location);
    const target = text(packages[location]!.resolved, 'link target');
    if (target.startsWith('/') || target.split('/').includes('..')) {
      fail(`External native link: ${target}`);
    }
    location = target.replace(/^\.\//, '');
  }
  if (!packages[location]) {
    fail(`Native lock target is missing: ${location}`);
  }
  return location;
}
/** Read the chosen physical npm graph; no version selection or semver resolution. */
export function lockedDependency(
  packages: Record<string, Record<string, unknown>>,
  from: string,
  name: string,
): string | null {
  if (!/^(?:@[^/]+\/)?[^/]+$/.test(name)) {
    fail(`Invalid dependency key: ${name}`);
  }
  let directory = from;
  while (true) {
    const candidate = `${directory ? `${directory}/` : ''}node_modules/${name}`;
    if (packages[candidate]) {
      return resolveLockNode(packages, candidate);
    }
    if (!directory) {
      return null;
    }
    directory = dirname(directory).replace(/^\.$/, '');
  }
}

export function localDependencyMap(
  root: string,
  state: ProjectState,
  path: ComponentPath,
): Record<string, ComponentPath> {
  if (state.components[path]?.toolchain !== 'node') {
    return {};
  }
  const manifest = nativeManifest(root, path),
    packages = packageLock(root),
    result: Record<string, ComponentPath> = {};
  for (const field of dependencyFields) {
    for (const name of Object.keys(object(manifest[field] ?? {}, field))) {
      const target = lockedDependency(packages, path, name);
      if (target !== null && Object.hasOwn(state.components, target)) {
        result[name] = target as ComponentPath;
      }
    }
  }
  return result;
}
function cargoInputs(root: string, path: string): { dependencies: string[]; members: string[] } {
  const manifest = object(
    parseToml(readFileSync(inputPath(root, `${path}/Cargo.toml`), 'utf8')),
    'Cargo manifest',
  );
  const dependencies = new Set<string>();
  const collect = (value: unknown): void => {
    const tables = object(value ?? {}, 'Cargo dependency tables');
    for (const field of ['dependencies', 'dev-dependencies', 'build-dependencies']) {
      for (const dep of Object.values(object(tables[field] ?? {}, field))) {
        if (
          typeof dep === 'object' &&
          dep !== null &&
          !Array.isArray(dep) &&
          object(dep, 'Cargo dependency').path !== undefined
        ) {
          dependencies.add(
            localPath(root, path, text(object(dep, 'Cargo dependency').path, 'Cargo path')),
          );
        }
      }
    }
  };
  collect(manifest);
  for (const target of Object.values(object(manifest.target ?? {}, 'Cargo targets'))) {
    collect(target);
  }
  const workspace = object(manifest.workspace ?? {}, 'Cargo workspace');
  collect(workspace);
  const members: string[] = [];
  if (workspace.members !== undefined) {
    if (!Array.isArray(workspace.members)) {
      fail('Cargo workspace members must be an array');
    }
    for (const member of workspace.members) {
      const value = text(member, 'Cargo member');
      if (/[?*]/.test(value) || value.includes('[') || value.includes(']')) {
        fail('Cargo workspace member globs require explicit registration');
      }
      members.push(localPath(root, path, value));
    }
  }
  return { dependencies: [...dependencies], members };
}
export function deriveComponentDependencies(
  root: string,
  state: ProjectState,
): Record<ComponentPath, ComponentPath[]> {
  const nodes = nodeComponents(state);
  const packages = nodes.length ? packageLock(root) : {};
  const result = {} as Record<ComponentPath, ComponentPath[]>;
  for (const [path, component] of Object.entries(state.components) as [
    ComponentPath,
    ProjectState['components'][ComponentPath],
  ][]) {
    const deps = new Set<ComponentPath>();
    if (component.toolchain === 'node') {
      const manifest = nativeManifest(root, path);
      for (const field of dependencyFields) {
        for (const name of Object.keys(object(manifest[field] ?? {}, field))) {
          const target = lockedDependency(packages, path, name);
          if (target !== null && Object.hasOwn(state.components, target)) {
            const dep = target as ComponentPath;
            if (state.components[dep]!.role !== 'library') {
              fail(`Application dependency is forbidden: ${path} -> ${dep}`);
            }
            if (dep !== path) {
              deps.add(dep);
            }
          }
        }
      }
    }
    if (component.toolchain === 'rust') {
      for (const target of cargoInputs(root, path).dependencies) {
        if (!Object.hasOwn(state.components, target)) {
          continue;
        }
        const dep = target as ComponentPath;
        if (state.components[dep]!.role !== 'library') {
          fail(`Application dependency is forbidden: ${path} -> ${dep}`);
        }
        if (dep !== path) {
          deps.add(dep);
        }
      }
    }
    result[path] = [...deps].sort();
  }
  const active = new Set<string>();
  const done = new Set<string>();
  const visit = (path: ComponentPath): void => {
    if (active.has(path)) {
      fail(`Component dependency cycle: ${path}`);
    }
    if (done.has(path)) {
      return;
    }
    active.add(path);
    for (const dependency of result[path] ?? []) {
      visit(dependency);
    }
    active.delete(path);
    done.add(path);
  };
  for (const path of Object.keys(result) as ComponentPath[]) {
    visit(path);
  }
  return result;
}

export function collectNativeInputClosure(root: string, state: ProjectState): InputIdentitySet {
  const files: InputIdentitySet['files'] = Object.create(null) as InputIdentitySet['files'];
  const directories = new Set<string>(),
    queue = nodeComponents(state).map(([path]) => String(path));
  if (queue.length) {
    queue.unshift('');
  }
  function record(name: string, required = false): void {
    const path = inputPath(root, name);
    const value = identity(path);
    if (required && value === null) {
      fail(`Missing native input: ${name}`);
    }
    files[relative(root, path)] = value;
  }
  function dependency(base: string, spec: string): void {
    if (!spec.startsWith('file:')) {
      return;
    }
    const name = localPath(root, base, spec.slice(5)),
      target = inputPath(root, name);
    if (!existsSync(target)) {
      fail(`Missing local dependency: ${name}`);
    }
    if (lstatSync(target).isDirectory()) {
      queue.push(name);
    } else if (name.endsWith('.tgz') || name.endsWith('.tar.gz')) {
      record(name, true);
    } else {
      fail(`Unsupported local dependency: ${name}`);
    }
  }
  if (queue.length) {
    record('package-lock.json', true);
    for (const item of Object.values(packageLock(root))) {
      if (item.link === true) {
        queue.push(localPath(root, '', text(item.resolved, 'link target')));
      } else if (typeof item.resolved === 'string') {
        dependency('', item.resolved);
      }
    }
  }
  while (queue.length) {
    const path = queue.shift()!;
    if (directories.has(path)) {
      continue;
    }
    directories.add(path);
    const prefix = path ? `${path}/` : '';
    record(`${prefix}package.json`, true);
    for (const file of ['package-lock.json', 'npm-shrinkwrap.json', '.npmrc']) {
      record(`${prefix}${file}`);
    }
    const manifest = nativeManifest(root, path);
    for (const field of dependencyFields) {
      for (const spec of Object.values(object(manifest[field] ?? {}, field))) {
        dependency(path, text(spec, 'dependency spec'));
      }
    }
  }
  const rustQueue = Object.entries(state.components)
    .filter(([, component]) => component.toolchain === 'rust')
    .map(([path]) => path);
  const rustSeen = new Set<string>();
  while (rustQueue.length) {
    const path = rustQueue.shift()!;
    if (rustSeen.has(path)) {
      continue;
    }
    rustSeen.add(path);
    record(`${path}/Cargo.toml`, true);
    for (const name of ['Cargo.lock', 'rust-toolchain.toml', 'rustfmt.toml']) {
      record(`${path}/${name}`, Object.hasOwn(state.components, path));
    }
    record(`${path}/.cargo/config.toml`);
    const native = cargoInputs(root, path);
    rustQueue.push(...native.dependencies, ...native.members);
  }
  const ordered = Object.fromEntries(Object.entries(files).sort(([a], [b]) => a.localeCompare(b)));
  const dirs = [...directories].sort();
  return {
    schemaVersion: 1,
    files: ordered,
    directories: dirs,
    digest: digest(JSON.stringify({ files: ordered, directories: dirs })),
  };
}
export function nativeInputDigest(root: string, state: ProjectState): string {
  return collectNativeInputClosure(root, state).digest;
}

export function assertNativeComponents(root: string, state: ProjectState): void {
  assertComponentManifests(root, state);
  assertToolProfiles(root, state);
  for (const area of ['apps', 'packages']) {
    if (!existsSync(join(root, area))) {
      continue;
    }
    for (const entry of readdirSync(join(root, area), { withFileTypes: true })) {
      if (!entry.isDirectory() && !entry.isSymbolicLink()) {
        continue;
      }
      const name = `${area}/${entry.name}`;
      if (
        ['package.json', 'Cargo.toml'].some((file) => existsSync(join(root, name, file))) &&
        !Object.hasOwn(state.components, name)
      ) {
        fail(`Unregistered native component: ${name}`);
      }
    }
  }
  const nodes = nodeComponents(state);
  const locked = nodes.length ? packageLock(root) : {};
  if (nodes.length) {
    const rootManifest = nativeManifest(root, '');
    if (JSON.stringify(rootManifest.workspaces) !== JSON.stringify(nodes.map(([path]) => path))) {
      fail('Root npm workspace membership differs from registered components');
    }
    for (const field of dependencyFields) {
      if (!isDeepStrictEqual(rootManifest[field] ?? {}, locked['']?.[field] ?? {})) {
        fail(`Root native lock is stale: ${field}`);
      }
    }
    for (const field of ['name', 'version']) {
      if (rootManifest[field] !== locked['']?.[field]) {
        fail('Native package identity projection is stale: root');
      }
    }
  }
  for (const [path, component] of Object.entries(state.components)) {
    if (component.toolchain === 'rust') {
      for (const file of ['Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml']) {
        if (identity(inputPath(root, `${path}/${file}`)) === null) {
          fail(`Missing Rust native input: ${path}/${file}`);
        }
      }
      continue;
    }
    const manifest = nativeManifest(root, path),
      tools = object(manifest.devDependencies ?? {}, 'devDependencies');
    for (const field of ['name', 'version']) {
      if (manifest[field] !== locked[path]?.[field]) {
        fail(`Native package identity projection is stale: ${path}`);
      }
    }
    const link = `node_modules/${text(manifest.name, 'workspace package name')}`;
    if (locked[link]?.link !== true || resolveLockNode(locked, link) !== path) {
      fail(`Native package identity projection is stale: ${path}`);
    }
    for (const [location, value] of Object.entries(locked)) {
      if (
        /^node_modules\/(?:@[^/]+\/)?[^/]+$/.test(location) &&
        value.link === true &&
        resolveLockNode(locked, location) === path &&
        location !== link
      ) {
        fail(`Native package identity projection is stale: ${path}`);
      }
    }
    for (const field of dependencyFields) {
      if (!isDeepStrictEqual(manifest[field] ?? {}, locked[path]?.[field] ?? {})) {
        fail(`Native lock is stale: ${path}:${field}`);
      }
    }
    for (const [name, value] of Object.entries(component.toolDependencies)) {
      if (tools[name] !== value) {
        fail(`Tool dependency conflict: ${path}/package.json devDependencies.${name}`);
      }
    }
    const scripts = object(manifest.scripts, 'scripts');
    for (const command of [
      'check',
      'fix',
      'test',
      'build',
      ...(component.role === 'app' ? ['dev'] : []),
    ]) {
      text(scripts[command], `native script ${path}:${command}`);
    }
    if (component.role === 'library' && manifest.exports === undefined) {
      fail(`Library must define public exports: ${path}`);
    }
  }
  const deps = deriveComponentDependencies(root, state);
  for (const [path, actual] of Object.entries(deps)) {
    if (
      JSON.stringify(actual) !==
      JSON.stringify(state.components[path as ComponentPath]!.dependencies)
    ) {
      fail(`Native dependency projection is stale: ${path}; run the accepted devkit update`);
    }
  }
  const inputs = collectNativeInputClosure(root, state);
  assertNpmrcCompatibility(root, inputs.directories);
}

export function assertComponentManifests(root: string, state: ProjectState): void {
  for (const [path, component] of Object.entries(state.components)) {
    const node = identity(inputPath(root, `${path}/package.json`)) !== null;
    const rust = identity(inputPath(root, `${path}/Cargo.toml`)) !== null;
    if (node && rust) {
      fail(`Ambiguous native manifests: ${path}`);
    }
    if ((component.toolchain === 'node' && !node) || (component.toolchain === 'rust' && !rust)) {
      fail(`Missing expected native ${component.toolchain} manifest: ${path}`);
    }
  }
}

export function assertToolProfiles(root: string, state: ProjectState): void {
  const payload = object(
    json(safePath(root, '.devkit/tooling-profiles.json')),
    'accepted tooling profiles',
  );
  if (
    payload.schemaVersion !== 1 ||
    payload.version !== state.version ||
    payload.packageMode !== state.packageMode
  ) {
    fail('Accepted tooling profile metadata differs from project');
  }
  const profiles = object(payload.profiles, 'tooling profiles');
  for (const [path, component] of Object.entries(state.components)) {
    if (
      !isDeepStrictEqual(
        { ...object(profiles[component.toolingProfile], 'profile tools') },
        { ...component.toolDependencies },
      )
    ) {
      fail(`Accepted tooling profile differs from component: ${path}`);
    }
  }
}

export function assertNpmrcCompatibility(root: string, directories: string[]): void {
  const read = (name: string): Record<string, string> => {
    if (!existsSync(name)) {
      return {};
    }
    const result: Record<string, string> = {};
    for (const line of readFileSync(name, 'utf8').split('\n')) {
      if (!line.trim() || /^\s*[#;]/.test(line)) {
        continue;
      }
      const match = /^\s*([^\s=]+)\s*=\s*([^\r\n]*)$/.exec(line);
      if (!match || /\$|\[|\]/.test(line)) {
        fail(`Unprovable npm configuration: ${relative(root, name)}`);
      }
      result[match[1]!] = match[2]!.trim();
    }
    return result;
  };
  const base = read(join(root, '.npmrc'));
  const defaults: Record<string, string> = {
    'install-links': 'false',
    'strict-peer-deps': 'true',
    'legacy-peer-deps': 'false',
    force: 'false',
    fund: 'false',
    audit: 'false',
    'update-notifier': 'false',
    'install-strategy': 'hoisted',
  };
  for (const directory of directories.filter(Boolean)) {
    for (const [key, value] of Object.entries(read(inputPath(root, `${directory}/.npmrc`)))) {
      if (value !== (base[key] ?? defaults[key])) {
        fail(`npm configuration changes in workspace context: ${directory}/.npmrc (${key})`);
      }
    }
  }
}
