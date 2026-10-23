import { fail, slug } from './files.ts';
import type {
  ComponentPath,
  ComponentRole,
  ComponentState,
  ProjectState,
  ToolingProfile,
} from './types.ts';

export const templates = {
  'node-api': { role: 'app', toolchain: 'node', toolingProfile: 'node' },
  'rust-api': { role: 'app', toolchain: 'rust', toolingProfile: 'rust' },
  'react-web': { role: 'app', toolchain: 'node', toolingProfile: 'web' },
  'electron-react': {
    role: 'app',
    toolchain: 'node',
    toolingProfile: 'electron',
    legacy: 'electron-react',
  },
  'react-client': {
    role: 'library',
    toolchain: 'node',
    toolingProfile: 'react-library',
    legacy: 'react-client',
  },
} as const;
export type TemplateId = keyof typeof templates;
export const profiles: Record<
  ToolingProfile,
  { toolchain: 'node' | 'rust'; roles: ComponentRole[]; template: TemplateId }
> = {
  node: { toolchain: 'node', roles: ['app'], template: 'node-api' },
  web: { toolchain: 'node', roles: ['app'], template: 'react-web' },
  electron: { toolchain: 'node', roles: ['app'], template: 'electron-react' },
  'react-library': { toolchain: 'node', roles: ['library'], template: 'react-client' },
  rust: { toolchain: 'rust', roles: ['app', 'library'], template: 'rust-api' },
};
export function templateId(value: string): TemplateId {
  const aliases: Record<string, TemplateId> = {
    node: 'node-api',
    react: 'react-web',
    rust: 'rust-api',
    'node-app': 'node-api',
    'react-app': 'react-web',
    'rust-app': 'rust-api',
  };
  const id = aliases[value] ?? value;
  if (!Object.hasOwn(templates, id)) {
    fail(`Unknown component template: ${value}`);
  }
  return id as TemplateId;
}
export function componentPath(role: ComponentRole, name: string): ComponentPath {
  return `${role === 'app' ? 'apps' : 'packages'}/${slug(name)}`;
}
export function validComponentPath(value: string): ComponentPath {
  const match = /^(apps|packages)\/([^/]+)$/.exec(value);
  if (!match) {
    fail(`Invalid component path: ${value}`);
  }
  slug(match[2]!);
  return value as ComponentPath;
}
export function nodeComponents(state: ProjectState): [ComponentPath, ComponentState][] {
  return (Object.entries(state.components) as [ComponentPath, ComponentState][])
    .filter(([, c]) => c.toolchain === 'node')
    .sort(([a], [b]) => a.localeCompare(b));
}
export function profileOutputs(profile: ToolingProfile): readonly string[] {
  return profile === 'rust' ? ['target'] : profile === 'electron' ? ['dist', 'release'] : ['dist'];
}
