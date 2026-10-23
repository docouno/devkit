import { existsSync, mkdirSync, readFileSync } from 'node:fs';
import { dirname, join, relative } from 'node:path';
import { isDeepStrictEqual } from 'node:util';
import { parseDocument } from 'yaml';
import {
  digest,
  fail,
  identity,
  inputPath,
  object,
  text,
  writeAtomic,
  writeJson,
} from './files.ts';
import type { ComposeInventory, InputIdentitySet } from './types.ts';

const entrypoints = ['.devkit/compose.apps.yaml', 'compose.yaml'];
export function captureComposeInputs(root: string, snapshot?: string): ComposeInventory {
  const files: InputIdentitySet['files'] = {},
    queue: [string, boolean][] = entrypoints.map((name) => [name, true]);
  while (queue.length) {
    const [name, optional] = queue.shift()!,
      path = inputPath(root, name),
      key = relative(root, path);
    if (Object.hasOwn(files, key)) {
      continue;
    }
    const value = identity(path);
    files[key] = value;
    if (!value) {
      if (!optional) {
        fail(`Missing Compose input: ${key}`);
      }
      continue;
    }
    const bytes = readFileSync(path),
      document = parseDocument(bytes.toString(), { merge: true, uniqueKeys: true, strict: true });
    if (document.errors.length || document.warnings.length) {
      fail(`Unsupported or invalid Compose YAML: ${key}`);
    }
    const data = object(document.toJS({ maxAliasCount: 100 }), `Compose ${key}`);
    if (snapshot) {
      writeAtomic(inputPath(snapshot, key), bytes, value.mode);
    }
    const add = (value: unknown): void => {
      const ref = text(value, 'Compose local reference');
      if (/\$|:\/\/|^\//.test(ref)) {
        fail(`Unsupported Compose reference in ${key}`);
      }
      queue.push([relative(root, inputPath(root, join(dirname(key), ref))), false]);
    };
    if (data.include !== undefined) {
      if (!Array.isArray(data.include)) {
        fail(`Unsupported Compose include in ${key}`);
      }
      for (const value of data.include) {
        add(value);
      }
    }
    for (const service of Object.values(object(data.services ?? {}, `services ${key}`))) {
      const item = object(service, 'Compose service');
      if (item.extends !== undefined) {
        const extension = object(item.extends, 'Compose extends');
        if (extension.file !== undefined) {
          add(extension.file);
        }
      }
    }
  }
  const ordered = Object.fromEntries(Object.entries(files).sort(([a], [b]) => a.localeCompare(b)));
  const inputs: InputIdentitySet = {
    schemaVersion: 1,
    files: ordered,
    directories: [],
    digest: digest(JSON.stringify(ordered)),
  };
  return {
    schemaVersion: 1,
    protocol: 'compose-local-v1',
    entrypoints: entrypoints.filter((name) => files[name] !== null),
    inputs,
    services: [],
  };
}
export function assertComposeInventory(root: string, inventory: ComposeInventory): void {
  if (
    inventory.schemaVersion !== 1 ||
    inventory.protocol !== 'compose-local-v1' ||
    !Array.isArray(inventory.services) ||
    inventory.services.some((name) => typeof name !== 'string')
  ) {
    fail('Invalid Compose inventory');
  }
  const current = captureComposeInputs(root);
  if (
    !isDeepStrictEqual(current.inputs, inventory.inputs) ||
    !isDeepStrictEqual(current.entrypoints, inventory.entrypoints)
  ) {
    fail('Compose admitted inputs changed');
  }
}
export function prepareComposeInventory(root: string, directory: string): ComposeInventory {
  const target = inputPath(root, directory);
  mkdirSync(target, { recursive: true });
  const snapshot = join(target, 'snapshot');
  mkdirSync(snapshot);
  const inventory = captureComposeInputs(root, snapshot);
  assertComposeInventory(root, inventory);
  writeJson(join(target, 'inventory.json'), inventory);
  return inventory;
}
export function loadComposeInventory(root: string, file: string): ComposeInventory {
  const path = inputPath(root, file);
  if (!relative(root, path).startsWith('.devkit/state/')) {
    fail('Compose inventory must be private operation state');
  }
  const inventory = JSON.parse(readFileSync(path, 'utf8')) as ComposeInventory;
  const model = `${path}.model.json`;
  if (inventory.entrypoints.length && !existsSync(model)) {
    fail('Compose inventory is missing native query output');
  }
  inventory.services = existsSync(model)
    ? Object.keys(
        object(
          object(JSON.parse(readFileSync(model, 'utf8')), 'Compose model').services ?? {},
          'Compose services',
        ),
      )
    : [];
  assertComposeInventory(root, inventory);
  return inventory;
}
