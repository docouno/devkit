import { nodeComponents, profileOutputs } from './catalog.ts';
import { fail } from './files.ts';
import type { InputIdentitySet, ProjectState } from './types.ts';

export function workspaceWriteDirectories(state: ProjectState, inputs: InputIdentitySet): string[] {
  if (nodeComponents(state).length === 0) {
    return [];
  }
  const paths = new Set<string>();
  for (const directory of inputs.directories) {
    paths.add(directory ? `${directory}/node_modules` : 'node_modules');
  }
  for (const [path, component] of nodeComponents(state)) {
    for (const output of profileOutputs(component.toolingProfile)) {
      paths.add(`${path}/${output}`);
    }
  }
  for (const path of paths) {
    if (
      /[\r\n\t\0]/u.test(path) ||
      path.startsWith('/') ||
      path.split('/').some((part) => !part || part === '.' || part === '..')
    ) {
      fail(`Invalid workspace output path: ${JSON.stringify(path)}`);
    }
  }
  return [...paths].sort();
}

/** A managed, portable Compose overlay. All source/manifests remain read-only. */
export function renderWorkspaceCompose(state: ProjectState, inputs: InputIdentitySet): string {
  const volumes = workspaceWriteDirectories(state, inputs).map((path) => ({
    type: 'bind',
    source: '${DEVKIT_PROJECT_ROOT:?}/' + path,
    target: '/workspace/' + path,
    read_only: false,
    bind: { create_host_path: false, selinux: 'z' },
  }));
  return JSON.stringify({ services: { node: { volumes }, desktop: { volumes } } }, null, 2) + '\n';
}
