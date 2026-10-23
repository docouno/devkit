import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { fail, slug } from './files.ts';
import { initProject } from './project.ts';
import type { Template } from './project.ts';
import { recoverProject, registerProjectApp, updateProject } from './project-update.ts';

const help = `Usage:
  devkit init TARGET --template node|react|rust|mixed [--package-source local|registry] [--name slug]
  devkit update TARGET [--package-source local|registry]
  devkit recover TARGET
  devkit register TARGET --app slug --template node|react|rust

Init defaults to registry packages. Source-checkout make init explicitly uses local archives.
Update preserves package source unless explicitly changed. Recover rolls back a pending update.
Register adopts an existing app with this exact release's native tools; then add it to DEVKIT_APPS.
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
  if (args.length === 0 || ['help', '--help', '-h'].includes(args[0]!)) {
    process.stdout.write(help);
    return;
  }
  const [command, target, ...rest] = args;
  if (!['init', 'update', 'recover', 'register'].includes(command!)) {
    fail(`Unknown command: ${command}\n${help}`);
  }
  if (!target || target.startsWith('--')) {
    fail('Expected a target project path');
  }
  const values: Record<string, string> = {};
  const allowed =
    command === 'init'
      ? ['--template', '--package-source', '--name']
      : command === 'update'
        ? ['--package-source']
        : command === 'register'
          ? ['--template', '--app']
          : [];
  for (let index = 0; index < rest.length; index += 2) {
    const key = rest[index]!;
    const value = rest[index + 1];
    if (!allowed.includes(key) || Object.hasOwn(values, key)) {
      fail(`Unknown or repeated option: ${key}`);
    }
    if (!value || value.startsWith('--')) {
      fail(`Missing value for ${key}`);
    }
    values[key] = value;
  }
  const packageMode = values['--package-source'];
  if (packageMode !== undefined && packageMode !== 'local' && packageMode !== 'registry') {
    fail(`Invalid package source: ${packageMode}`);
  }
  const template = values['--template'];
  if (
    template !== undefined &&
    !['node', 'react', 'rust', ...(command === 'init' ? ['mixed'] : [])].includes(template)
  ) {
    fail(`Invalid template: ${template}`);
  }
  if (values['--name'] !== undefined) {
    slug(values['--name']);
  }
  if (command === 'init') {
    if (template === undefined) {
      fail('Init requires --template node|react|rust|mixed');
    }
    const state = await initProject(releaseRoot, target, {
      template: template as Template,
      packageMode: packageMode ?? 'registry',
      ...(values['--name'] === undefined ? {} : { name: values['--name'] }),
    });
    process.stdout.write(
      `Created a project with devkit ${state.version}; package source: ${state.packageMode}. Run make setup in the new project directory.\n`,
    );
  } else if (command === 'update') {
    const state = await updateProject(
      releaseRoot,
      target,
      packageMode === undefined ? {} : { packageMode },
    );
    process.stdout.write(
      `Accepted devkit ${state.version}; package source: ${state.packageMode}. Review the project diff, then run make setup.\n`,
    );
  } else if (command === 'recover') {
    process.stdout.write(
      recoverProject(target)
        ? 'Rolled back the pending update. Review the project diff before retrying.\n'
        : 'No pending update.\n',
    );
  } else {
    const name = values['--app'];
    if (name === undefined || template === undefined) {
      fail('Register requires --app slug --template node|react|rust');
    }
    await registerProjectApp(
      releaseRoot,
      target,
      slug(name),
      template as 'node' | 'react' | 'rust',
    );
    process.stdout.write(
      `Registered apps/${name}. Add ${name} to DEVKIT_APPS in the project Makefile and its development service to compose.yaml, then run make setup.\n`,
    );
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main(process.argv.slice(2)).catch((error: unknown) => {
    process.stderr.write(`devkit: ${error instanceof Error ? error.message : String(error)}\n`);
    process.exitCode = 1;
  });
}
