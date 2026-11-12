const envLocation = 'node_modules/@docouno/env';
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
      packageName(`node_modules/${name}`);
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

function packageName(location) {
  if (
    !/^(?:node_modules\/(?:@[a-z0-9._-]+\/)?[a-z0-9][a-z0-9._-]*)(?:\/node_modules\/(?:@[a-z0-9._-]+\/)?[a-z0-9][a-z0-9._-]*)*$/i.test(
      location,
    )
  ) {
    throw new Error(`Unsupported canonical lock location: ${location}`);
  }
  return location.slice(location.lastIndexOf('node_modules/') + 'node_modules/'.length);
}

export function validIntegrity(value) {
  if (typeof value !== 'string') {
    return false;
  }
  const match = /^(sha256|sha384|sha512)-([A-Za-z0-9+/]+={0,2})$/.exec(value);
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

function resolveLockedLocation(entries, from, depName) {
  let directory = from;
  for (;;) {
    const candidate = directory
      ? `${directory}/node_modules/${depName}`
      : `node_modules/${depName}`;
    if (Object.hasOwn(entries, candidate)) {
      return candidate;
    }
    if (!directory) {
      return undefined;
    }
    const slash = directory.lastIndexOf('/');
    directory = slash === -1 ? '' : directory.slice(0, slash);
  }
}

function collectAliasTargets(entries) {
  const aliasTargets = new Map();
  const nonAliasLocations = new Set();
  for (const [from, rawEntry] of Object.entries(entries)) {
    const entry = object(rawEntry, from || 'canonical lock root');
    for (const field of dependencyFields) {
      for (const [depName, spec] of Object.entries(object(entry[field] ?? {}, field))) {
        packageName(`node_modules/${depName}`);
        const parsed = parseRegistryDependencySpec(spec, depName);
        const resolvedLoc = resolveLockedLocation(entries, from, depName);
        if (!resolvedLoc) {
          continue;
        }
        if (parsed.kind === 'alias') {
          const previous = aliasTargets.get(resolvedLoc);
          if (
            (previous !== undefined && previous !== parsed.targetName) ||
            (nonAliasLocations.has(resolvedLoc) && parsed.targetName !== packageName(resolvedLoc))
          ) {
            throw new Error(`Conflicting alias target identity at ${resolvedLoc}`);
          }
          aliasTargets.set(resolvedLoc, parsed.targetName);
        } else {
          const previous = aliasTargets.get(resolvedLoc);
          if (previous !== undefined && previous !== packageName(resolvedLoc)) {
            throw new Error(`Conflicting alias target identity at ${resolvedLoc}`);
          }
          nonAliasLocations.add(resolvedLoc);
        }
      }
    }
  }
  return aliasTargets;
}

function identity(location, value, allowMissing, aliasTargets) {
  const record = object(value, location),
    slotName = packageName(location),
    aliasTarget = aliasTargets?.get(location),
    effectiveName = aliasTarget ?? slotName;
  exactVersion(record.version);
  if (
    record.link ||
    record.inBundle ||
    (aliasTarget !== undefined
      ? record.name !== aliasTarget
      : record.name !== undefined && record.name !== slotName)
  ) {
    throw new Error(`Unsupported canonical registry identity: ${location}`);
  }
  if (
    record.resolved !== undefined &&
    record.resolved !== registryUrl(effectiveName, record.version)
  ) {
    throw new Error(`Canonical resolved identity conflict: ${location}`);
  }
  if (record.integrity !== undefined && !validIntegrity(record.integrity)) {
    throw new Error(`Invalid canonical integrity: ${location}`);
  }
  if (!allowMissing && (record.resolved === undefined || record.integrity === undefined)) {
    throw new Error(`Incomplete canonical registry identity: ${location}`);
  }
  return { record, name: effectiveName };
}

function packages(value) {
  const lock = object(value, 'canonical package lock');
  if (lock.lockfileVersion !== 3) {
    throw new Error('Canonical package lock must use lockfileVersion 3');
  }
  const entries = object(lock.packages, 'canonical lock packages');
  object(entries[''], 'canonical lock root');
  return entries;
}

/** Never reselect versions. Fetch dist only where URL/SRI is absent. */
export async function hydrateLockIdentities(value, fetchDist) {
  const lock = structuredClone(value),
    entries = packages(lock),
    aliasTargets = collectAliasTargets(entries),
    requests = new Map();
  for (const [location, value] of Object.entries(entries)) {
    if (!location) {
      continue;
    }
    const { record, name } = identity(location, value, true, aliasTargets);
    if (record.resolved !== undefined && record.integrity !== undefined) {
      continue;
    }
    const key = `${name}@${record.version}`;
    let dist = requests.get(key);
    if (!dist) {
      dist = object(await fetchDist(name, record.version), `registry identity ${key}`);
      if (dist.tarball !== registryUrl(name, record.version) || !validIntegrity(dist.integrity)) {
        throw new Error(`Unsupported registry identity: ${key}`);
      }
      requests.set(key, dist);
    }
    if (record.integrity !== undefined && record.integrity !== dist.integrity) {
      throw new Error(`Canonical integrity identity conflict: ${location}`);
    }
    record.resolved = dist.tarball;
    record.integrity = dist.integrity;
  }
  assertCanonicalLock(lock);
  return lock;
}

export function assertCanonicalLock(value) {
  const entries = packages(value),
    aliasTargets = collectAliasTargets(entries);
  for (const [location, entry] of Object.entries(entries)) {
    if (location) {
      identity(location, entry, false, aliasTargets);
    }
    assertRegistryDependencies(entry);
  }
  const encoded = JSON.stringify(value);
  if (/127\.0\.0\.1|localhost|file:|workspace:|\/workspace\/|\.artifacts\//i.test(encoded)) {
    throw new Error('Source/local endpoint remains in canonical lock');
  }
}

/** The sole version replacement is the template's accepted env dependency. */
export function prepareTemplate(manifestInput, lockInput, sourceVersion, version) {
  const manifest = structuredClone(object(manifestInput, 'template manifest'));
  assertRegistryDependencies(manifest);
  if (manifest.workspaces !== undefined) {
    throw new Error('Default npm templates must be standalone packages');
  }
  const deps = object(manifest.devDependencies, 'template devDependencies');
  if (deps['@docouno/env'] !== sourceVersion) {
    throw new Error('Template must accept the exact source env version');
  }
  for (const field of dependencyFields.filter((name) => name !== 'devDependencies')) {
    if (Object.hasOwn(object(manifest[field] ?? {}, field), '@docouno/env')) {
      throw new Error(`Unexpected template env dependency in ${field}`);
    }
  }
  deps['@docouno/env'] = version;
  const lock = structuredClone(lockInput),
    entries = packages(lock);
  for (const field of dependencyFields) {
    const sorted = (value) =>
      JSON.stringify(Object.entries(value ?? {}).sort(([a], [b]) => (a < b ? -1 : 1)));
    const expected =
      field === 'devDependencies'
        ? { ...manifest[field], '@docouno/env': sourceVersion }
        : manifest[field];
    if (sorted(entries[''][field]) !== sorted(expected)) {
      throw new Error(`Template manifest and source lock disagree: ${field}`);
    }
  }
  if (
    object(entries[envLocation], 'template env lock').version !== sourceVersion ||
    object(entries[''].devDependencies, 'template lock devDependencies')['@docouno/env'] !==
      sourceVersion
  ) {
    throw new Error('Template accepted env lock version differs from source');
  }
  // Existing env SRI describes the previous bytes, even during same-version rebuilds.
  delete entries[envLocation];
  entries[''].devDependencies['@docouno/env'] = version;
  return { manifest, lock };
}

/** Rewrite only the exact fixture origin; everything else must already be public. */
export function canonicalizeLock(value, registry) {
  const lock = structuredClone(value);
  for (const [location, entry] of Object.entries(packages(lock))) {
    if (location && typeof entry.resolved === 'string' && entry.resolved.startsWith(registry)) {
      entry.resolved = `https://registry.npmjs.org/${entry.resolved.slice(registry.length)}`;
    }
  }
  assertCanonicalLock(lock);
  return lock;
}

export function assertSelectedIdentities(previous, next, version, integrity) {
  const before = packages(previous),
    after = packages(next);
  for (const [location, record] of Object.entries(before)) {
    if (!location || location === envLocation) {
      continue;
    }
    const actual = after[location];
    if (
      !actual ||
      ['name', 'version', 'resolved', 'integrity'].some((field) => actual[field] !== record[field])
    ) {
      throw new Error(`npm changed a selected external identity: ${location}`);
    }
  }
  const env = object(after[envLocation], 'normalized env lock');
  if (
    env.version !== version ||
    env.integrity !== integrity ||
    env.resolved !== registryUrl('@docouno/env', version)
  ) {
    throw new Error('Normalized env identity differs from the packed archive');
  }
  if (
    object(after[''].devDependencies, 'normalized root devDependencies')['@docouno/env'] !== version
  ) {
    throw new Error('Normalized template accepted env version differs');
  }
}
