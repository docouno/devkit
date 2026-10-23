import { existsSync } from 'node:fs';
import { join, posix } from 'node:path';
import { isDeepStrictEqual } from 'node:util';
import { fail, object, text, writeJson } from './files.ts';
import {
  dependencyFields,
  lockedDependency,
  nativeManifest,
  packageLock,
  resolveLockNode,
} from './native-workspace.ts';
import { npmEnvironment, run } from './process.ts';
import type { ComponentPath, ProjectState } from './types.ts';

type Fields = Record<string, unknown>;
interface Edge {
  field: string;
  name: string;
  spec: string;
  to: string | null;
}
interface Node {
  identity: Fields;
  edges: Edge[];
}
export interface NativeGraph {
  nodes: Record<string, Node>;
  roots: Record<string, Edge[]>;
}
export interface ProductGraph {
  graph: NativeGraph;
  roots: Record<string, Edge[]>;
}

function artifactIdentity(location: string, data: Fields): Fields {
  if (!location.includes('node_modules/')) {
    return { local: location, name: data.name, version: data.version };
  }
  const result: Fields = {};
  for (const key of [
    'version',
    'resolved',
    'integrity',
    'os',
    'cpu',
    'libc',
    'optional',
    'devOptional',
    'peer',
    'inBundle',
  ]) {
    if (data[key] !== undefined) {
      result[key] = data[key];
    }
  }
  if (typeof result.resolved === 'string' && result.resolved.startsWith('file:')) {
    result.resolved = `file:${posix.normalize(result.resolved.slice(5))}`;
  }
  return result;
}
export function readNativeGraph(root: string): NativeGraph {
  const packages = packageLock(root),
    nodes: Record<string, Node> = Object.create(null) as Record<string, Node>;
  for (const [location, raw] of Object.entries(packages)) {
    if (raw.link === true) {
      continue;
    }
    const data =
      !location.includes('node_modules/') && existsSync(join(root, location, 'package.json'))
        ? nativeManifest(root, location)
        : raw;
    const edges: Edge[] = [];
    for (const field of dependencyFields) {
      for (const [name, value] of Object.entries(object(data[field] ?? {}, field))) {
        const spec = text(value, `${location}:${name}`);
        if (/^(git|https?:|npm:|workspace:)/.test(spec)) {
          fail(`Unsupported dependency for preserved graph: ${location}:${name}`);
        }
        const to = lockedDependency(packages, location, name);
        const optional =
          field === 'optionalDependencies' ||
          (field === 'peerDependencies' &&
            object(object(data.peerDependenciesMeta ?? {}, 'peer metadata')[name] ?? {}, name)
              .optional === true);
        if (to === null && !optional) {
          fail(`Native lock omits dependency ${location}:${name}`);
        }
        edges.push({ field, name, spec, to });
      }
    }
    nodes[location] = { identity: artifactIdentity(location, data), edges };
  }
  return {
    nodes,
    roots: Object.fromEntries(
      Object.entries(nodes)
        .filter(([name]) => !name.includes('node_modules/'))
        .map(([name, node]) => [name, node.edges]),
    ),
  };
}
export function captureProductGraph(root: string, state: ProjectState): ProductGraph {
  const graph = readNativeGraph(root),
    roots: Record<string, Edge[]> = {};
  for (const [path, edges] of Object.entries(graph.roots)) {
    if (path && !Object.hasOwn(state.components, path)) {
      continue;
    }
    const owned = state.components[path as ComponentPath]?.toolDependencies ?? {};
    roots[path] = edges.filter(
      (edge) => edge.field !== 'devDependencies' || !Object.hasOwn(owned, edge.name),
    );
  }
  return { graph, roots };
}
export function assertProductGraphsPreserved(before: ProductGraph, after: NativeGraph): void {
  for (const [root, edges] of Object.entries(before.roots)) {
    const mapping = new Map<string, string>(),
      seen = new Set<string>();
    function compareEdge(old: Edge, next: Edge | undefined, label: string): void {
      if (
        !next ||
        old.field !== next.field ||
        old.name !== next.name ||
        old.spec !== next.spec ||
        (old.to === null) !== (next.to === null)
      ) {
        fail(`Product dependency conflict: ${label}`);
      }
      if (old.to === null || next.to === null) {
        return;
      }
      const mapped = mapping.get(old.to);
      if (mapped !== undefined && mapped !== next.to) {
        fail(`Product shared dependency split: ${label}`);
      }
      mapping.set(old.to, next.to);
      const pair = `${old.to}\0${next.to}`;
      if (seen.has(pair)) {
        return;
      }
      seen.add(pair);
      const a = before.graph.nodes[old.to],
        b = after.nodes[next.to];
      if (!a || !b || !isDeepStrictEqual(a.identity, b.identity)) {
        fail(`Product artifact changed: ${label}`);
      }
      const aa = a.edges.filter((edge) => edge.field !== 'devDependencies'),
        bb = b.edges.filter((edge) => edge.field !== 'devDependencies');
      if (aa.length !== bb.length) {
        fail(`Product dependency topology changed: ${label}`);
      }
      for (const edge of aa) {
        compareEdge(
          edge,
          bb.find((item) => item.field === edge.field && item.name === edge.name),
          `${label} -> ${edge.name}`,
        );
      }
    }
    const final = after.roots[root];
    if (!final) {
      fail(`Product native root disappeared: ${root}`);
    }
    for (const edge of edges) {
      compareEdge(
        edge,
        final.find((item) => item.field === edge.field && item.name === edge.name),
        `${root || 'root'}:${edge.name}`,
      );
    }
  }
}
export interface LockSeed {
  prefix: string;
  lock: unknown;
}
export function seedWorkspaceLock(root: string, state: ProjectState, seeds: LockSeed[]): void {
  const manifest = nativeManifest(root, ''),
    packages: Record<string, Fields> = { '': manifest };
  for (const seed of seeds) {
    const lock = object(seed.lock, 'native lock seed');
    if (lock.lockfileVersion !== 3) {
      fail('Migration supports native npm lockfileVersion 3');
    }
    for (const [key, value] of Object.entries(object(lock.packages, 'seed packages'))) {
      const target = posix.normalize(posix.join(seed.prefix, key));
      if (target === '..' || target.startsWith('../') || target.startsWith('/')) {
        fail(`External native lock path: ${target}`);
      }
      const record = { ...object(value, key) };
      if (record.link === true) {
        record.resolved = posix.normalize(
          posix.join(seed.prefix, text(record.resolved, 'link target')),
        );
      } else if (typeof record.resolved === 'string' && record.resolved.startsWith('file:')) {
        record.resolved = `file:${posix.normalize(posix.join(seed.prefix, record.resolved.slice(5)))}`;
      }
      if (target === '.') {
        continue;
      }
      if (packages[target] && !isDeepStrictEqual(packages[target], record)) {
        const existing = packages[target];
        if (
          existing.version !== record.version ||
          existing.integrity !== record.integrity ||
          existing.resolved !== record.resolved
        ) {
          fail(`Conflicting native lock seed: ${target}`);
        }
        packages[target] = { ...existing, ...record };
      } else {
        packages[target] = record;
      }
    }
  }
  for (const [path, component] of Object.entries(state.components)) {
    if (component.toolchain !== 'node') {
      continue;
    }
    const current = nativeManifest(root, path),
      name = text(current.name, 'workspace package name');
    for (const [location, value] of Object.entries(packages)) {
      if (
        /^node_modules\/(?:@[^/]+\/)?[^/]+$/.test(location) &&
        value.link === true &&
        value.resolved === path &&
        location !== `node_modules/${name}`
      ) {
        delete packages[location];
      }
    }
    packages[path] = current;
    const link = `node_modules/${name}`;
    if (packages[link] && packages[link].link !== true) {
      fail(`Workspace name collides with installed root package: ${name}`);
    }
    packages[link] = { resolved: path, link: true };
  }
  for (const [location, record] of Object.entries(packages)) {
    if (record.link === true) {
      resolveLockNode(packages, location);
    }
  }
  writeJson(join(root, 'package-lock.json'), {
    name: manifest.name,
    version: manifest.version,
    lockfileVersion: 3,
    requires: true,
    packages,
  });
}
export async function regenerateWorkspaceLock(root: string, registry?: string): Promise<void> {
  await run(
    'npm',
    [
      'install',
      '--package-lock-only',
      '--package-lock=true',
      '--ignore-scripts',
      '--include=dev',
      '--strict-peer-deps',
      '--legacy-peer-deps=false',
      '--force=false',
    ],
    { cwd: root, env: npmEnvironment(join(root, '.devkit/cache/npm'), registry), quiet: true },
  );
}
export { assertNpmrcCompatibility } from './native-workspace.ts';
