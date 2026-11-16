import { parse, stringify } from 'yaml';
import { isDeepStrictEqual } from 'node:util';
const dependencyFields = [
  'dependencies',
  'devDependencies',
  'optionalDependencies',
  'peerDependencies',
];
const registryRangePattern = /^[~^<>=*v\d][0-9A-Za-z.*+~^<>=| -]*$/;
const npmPackagePattern = /^(?:@[a-z0-9][a-z0-9._-]*\/)?[a-z0-9][a-z0-9._-]*$/;

export function parseRegistryDependencySpec(spec, label) {
  if (typeof spec !== 'string' || spec.length === 0 || spec.length > 256) {
    throw new Error(`Source/local dependency is not an accepted registry range: ${label}`);
  }
  if (spec.startsWith('npm:')) {
    const rest = spec.slice(4);
    if (!rest) {
      throw new Error(`Invalid npm: alias dependency specifier: ${label}`);
    }
    const splitAt = rest.startsWith('@') ? rest.indexOf('@', 1) : rest.indexOf('@');
    const targetName = splitAt === -1 ? rest : rest.slice(0, splitAt);
    const innerSpec = splitAt === -1 ? undefined : rest.slice(splitAt + 1);
    if (
      !npmPackagePattern.test(targetName) ||
      targetName.includes('..') ||
      (innerSpec !== undefined &&
        (!innerSpec ||
          /^(?:npm:|file:|link:|workspace:|git|github:|http:|https:|\/|\.\/|\.\.\/|~$|~\/)/i.test(
            innerSpec,
          ) ||
          !registryRangePattern.test(innerSpec)))
    ) {
      throw new Error(`Source/local dependency is not an accepted registry range: ${label}`);
    }
    return { kind: 'alias', targetName, innerSpec };
  }
  if (!registryRangePattern.test(spec)) {
    throw new Error(`Source/local dependency is not an accepted registry range: ${label}`);
  }
  return { kind: 'range', spec };
}

export function assertRegistryDependencies(value) {
  for (const field of dependencyFields) {
    for (const [name, spec] of Object.entries(object(value[field] ?? {}, field))) {
      if (!npmPackagePattern.test(name) || name.includes('..')) {
        throw new Error(`Invalid package name: ${name}`);
      }
      parseRegistryDependencySpec(spec, name);
    }
  }
}

export function object(value, label) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error(`${label} must be an object`);
  }
  return value;
}

export function exactVersion(value) {
  if (
    typeof value !== 'string' ||
    value.length > 128 ||
    !/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/.test(
      value,
    )
  ) {
    throw new Error(`Invalid exact version: ${String(value)}`);
  }
  const prerelease = value.split('+')[0].split('-').slice(1).join('-');
  if (prerelease.split('.').some((part) => /^0\d+$/.test(part))) {
    throw new Error(`Invalid exact version: ${value}`);
  }
  return value;
}

export function validIntegrity(value) {
  if (typeof value !== 'string') {
    return false;
  }
  const match = /^(sha512)-([A-Za-z0-9+/]+={0,2})$/.exec(value);
  if (!match) {
    return false;
  }
  const bytes = Buffer.from(match[2], 'base64');
  return (
    bytes.length === { sha256: 32, sha384: 48, sha512: 64 }[match[1]] &&
    bytes.toString('base64') === match[2]
  );
}

export function registryUrl(name, version) {
  return `https://registry.npmjs.org/${name}/-/${name.slice(name.lastIndexOf('/') + 1)}-${version}.tgz`;
}

export const parseLock = (text) => parse(text, { maxAliasCount: 0, uniqueKeys: true });
export const encodeLock = (lock) => stringify(lock, { lineWidth: 0 });
const fields = ['dependencies', 'optionalDependencies', 'devDependencies'];
const baseKey = (key) => key.split('(')[0];
function parts(key) {
  const base = baseKey(key);
  const at = base.indexOf('@', base.startsWith('@') ? 1 : 0);
  const name = base.slice(0, at),
    version = base.slice(at + 1);
  if (at < 1 || !npmPackagePattern.test(name) || name.includes('..')) {
    throw new Error(`Unsupported canonical registry identity: ${key}`);
  }
  exactVersion(version);
  if (!/^(?:\([^()]+(?:\([^()]*\))*\))*$/.test(key.slice(base.length))) {
    // Nested peer contexts are checked through their referenced snapshots below.
    let depth = 0;
    for (const c of key.slice(base.length)) {
      if (c === '(') {
        depth++;
      }
      if (c === ')' && --depth < 0) {
        throw new Error(`Invalid peer context: ${key}`);
      }
    }
    if (depth !== 0) {
      throw new Error(`Invalid peer context: ${key}`);
    }
  }
  return { name, version };
}
function entries(value) {
  const lock = object(value, 'canonical pnpm lock');
  if (String(lock.lockfileVersion) !== '9.0') {
    throw new Error('Canonical pnpm lock must use lockfileVersion 9.0');
  }
  const importers = object(lock.importers, 'canonical importers');
  if (Object.keys(importers).length !== 1 || !importers['.']) {
    throw new Error('Canonical templates must have exactly one root importer');
  }
  object(importers['.'], 'canonical root importer');
  object(lock.snapshots ?? {}, 'canonical snapshots');
  return object(lock.packages ?? {}, 'canonical packages');
}
export function snapshotKey(name, ref) {
  if (typeof ref !== 'string') {
    throw new Error(`Invalid dependency reference: ${name}`);
  }
  const key = baseKey(ref).includes('@') ? ref : `${name}@${ref}`;
  parts(key);
  return key;
}
function identity(key, raw, allowMissing) {
  const { name, version } = parts(key),
    record = object(raw, key);
  if (key !== baseKey(key)) {
    throw new Error(`Package metadata has a peer context: ${key}`);
  }
  if (
    record.link ||
    record.inBundle ||
    (record.name !== undefined && record.name !== name) ||
    (record.version !== undefined && record.version !== version)
  ) {
    throw new Error(`Unsupported canonical registry identity: ${key}`);
  }
  const resolution = object(record.resolution ?? {}, `resolution ${key}`);
  if (Object.keys(resolution).some((k) => !['integrity', 'tarball'].includes(k))) {
    throw new Error(`Unsupported canonical registry identity: ${key}`);
  }
  if (resolution.tarball !== undefined && resolution.tarball !== registryUrl(name, version)) {
    throw new Error(`Canonical resolved identity conflict: ${key}`);
  }
  if (resolution.integrity !== undefined && !validIntegrity(resolution.integrity)) {
    throw new Error(`Invalid canonical integrity: ${key}`);
  }
  if (!allowMissing && !validIntegrity(resolution.integrity)) {
    throw new Error(`Incomplete canonical registry identity: ${key}`);
  }
  assertRegistryDependencies({ peerDependencies: record.peerDependencies });
  return { name, version, record, resolution };
}
function checkReferences(lock) {
  const check = (name, ref) => {
    const key = snapshotKey(name, ref);
    if (
      !Object.hasOwn(lock.packages ?? {}, baseKey(key)) ||
      !Object.hasOwn(lock.snapshots ?? {}, key)
    ) {
      throw new Error(`Missing canonical snapshot: ${key}`);
    }
  };
  for (const field of fields) {
    for (const [name, value] of Object.entries(lock.importers['.'][field] ?? {})) {
      const dep = object(value, `importer ${name}`);
      const spec = parseRegistryDependencySpec(dep.specifier, name);
      const target = parts(snapshotKey(name, dep.version)).name;
      if (target !== (spec.kind === 'alias' ? spec.targetName : name)) {
        throw new Error(`Importer resolves to mismatched package: ${name}`);
      }
      check(name, dep.version);
    }
  }
  for (const [key, snapshot] of Object.entries(lock.snapshots ?? {})) {
    parts(key);
    if (!Object.hasOwn(lock.packages ?? {}, baseKey(key))) {
      throw new Error(`Missing canonical package: ${key}`);
    }
    for (const field of ['dependencies', 'optionalDependencies']) {
      for (const [name, ref] of Object.entries(snapshot[field] ?? {})) {
        check(name, ref);
      }
    }
  }
}
export function assertManifestImporter(manifest, lock) {
  entries(lock);
  const root = lock.importers['.'];
  for (const field of fields) {
    const expected = { ...(manifest[field] ?? {}) };
    if (field === 'dependencies') {
      for (const key of Object.keys(manifest.optionalDependencies ?? {})) {
        delete expected[key];
      }
    }
    const actual = object(root[field] ?? {}, field);
    if (
      JSON.stringify(Object.keys(actual).sort()) !== JSON.stringify(Object.keys(expected).sort()) ||
      Object.entries(expected).some(([name, spec]) => actual[name]?.specifier !== spec)
    ) {
      throw new Error(`Template manifest and source lock disagree: ${field}`);
    }
  }
  for (const name of Object.keys(manifest.peerDependencies ?? {})) {
    if (
      !manifest.peerDependenciesMeta?.[name]?.optional &&
      !fields.some((field) => root[field]?.[name])
    ) {
      throw new Error(`Missing template peer resolution: ${name}`);
    }
  }
}
export async function hydrateLockIdentities(value, fetchDist) {
  const lock = structuredClone(value);
  for (const [key, raw] of Object.entries(entries(lock))) {
    const { name, version, record, resolution } = identity(key, raw, true);
    if (resolution.integrity !== undefined) {
      continue;
    }
    const dist = object(await fetchDist(name, version), `registry identity ${key}`);
    if (dist.tarball !== registryUrl(name, version) || !validIntegrity(dist.integrity)) {
      throw new Error(`Unsupported registry identity: ${key}`);
    }
    record.resolution = { integrity: dist.integrity };
  }
  assertCanonicalLock(lock);
  return lock;
}
export function assertCanonicalLock(value) {
  for (const [key, raw] of Object.entries(entries(value))) {
    identity(key, raw, false);
  }
  checkReferences(value);
  if (
    /127\.0\.0\.1|localhost|file:|link:|workspace:|\/workspace\/|\.artifacts\//i.test(
      JSON.stringify(value),
    )
  ) {
    throw new Error('Source/local endpoint remains in canonical lock');
  }
}
function rewriteEnvironmentKey(key, sourceVersion, version, depth = 0) {
  if (depth >= 64) {
    throw new Error('Template peer context exceeds depth budget');
  }
  const base = baseKey(key);
  let result = base === `@docouno/env@${sourceVersion}` ? `@docouno/env@${version}` : base;
  let offset = base.length;
  while (offset < key.length) {
    if (key[offset] !== '(') {
      throw new Error(`Invalid template peer context: ${key}`);
    }
    let end = offset + 1,
      nesting = 1;
    while (end < key.length && nesting > 0) {
      if (key[end] === '(') {
        nesting++;
      }
      if (key[end] === ')') {
        nesting--;
      }
      end++;
    }
    if (nesting !== 0) {
      throw new Error(`Invalid template peer context: ${key}`);
    }
    const peer = key.slice(offset + 1, end - 1);
    result += `(${rewriteEnvironmentKey(peer, sourceVersion, version, depth + 1)})`;
    offset = end;
  }
  return result;
}

function rewriteEnvironmentSelections(input, sourceVersion, version) {
  const lock = structuredClone(input);
  const rewriteKey = (key) => rewriteEnvironmentKey(key, sourceVersion, version);
  const rewriteRef = (name, ref) => {
    const key = rewriteKey(snapshotKey(name, ref));
    return baseKey(ref).includes('@') ? key : key.slice(name.length + 1);
  };
  const remap = (value, change) => {
    const mapped = Object.entries(value).map(([key, record]) => [rewriteKey(key), change(record)]);
    if (new Set(mapped.map(([key]) => key)).size !== mapped.length) {
      throw new Error('Template environment rewrite collides with another selection');
    }
    return Object.fromEntries(mapped);
  };
  for (const importer of Object.values(lock.importers)) {
    for (const field of fields) {
      for (const [name, dependency] of Object.entries(importer[field] ?? {})) {
        dependency.version = rewriteRef(name, dependency.version);
      }
    }
  }
  lock.packages = remap(lock.packages, (record) => record);
  lock.snapshots = remap(lock.snapshots ?? {}, (snapshot) => {
    for (const field of ['dependencies', 'optionalDependencies']) {
      if (snapshot[field]) {
        snapshot[field] = Object.fromEntries(
          Object.entries(snapshot[field]).map(([name, ref]) => [name, rewriteRef(name, ref)]),
        );
      }
    }
    return snapshot;
  });
  return lock;
}

export function prepareTemplate(manifestInput, lockInput, sourceVersion, version) {
  const manifest = structuredClone(object(manifestInput, 'template manifest'));
  assertRegistryDependencies(manifest);
  if (manifest.workspaces !== undefined) {
    throw new Error('Default pnpm templates must be standalone packages');
  }
  if (manifest.devDependencies?.['@docouno/env'] !== sourceVersion) {
    throw new Error('Template must accept the exact source env version');
  }
  for (const field of dependencyFields.filter((f) => f !== 'devDependencies')) {
    if (Object.hasOwn(manifest[field] ?? {}, '@docouno/env')) {
      throw new Error(`Unexpected template env dependency in ${field}`);
    }
  }
  assertManifestImporter(manifest, lockInput);
  const oldKey = `@docouno/env@${sourceVersion}`,
    newKey = `@docouno/env@${version}`;
  if (!Object.hasOwn(entries(lockInput), oldKey)) {
    throw new Error('Template accepted env lock version differs from source');
  }
  // Only typed selections and the owned environment identity are rewritten.
  const lock = rewriteEnvironmentSelections(lockInput, sourceVersion, version);
  manifest.devDependencies['@docouno/env'] = version;
  lock.importers['.'].devDependencies['@docouno/env'].specifier = version;
  const envRef = lock.importers['.'].devDependencies['@docouno/env'];
  if (snapshotKey('@docouno/env', envRef.version).split('(')[0] !== newKey) {
    throw new Error('Template accepted env lock version differs from source');
  }
  if (lock.packages[newKey].version !== undefined) {
    lock.packages[newKey].version = version;
  }
  lock.packages[newKey].resolution = {};
  return { manifest, lock };
}
export function canonicalizeLock(value, registry) {
  const lock = structuredClone(value);
  for (const [key, record] of Object.entries(entries(lock))) {
    const tarball = record.resolution?.tarball;
    if (typeof tarball === 'string' && tarball.startsWith(registry)) {
      const { name, version } = parts(key);
      if (
        `https://registry.npmjs.org/${tarball.slice(registry.length)}` !==
        registryUrl(name, version)
      ) {
        throw new Error(`Canonical resolved identity conflict: ${key}`);
      }
      delete record.resolution.tarball;
    }
  }
  assertCanonicalLock(lock);
  return lock;
}
export function assertSelectedIdentities(previous, next, version, integrity) {
  const before = entries(previous),
    after = entries(next),
    envKey = `@docouno/env@${version}`;
  if (JSON.stringify(Object.keys(before).sort()) !== JSON.stringify(Object.keys(after).sort())) {
    throw new Error('pnpm changed selected external identities');
  }
  for (const [key, record] of Object.entries(before)) {
    if (key !== envKey && record.resolution.integrity !== after[key]?.resolution?.integrity) {
      throw new Error(`pnpm changed a selected external identity: ${key}`);
    }
    const metadata = (value) =>
      Object.fromEntries(Object.entries(value).filter(([field]) => field !== 'resolution'));
    if (!isDeepStrictEqual(metadata(record), metadata(after[key]))) {
      throw new Error(`pnpm changed selected template package metadata: ${key}`);
    }
  }
  const graph = (value) => ({
    ...Object.fromEntries(Object.entries(value).filter(([key]) => key !== 'packages')),
    snapshots: value.snapshots ?? {},
  });
  if (!isDeepStrictEqual(graph(previous), graph(next))) {
    throw new Error('pnpm changed the selected template graph');
  }
  if (
    after[envKey]?.resolution?.integrity !== integrity ||
    next.importers['.'].devDependencies?.['@docouno/env']?.specifier !== version
  ) {
    throw new Error('Normalized env identity differs from the packed archive');
  }
  assertCanonicalLock(next);
}
