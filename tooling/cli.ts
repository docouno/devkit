import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { fail, slug } from './files.ts';
import { initProject } from './project.ts';
import type { ComponentSpec, Template } from './project.ts';
import {
  addProjectComponent,
  recoverProject,
  registerProjectComponent,
  updateProject,
} from './project-update.ts';
import { loadComposeInventory, prepareComposeInventory } from './compose-inputs.ts';
import { profiles } from './catalog.ts';
import type { PackageMode, ToolingProfile } from './types.ts';

const help = `Usage:
  devkit init TARGET --app NAME=TEMPLATE [--app NAME=TEMPLATE ...]
  devkit init TARGET --preset fullstack-desktop
  devkit add-app TARGET --name NAME --template TEMPLATE [--use CLIENT]
  devkit add-package TARGET --name NAME --template react-client
  devkit register TARGET --app NAME|--package NAME --profile PROFILE
  devkit update TARGET [--package-source local|registry]
  devkit recover TARGET

Templates: node-api, rust-api, react-web, electron-react, react-client.
Legacy init --template node|react|rust|mixed remains accepted.
`;
export async function main(
  args: readonly string[],
  releaseRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..'),
): Promise<void> {
  if (
    process.env.DEVKIT_EXPECTED_NODE_VERSION !== undefined &&
    process.versions.node !== process.env.DEVKIT_EXPECTED_NODE_VERSION
  ) {
    fail(`Unexpected factory Node version: ${process.versions.node}`);
  }
  if (!args.length || ['help', '--help', '-h'].includes(args[0]!)) {
    process.stdout.write(help);
    return;
  }
  const [command, target, ...rest] = args;
  if (
    ![
      'init',
      'add-app',
      'add-package',
      'register',
      'update',
      'recover',
      'prepare-compose',
    ].includes(command!)
  ) {
    fail(`Unknown command: ${command}`);
  }
  if (!target || target.startsWith('--')) {
    fail('Expected a target project path');
  }
  const allowed: Record<string, string[]> = {
    init: ['--app', '--preset', '--template', '--name', '--package-source'],
    'add-app': ['--name', '--template', '--use', '--compose-inventory'],
    'add-package': ['--name', '--template', '--use'],
    register: ['--app', '--package', '--profile', '--template'],
    update: ['--package-source'],
    recover: [],
    'prepare-compose': ['--output'],
  };
  const values: Record<string, string[]> = {};
  for (let index = 0; index < rest.length; index += 2) {
    const key = rest[index]!,
      value = rest[index + 1];
    if (
      !allowed[command!]!.includes(key) ||
      (values[key] && !(key === '--app' && command === 'init'))
    ) {
      fail(`Unknown or repeated option: ${key}`);
    }
    if (!value || value.startsWith('--')) {
      fail(`Missing value for ${key}`);
    }
    (values[key] ??= []).push(value);
  }
  const get = (name: string) => values[name]?.[0];
  const packageMode = get('--package-source');
  if (packageMode !== undefined && !['local', 'registry'].includes(packageMode)) {
    fail('Invalid package source');
  }
  if (command === 'prepare-compose') {
    const output = get('--output');
    if (!output) {
      fail('Missing inventory output');
    }
    prepareComposeInventory(target, output);
    return;
  }
  if (command === 'init') {
    const apps = values['--app']?.map((value) => {
      const index = value.indexOf('=');
      if (index <= 0) {
        fail('Expected --app NAME=TEMPLATE');
      }
      return { name: slug(value.slice(0, index)), template: value.slice(index + 1) };
    });
    const state = await initProject(releaseRoot, target, {
      packageMode: (packageMode ?? 'registry') as PackageMode,
      ...(apps ? { apps } : {}),
      ...(get('--name') ? { name: slug(get('--name')!) } : {}),
      ...(get('--template') ? { template: get('--template') as Template } : {}),
      ...(get('--preset') ? { preset: get('--preset')! } : {}),
    });
    process.stdout.write(`Created a project with devkit ${state.version}. Run make setup.\n`);
    return;
  }
  if (command === 'recover') {
    process.stdout.write(
      recoverProject(target) ? 'Rolled back the pending operation.\n' : 'No pending update.\n',
    );
    return;
  }
  if (command === 'update') {
    const state = await updateProject(
      releaseRoot,
      target,
      packageMode ? { packageMode: packageMode as PackageMode } : {},
    );
    process.stdout.write(
      `Accepted devkit ${state.version}. Review the diff, then run make setup.\n`,
    );
    return;
  }
  if (command === 'register') {
    if (Boolean(get('--app')) === Boolean(get('--package'))) {
      fail('Register requires exactly one --app or --package');
    }
    const legacy: Record<string, ToolingProfile> = { node: 'node', react: 'web', rust: 'rust' };
    const profile = get('--profile') ?? legacy[get('--template') ?? ''];
    if (!profile || !Object.hasOwn(profiles, profile)) {
      fail('Register requires a supported --profile');
    }
    await registerProjectComponent(
      releaseRoot,
      target,
      get('--app') ? 'app' : 'library',
      slug((get('--app') ?? get('--package'))!),
      profile as ToolingProfile,
    );
  } else {
    const name = get('--name'),
      template = get('--template');
    if (!name || !template) {
      fail('Add requires --name and --template');
    }
    const spec: ComponentSpec = {
      name: slug(name),
      template,
      ...(get('--use') ? { uses: [slug(get('--use')!)] } : {}),
    };
    await addProjectComponent(
      releaseRoot,
      target,
      command === 'add-app' ? 'app' : 'library',
      spec,
      get('--compose-inventory')
        ? { composeInventory: loadComposeInventory(target, get('--compose-inventory')!) }
        : {},
    );
  }
  process.stdout.write('Component registered. Review the diff, then run make setup.\n');
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main(process.argv.slice(2)).catch((error: unknown) => {
    process.stderr.write(`devkit: ${error instanceof Error ? error.message : String(error)}\n`);
    process.exitCode = 1;
  });
}
