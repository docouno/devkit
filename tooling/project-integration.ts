import { basename } from 'node:path';
import { nodeComponents, profileOutputs } from './catalog.ts';
import { fail } from './files.ts';
import type { ComponentPath, ProjectState } from './types.ts';

function target(command: string, path: ComponentPath, role: 'app' | 'library'): string {
  return role === 'app' ? command + '-' + basename(path) : command + '/' + path;
}

export function renderProjectMake(state: ProjectState): string {
  const entries = Object.entries(state.components) as [
    ComponentPath,
    ProjectState['components'][ComponentPath],
  ][];
  entries.sort(([a], [b]) => a.localeCompare(b));
  const names = (predicate: (component: ProjectState['components'][ComponentPath]) => boolean) =>
    entries
      .filter(([, component]) => predicate(component))
      .map(([path]) => basename(path))
      .join(' ');
  let result = '# Managed component registration.\n';
  result += 'DEVKIT_REGISTERED_APPS := ' + names((c) => c.role === 'app') + '\n';
  result += 'DEVKIT_REGISTERED_PACKAGES := ' + names((c) => c.role === 'library') + '\n';
  result +=
    'DEVKIT_DESKTOP_APPS := ' +
    names((c) => c.role === 'app' && c.toolingProfile === 'electron') +
    '\n';
  for (const [path, component] of entries) {
    if (component.toolchain === 'node') {
      result += target('setup', path, component.role) + ': setup-node-workspace\n';
    }
    for (const command of ['check', 'test', 'build', 'dev']) {
      if (component.role === 'library' && command === 'dev') {
        continue;
      }
      const dependencies = component.dependencies.flatMap((dependency) =>
        command === 'check'
          ? ['check/' + dependency, 'build/' + dependency]
          : ['build/' + dependency],
      );
      if (dependencies.length) {
        result += target(command, path, component.role) + ': ' + dependencies.join(' ') + '\n';
      }
    }
    if (component.toolingProfile === 'electron') {
      result +=
        target('package', path, component.role) +
        ': ' +
        target('build', path, component.role) +
        '\n';
    }
  }
  return result;
}

export function renderAppCompose(state: ProjectState): string {
  const services: Record<string, unknown> = {};
  const nodeVolumes = [
    {
      type: 'bind',
      source: '${DEVKIT_PROJECT_ROOT:?}/node_modules',
      target: '/workspace/node_modules',
      read_only: false,
      bind: { create_host_path: false, selinux: 'z' },
    },
    ...nodeComponents(state).flatMap(([path, component]) =>
      ['node_modules', ...profileOutputs(component.toolingProfile)].map((directory) => ({
        type: 'bind',
        source: '${DEVKIT_PROJECT_ROOT:?}/' + path + '/' + directory,
        target: '/workspace/' + path + '/' + directory,
        read_only: false,
        bind: { create_host_path: false, selinux: 'z' },
      })),
    ),
  ];
  for (const [path, component] of Object.entries(state.components)) {
    if (component.role !== 'app' || component.development?.owner !== 'managed') {
      continue;
    }
    const rust = component.toolchain === 'rust';
    const desktop = component.toolingProfile === 'electron';
    const name = component.development.service ?? basename(path);
    const environment: Record<string, string> = {
      HOME: '/tmp/devkit-home',
      NPM_CONFIG_UPDATE_NOTIFIER: 'false',
    };
    const api = component.development.api;
    if (api !== undefined) {
      const server = state.components[api];
      if (!server || server.role !== 'app' || server.development?.kind !== 'compose') {
        fail('Invalid API connection: ' + api);
      }
      environment.DEVKIT_API_TARGET =
        'http://' + (server.development.service ?? basename(api)) + ':8080';
      if (component.toolingProfile === 'web') {
        environment.VITE_API_ENABLED = 'true';
      }
    }
    if (rust) {
      Object.assign(environment, {
        CARGO_HOME: '/cache/cargo',
        CARGO_TARGET_DIR: '/workspace/' + path + '/target',
        RUSTUP_AUTO_INSTALL: '0',
      });
    }
    const rootVolume = {
      type: 'bind',
      source: '${DEVKIT_PROJECT_ROOT:?}',
      target: '/workspace',
      read_only: true,
      bind: { create_host_path: false, selinux: 'z' },
    };
    const volumes = rust
      ? [
          rootVolume,
          {
            type: 'bind',
            source: '${DEVKIT_PROJECT_ROOT:?}/' + path + '/target',
            target: '/workspace/' + path + '/target',
            read_only: false,
            bind: { create_host_path: false, selinux: 'z' },
          },
          {
            type: 'bind',
            source: '${DEVKIT_PROJECT_ROOT:?}/.devkit/cache/cargo',
            target: '/cache/cargo',
            read_only: false,
            bind: { create_host_path: false, selinux: 'z' },
          },
        ]
      : [rootVolume, ...nodeVolumes];
    services[name] = {
      image: rust
        ? '${DEVKIT_RUST_IMAGE:-devkit-unprepared-rust}'
        : desktop
          ? '${DEVKIT_DESKTOP_IMAGE:-devkit-unprepared-desktop}'
          : '${DEVKIT_NODE_IMAGE:?}',
      init: true,
      user: '${DEVKIT_UID:?}:${DEVKIT_GID:?}',
      working_dir: '/workspace/' + path,
      command: rust
        ? ['cargo', 'run', '--locked', '--offline']
        : desktop
          ? ['bash', '/workspace/.devkit/runtime/desktop-dev.sh']
          : ['npm', 'run', 'dev'],
      environment,
      volumes,
      ...(desktop
        ? {
            read_only: true,
            tmpfs: ['/tmp:mode=1777'],
            shm_size: '256m',
            cap_drop: ['ALL'],
            security_opt: [
              'no-new-privileges:true',
              'seccomp=${DEVKIT_PROJECT_ROOT:?}/.devkit/runtime/desktop-seccomp.json',
            ],
          }
        : { ports: ['127.0.0.1::' + (component.toolingProfile === 'web' ? '5173' : '8080')] }),
    };
  }
  return JSON.stringify({ services }, null, 2) + '\n';
}
