import { createHash } from 'node:crypto';
import { lstatSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { fail, identity, inputPath, object, text } from './files.ts';
import { npmEnvironment, run } from './process.ts';
import { dependencyFields, localPath, lockedDependency } from './native-workspace.ts';
import type { FileIdentity } from './types.ts';

interface Dist {
  tarball: string;
  integrity: string;
}
interface HydrateOptions {
  cwd: string;
  cache: string;
  registry?: string;
  fetchDist?: (name: string, version: string) => Promise<Dist>;
}
const metadata = new Map<string, Promise<Dist>>();
const integrityPattern = /^sha(?:256|384|512)-[A-Za-z0-9+/]+={0,2}$/;
function packageName(location: string): string {
  const name = location.slice(location.lastIndexOf('node_modules/') + 'node_modules/'.length);
  if (!/^(?:@[a-z0-9._-]+\/)?[a-z0-9][a-z0-9._-]*$/i.test(name)) {
    fail(`Unsupported canonical lock path: ${location}`);
  }
  return name;
}
function complete(record: Record<string, unknown>): boolean {
  return (
    typeof record.resolved === 'string' &&
    /^https?:\/\//.test(record.resolved) &&
    typeof record.integrity === 'string' &&
    integrityPattern.test(record.integrity)
  );
}
export function assertCompleteLockIdentities(value: unknown): void {
  const lock = object(value, 'canonical package lock');
  if (lock.lockfileVersion !== 3) {
    fail('Canonical package lock must use lockfileVersion 3');
  }
  for (const [location, value] of Object.entries(
    object(lock.packages, 'canonical lock packages'),
  )) {
    if (!location) {
      continue;
    }
    const entry = object(value, location);
    if (entry.link || entry.inBundle || !location.includes('node_modules/') || !complete(entry)) {
      fail(`Incomplete canonical registry identity: ${location}`);
    }
  }
}
export async function hydrateLockIdentities(
  value: unknown,
  options: HydrateOptions,
): Promise<Record<string, unknown>> {
  const lock = structuredClone(object(value, 'canonical package lock'));
  if (lock.lockfileVersion !== 3) {
    fail('Canonical package lock must use lockfileVersion 3');
  }
  const entries = Object.entries(object(lock.packages, 'canonical lock packages'));
  const pending = entries.filter(
    ([location, value]) => location && !complete(object(value, location)),
  );
  const requests = options.fetchDist ? new Map<string, Promise<Dist>>() : metadata;
  let cursor = 0;
  const worker = async (): Promise<void> => {
    while (cursor < pending.length) {
      const [location, value] = pending[cursor++]!;
      const entry = object(value, location);
      if (entry.link || entry.inBundle || !location.includes('node_modules/')) {
        fail(`Unsupported canonical lock entry: ${location}`);
      }
      const name = packageName(location),
        version = text(entry.version, `locked version ${location}`);
      if (!/^\d+\.\d+\.\d+(?:[-+][A-Za-z0-9.-]+)?$/.test(version)) {
        fail(`Canonical lock has a non-exact version: ${location}`);
      }
      const key = `${options.registry ?? 'https://registry.npmjs.org/'}\n${name}@${version}`;
      let request = requests.get(key);
      if (!request) {
        request = options.fetchDist
          ? options.fetchDist(name, version)
          : (async () => {
              const result = await run(
                'npm',
                ['view', `${name}@${version}`, 'dist', '--json', '--prefer-offline'],
                {
                  cwd: options.cwd,
                  env: npmEnvironment(join(options.cache, 'identity-metadata'), options.registry),
                  quiet: true,
                },
              );
              const dist = object(
                JSON.parse(result.stdout),
                `registry identity ${name}@${version}`,
              );
              return {
                tarball: text(dist.tarball, 'dist.tarball'),
                integrity: text(dist.integrity, 'dist.integrity'),
              };
            })();
        requests.set(key, request);
      }
      const dist = await request;
      if (!/^https?:\/\//.test(dist.tarball) || !integrityPattern.test(dist.integrity)) {
        fail(`Unsupported registry identity: ${name}@${version}`);
      }
      if (entry.resolved !== undefined && entry.resolved !== dist.tarball) {
        fail(`Canonical resolved identity conflict: ${location}`);
      }
      if (entry.integrity !== undefined && entry.integrity !== dist.integrity) {
        fail(`Canonical integrity conflict: ${location}`);
      }
      entry.resolved = dist.tarball;
      entry.integrity = dist.integrity;
    }
  };
  await Promise.all(Array.from({ length: Math.min(4, pending.length) }, worker));
  assertCompleteLockIdentities(lock);
  return lock;
}

export interface LegacyIdentityNormalization {
  lock: Record<string, unknown>;
  normalizedRegistryEntries: number;
  localArchives: Record<string, FileIdentity>;
}

/** Schema-1 transition only: identities observed now do not prove historical installs. */
export async function normalizeLegacyLockIdentities(
  value: unknown,
  options: HydrateOptions & { projectRoot: string },
): Promise<LegacyIdentityNormalization> {
  const lock = structuredClone(object(value, 'legacy package lock'));
  if (lock.lockfileVersion !== 3) {
    fail('Legacy identity normalization supports lockfileVersion 3');
  }
  const packages = Object.fromEntries(
    Object.entries(object(lock.packages, 'legacy lock packages')).map(([location, record]) => [
      location,
      object(record, location),
    ]),
  );
  const localNodes = new Map<string, string>();
  for (const [location, record] of Object.entries(packages)) {
    if (record.link === true) {
      continue;
    }
    for (const field of dependencyFields) {
      for (const [name, spec] of Object.entries(object(record[field] ?? {}, field))) {
        if (typeof spec !== 'string' || !spec.startsWith('file:')) {
          continue;
        }
        const target = lockedDependency(packages, location, name);
        if (target === null) {
          fail(`Legacy lock omits local dependency: ${location}:${name}`);
        }
        const path = localPath(options.projectRoot, location, spec.slice(5));
        const info = lstatSync(inputPath(options.projectRoot, path));
        if (info.isDirectory()) {
          if (target !== path) {
            fail(`Legacy local directory target conflicts: ${location}:${name}`);
          }
        } else if (info.isFile()) {
          const previous = localNodes.get(target);
          if (previous !== undefined && previous !== path) {
            fail(`Conflicting legacy local archive: ${target}`);
          }
          localNodes.set(target, path);
        } else {
          fail(`Unsupported legacy native input: ${path}`);
        }
      }
    }
    if (typeof record.resolved === 'string' && record.resolved.startsWith('file:')) {
      const path = localPath(options.projectRoot, '', record.resolved.slice(5));
      const previous = localNodes.get(location);
      if (previous !== undefined && previous !== path) {
        fail(`Legacy local archive identity conflicts: ${location}`);
      }
      localNodes.set(location, path);
    }
  }
  const localArchives: Record<string, FileIdentity> = {};
  const registry: Record<string, unknown> = { '': {} };
  let normalizedRegistryEntries = 0;
  for (const [location, record] of Object.entries(packages)) {
    if (!location || record.link === true || !location.includes('node_modules/')) {
      continue;
    }
    const archive = localNodes.get(location);
    if (archive !== undefined) {
      if (
        record.resolved !== undefined &&
        (typeof record.resolved !== 'string' || !record.resolved.startsWith('file:'))
      ) {
        fail(`Legacy local archive identity conflicts: ${location}`);
      }
      const absolute = inputPath(options.projectRoot, archive),
        bytes = readFileSync(absolute);
      localArchives[archive] = identity(absolute)!;
      const expected = record.integrity;
      if (expected !== undefined) {
        const match = /^(sha(?:1|256|384|512))-([A-Za-z0-9+/]+={0,2})$/.exec(
          text(expected, 'local archive integrity'),
        );
        if (
          !match ||
          `${match[1]}-${createHash(match[1]!).update(bytes).digest('base64')}` !== expected
        ) {
          fail(`Legacy local archive integrity conflict: ${location}`);
        }
      } else {
        record.integrity = `sha512-${createHash('sha512').update(bytes).digest('base64')}`;
      }
      if (record.resolved === undefined) {
        record.resolved = `file:${archive}`;
      }
      continue;
    }
    registry[location] = record;
    if (!complete(record)) {
      normalizedRegistryEntries += 1;
    }
  }
  const hydrated = await hydrateLockIdentities({ lockfileVersion: 3, packages: registry }, options);
  for (const [location, record] of Object.entries(
    object(hydrated.packages, 'normalized registry packages'),
  )) {
    if (location) {
      packages[location] = object(record, location);
    }
  }
  lock.packages = packages;
  return { lock, normalizedRegistryEntries, localArchives };
}
