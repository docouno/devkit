import { createHash } from 'node:crypto';
import { lstatSync, readFileSync, readdirSync } from 'node:fs';
import { basename, join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { nodeComponents, profileOutputs } from './catalog.ts';
import { fail, object, safePath, text } from './files.ts';
import { assertNativeComponents, collectNativeInputClosure } from './native-workspace.ts';
import { assertManaged, assertNoPending, projectRoot, projectState } from './project-state.ts';
import { renderProjectMake, renderAppCompose } from './project-integration.ts';
import { renderWorkspaceCompose, workspaceWriteDirectories } from './workspace-compose.ts';
import type { ComponentPath, ProjectState } from './types.ts';

const hash = (bytes: string | Buffer): string => createHash('sha256').update(bytes).digest('hex');
const checksumPath = '.devkit/runtime.sha256';

function verifyManagedSet(root: string, state: ProjectState): void {
  const required = [
    checksumPath,
    '.devkit/tooling-profiles.json',
    '.devkit/project.mk',
    '.devkit/compose.apps.yaml',
    '.devkit/compose.workspace.json',
    ...[
      'devkit.mk',
      'devkit.sh',
      'guard.sh',
      'compose.tools.yaml',
      'toolchains.env',
      'project.mjs',
    ].map((name) => `.devkit/runtime/${name}`),
  ];
  for (const name of required) {
    if (!Object.hasOwn(state.managed, name)) {
      fail(`Missing managed runtime identity: ${name}`);
    }
  }
  function visit(path: string, prefix: string): void {
    for (const name of readdirSync(path).sort()) {
      const relative = `${prefix}/${name}`;
      const child = join(path, name);
      const info = lstatSync(child);
      if (info.isDirectory() && !info.isSymbolicLink()) {
        visit(child, relative);
      } else if (
        !info.isFile() ||
        info.isSymbolicLink() ||
        !Object.hasOwn(state.managed, relative)
      ) {
        fail(`Unlisted or nonregular runtime file: ${relative}`);
      }
    }
  }
  visit(safePath(root, '.devkit/runtime'), '.devkit/runtime');
  const seen = new Set<string>();
  for (const line of readFileSync(safePath(root, checksumPath), 'utf8').split('\n')) {
    if (!line) {
      continue;
    }
    const record = /^([a-f0-9]{64}) {2}([A-Za-z0-9._/-]+)$/u.exec(line);
    const name = record?.[2];
    if (
      !record ||
      !name ||
      name === checksumPath ||
      seen.has(name) ||
      state.managed[name]?.sha256 !== record[1]
    ) {
      fail('Checksum table differs from the accepted manifest');
    }
    seen.add(name);
  }
  if (Object.keys(state.managed).some((name) => name !== checksumPath && !seen.has(name))) {
    fail('Checksum table omits a managed file');
  }
}

export function readRuntimeProject(rootInput: string, expectedVersion: string): ProjectState {
  const root = projectRoot(rootInput);
  assertNoPending(root);
  const state = projectState(root);
  if (state.version !== expectedVersion) {
    fail('Manifest version differs from accepted toolchains.env');
  }
  assertManaged(root, state);
  verifyManagedSet(root, state);
  assertNativeComponents(root, state);
  const inputs = collectNativeInputClosure(root, state);
  if (readFileSync(safePath(root, '.devkit/project.mk'), 'utf8') !== renderProjectMake(state)) {
    fail('Component Make registration is stale; run the accepted devkit update');
  }
  if (
    readFileSync(safePath(root, '.devkit/compose.apps.yaml'), 'utf8') !== renderAppCompose(state)
  ) {
    fail('Development integration is stale; run the accepted devkit update');
  }
  if (
    readFileSync(safePath(root, '.devkit/compose.workspace.json'), 'utf8') !==
    renderWorkspaceCompose(state, inputs)
  ) {
    fail('Workspace mounts are stale; run the accepted devkit update');
  }
  return state;
}

export function nodeWorkspaceFingerprint(root: string, state: ProjectState): string {
  const nodeState = { ...state, components: Object.fromEntries(nodeComponents(state)) };
  const inputs = collectNativeInputClosure(root, nodeState);
  return hash(state.releaseIdentity + '\n' + inputs.digest);
}

export function describeRuntime(rootInput: string, state: ProjectState): string {
  const root = projectRoot(rootInput);
  const inputs = collectNativeInputClosure(root, state);
  const directories = new Set(workspaceWriteDirectories(state, inputs));
  const lines = [`node-fingerprint\t${nodeWorkspaceFingerprint(root, state)}`];
  for (const [id, component] of Object.entries(state.components).sort(([a], [b]) =>
    a.localeCompare(b),
  )) {
    const fields = [
      'component',
      id,
      component.toolchain,
      component.toolingProfile,
      component.role,
      component.development?.kind ?? '-',
      component.development?.service ?? '-',
      component.development?.api ?? '-',
    ];
    if (fields.some((field) => /[\r\n\t\0]/u.test(field))) {
      fail('Invalid runtime component metadata');
    }
    lines.push(fields.join('\t'));
    for (const output of profileOutputs(component.toolingProfile)) {
      directories.add(`${id}/${output}`);
    }
  }
  for (const directory of [...directories].sort()) {
    if (/[\r\n\t\0]/u.test(directory)) {
      fail('Runtime directory contains a control character');
    }
    lines.push(`directory\t${directory}`);
  }
  return lines.join('\n') + '\n';
}

/** Consume Compose's normalized model, which includes implicit service dependencies. */
export function developmentComponents(
  state: ProjectState,
  modelInput: unknown,
  selectedService: string,
): ComponentPath[] {
  const services = object(object(modelInput, 'Compose model').services, 'Compose services');
  const selected = object(services[selectedService], 'selected Compose service');
  const profilesOf = (service: Record<string, unknown>): string[] => {
    if (service.profiles === undefined) {
      return [];
    }
    if (!Array.isArray(service.profiles)) {
      fail('Invalid normalized Compose profiles');
    }
    return service.profiles.map((profile) => text(profile, 'Compose profile'));
  };
  // Explicit selection enables the selected service's profiles; ambient profiles
  // are cleared by the launcher. Other profiled services remain inactive.
  const activeProfiles = new Set(profilesOf(selected));
  const seen = new Set<string>();
  const visit = (name: string, required: boolean): void => {
    if (seen.has(name)) {
      return;
    }
    const value = services[name];
    if (value === undefined && !required) {
      return;
    }
    const service = object(value, `Compose service ${name}`);
    const profiles = profilesOf(service);
    if (
      name !== selectedService &&
      profiles.length &&
      !profiles.some((p) => activeProfiles.has(p))
    ) {
      if (!required) {
        return;
      }
      fail(`Compose dependency is not active: ${name}`);
    }
    seen.add(name);
    for (const [dependency, settings] of Object.entries(
      object(service.depends_on ?? {}, 'Compose dependencies'),
    )) {
      visit(dependency, object(settings, 'Compose dependency').required !== false);
    }
  };
  visit(selectedService, true);
  return Object.entries(state.components)
    .filter(
      ([path, component]) =>
        component.role === 'app' && seen.has(component.development?.service ?? basename(path)),
    )
    .map(([path]) => path as ComponentPath)
    .sort();
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    if (
      process.env.DEVKIT_NODE_VERSION &&
      process.versions.node !== process.env.DEVKIT_NODE_VERSION
    ) {
      fail(`Unexpected control Node version: ${process.versions.node}`);
    }
    const [command, root, expected, modelPath, selectedService] = process.argv.slice(2);
    if (
      !root ||
      !expected ||
      !['verify', 'describe', 'fingerprint', 'development'].includes(command ?? '')
    ) {
      fail('Expected project command, root and accepted version');
    }
    const state = readRuntimeProject(root, expected);
    if (command === 'describe') {
      process.stdout.write(describeRuntime(root, state));
    }
    if (command === 'fingerprint') {
      process.stdout.write(nodeWorkspaceFingerprint(root, state) + '\n');
    }
    if (command === 'development') {
      if (!modelPath || !selectedService) {
        fail('Expected Compose model path and selected service');
      }
      let model: unknown;
      try {
        model = JSON.parse(readFileSync(safePath(root, modelPath), 'utf8'));
      } catch {
        fail('Invalid normalized Compose model');
      }
      process.stdout.write(developmentComponents(state, model, selectedService).join('\n') + '\n');
    }
  } catch (error) {
    process.stderr.write(`devkit: ${error instanceof Error ? error.message : String(error)}\n`);
    process.exitCode = 1;
  }
}
