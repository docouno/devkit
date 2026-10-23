import { createHash, randomUUID } from 'node:crypto';
import {
  chmodSync,
  copyFileSync,
  existsSync,
  lstatSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  readlinkSync,
  realpathSync,
  renameSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from 'node:fs';
import { dirname, join, relative, resolve, sep } from 'node:path';
import type { FileIdentity, Release } from './types.ts';

export function fail(message: string): never {
  throw new Error(message);
}

export function object(value: unknown, label: string): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    fail(`${label} must be an object`);
  }
  return value as Record<string, unknown>;
}

export function text(value: unknown, label: string): string {
  if (typeof value !== 'string' || value.length === 0) {
    fail(`${label} must be a nonempty string`);
  }
  return value;
}

export function version(value: unknown): string {
  const result = text(value, 'version');
  if (!/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[a-z0-9][a-z0-9.-]*)?$/.test(result)) {
    fail(`Invalid release version: ${result}`);
  }
  return result;
}

export function slug(value: string): string {
  if (value.length > 63 || !/^[a-z][a-z0-9]*(-[a-z0-9]+)*$/.test(value)) {
    fail(`Expected a lower-case name containing letters, digits and hyphens: ${value}`);
  }
  return value;
}

export function json(path: string): unknown {
  try {
    return JSON.parse(readFileSync(path, 'utf8')) as unknown;
  } catch (error) {
    fail(`Cannot read JSON ${path}: ${error instanceof Error ? error.message : String(error)}`);
  }
}

export function digest(bytes: string | Buffer): string {
  return createHash('sha256').update(bytes).digest('hex');
}

export function relativePath(value: unknown): string {
  const result = text(value, 'relative path');
  const parts = result.split('/');
  if (
    !/^[A-Za-z0-9._/-]+$/.test(result) ||
    parts.some((part) => ['', '.', '..', '__proto__', 'prototype', 'constructor'].includes(part))
  ) {
    fail(`Unsafe release path: ${result}`);
  }
  return result;
}

export function safePath(root: string, name: string): string {
  const parts = relativePath(name).split('/');
  let current = root;
  for (const [index, part] of parts.entries()) {
    current = join(current, part);
    if (existsSync(current) || isLink(current)) {
      const info = lstatSync(current);
      if (info.isSymbolicLink()) {
        fail(`Refusing a symlink in managed path: ${current}`);
      }
      if (index < parts.length - 1 && !info.isDirectory()) {
        fail(`Managed path parent is not a directory: ${current}`);
      }
    }
  }
  return current;
}

/** Native input paths may contain spaces; they still cannot leave or redirect the checkout. */
export function inputPath(root: string, name: string): string {
  if (!name || name.includes('\0')) {
    fail(`Invalid native input path: ${name}`);
  }
  const target = resolve(root, name);
  const suffix = relative(root, target);
  if (
    !suffix ||
    suffix.startsWith(`..${sep}`) ||
    suffix === '..' ||
    resolve(target) === resolve(root)
  ) {
    fail(`Native input escapes the project: ${name}`);
  }
  let current = root;
  const parts = suffix.split(sep);
  for (const [index, part] of parts.entries()) {
    current = join(current, part);
    if (existsSync(current) || isLink(current)) {
      const info = lstatSync(current);
      if (info.isSymbolicLink()) {
        fail(`Symlink native input: ${name}`);
      }
      if (index < parts.length - 1 && !info.isDirectory()) {
        fail(`Non-directory native input ancestor: ${name}`);
      }
    }
  }
  return target;
}

function isLink(path: string): boolean {
  try {
    return lstatSync(path).isSymbolicLink();
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === 'ENOENT') {
      return false;
    }
    throw error;
  }
}

export function identity(path: string): FileIdentity | null {
  if (!existsSync(path) && !isLink(path)) {
    return null;
  }
  const info = lstatSync(path);
  if (!info.isFile() || info.isSymbolicLink()) {
    fail(`Expected a regular file: ${path}`);
  }
  return { sha256: digest(readFileSync(path)), mode: info.mode & 0o777 };
}

export function same(a: FileIdentity | null, b: FileIdentity | null): boolean {
  return a === null || b === null ? a === b : a.sha256 === b.sha256 && a.mode === b.mode;
}

export function writeAtomic(path: string, bytes: string | Buffer, mode = 0o644): void {
  mkdirSync(dirname(path), { recursive: true });
  const temporary = join(dirname(path), `.devkit-${randomUUID()}.tmp`);
  try {
    writeFileSync(temporary, bytes, { mode });
    chmodSync(temporary, mode);
    renameSync(temporary, path);
  } finally {
    rmSync(temporary, { force: true });
  }
}

export function writeJson(path: string, value: unknown): void {
  writeAtomic(path, `${JSON.stringify(value, null, 2)}\n`);
}

export function fileMap(root: string): Record<string, FileIdentity> {
  const result: Record<string, FileIdentity> = Object.create(null) as Record<string, FileIdentity>;
  function visit(directory: string): void {
    for (const entry of readdirSync(directory, { withFileTypes: true }).sort((a, b) =>
      a.name.localeCompare(b.name),
    )) {
      const path = join(directory, entry.name);
      const name = relative(root, path).split(sep).join('/');
      relativePath(name);
      if (entry.isSymbolicLink()) {
        fail(`Release payload contains a symlink: ${name}`);
      }
      if (entry.isDirectory()) {
        visit(path);
      } else {
        const value = identity(path);
        if (value === null) {
          fail(`Release file disappeared: ${name}`);
        }
        result[name] = value;
      }
    }
  }
  visit(root);
  return result;
}

export function copyTree(
  source: string,
  target: string,
  options: { ignore?: (path: string) => boolean; preserveLinks?: boolean } = {},
): void {
  const sourceRoot = realpathSync(source);
  function copy(path: string, destination: string): void {
    const name = relative(sourceRoot, path).split(sep).join('/');
    if (name && options.ignore?.(name)) {
      return;
    }
    const info = lstatSync(path);
    if (info.isSymbolicLink()) {
      if (!options.preserveLinks) {
        fail(`Refusing payload symlink: ${path}`);
      }
      mkdirSync(dirname(destination), { recursive: true });
      symlinkSync(readlinkSync(path), destination);
    } else if (info.isDirectory()) {
      mkdirSync(destination, { recursive: true });
      for (const entry of readdirSync(path).sort()) {
        copy(join(path, entry), join(destination, entry));
      }
    } else if (info.isFile()) {
      mkdirSync(dirname(destination), { recursive: true });
      copyFileSync(path, destination);
      chmodSync(destination, info.mode & 0o777);
    } else {
      fail(`Unsupported input file type: ${path}`);
    }
  }
  copy(sourceRoot, resolve(target));
}

export function readRelease(root: string): Release {
  const raw = object(json(join(root, 'release.json')), 'release manifest');
  if (raw.schemaVersion !== 1) {
    fail('Unsupported release schemaVersion');
  }
  const parsedVersion = version(raw.version);
  const environment = object(raw.environment, 'release environment');
  const files = object(raw.files, 'release files');
  const result: Release = {
    schemaVersion: 1,
    version: parsedVersion,
    rustVersion: version(raw.rustVersion),
    environment: {
      name: text(environment.name, 'environment package name'),
      version: version(environment.version),
      archive: relativePath(environment.archive),
      integrity: text(environment.integrity, 'package integrity'),
    },
    files: Object.create(null) as Record<string, FileIdentity>,
  };
  if (result.environment.name !== '@docouno/env' || result.environment.version !== parsedVersion) {
    fail('Environment package identity does not match this release');
  }
  if (!/^sha512-[A-Za-z0-9+/]+={0,2}$/.test(result.environment.integrity)) {
    fail('Invalid environment package integrity');
  }
  for (const [name, value] of Object.entries(files)) {
    const safeName = relativePath(name);
    if (safeName === 'release.json') {
      fail('Release manifest cannot hash itself');
    }
    const expected = object(value, `identity of ${name}`);
    const hash = text(expected.sha256, `sha256 of ${name}`);
    const mode = expected.mode;
    if (
      !/^[a-f0-9]{64}$/.test(hash) ||
      typeof mode !== 'number' ||
      ![0o644, 0o755].includes(mode)
    ) {
      fail(`Invalid file identity: ${name}`);
    }
    const accepted = { sha256: hash, mode: Number(mode) };
    if (!same(identity(safePath(root, safeName)), accepted)) {
      fail(`Release integrity mismatch: ${name}`);
    }
    result.files[safeName] = accepted;
  }
  const actual = fileMap(root);
  delete actual['release.json'];
  const actualNames = Object.keys(actual).sort();
  const expectedNames = Object.keys(result.files).sort();
  if (JSON.stringify(actualNames) !== JSON.stringify(expectedNames)) {
    fail('Release payload file set differs from its manifest');
  }
  if (!Object.hasOwn(result.files, result.environment.archive)) {
    fail('Environment archive is not in the release');
  }
  const archive = readFileSync(safePath(root, result.environment.archive));
  const integrity = `sha512-${createHash('sha512').update(archive).digest('base64')}`;
  if (integrity !== result.environment.integrity) {
    fail('Environment archive integrity mismatch');
  }
  return result;
}
