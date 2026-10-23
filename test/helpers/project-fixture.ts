import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileMap, writeJson } from '../../tooling/files.ts';
import type { Release } from '../../tooling/types.ts';

const toolchain =
  'DEVKIT_NODE_IMAGE=node:24.18.0-bookworm-slim@sha256:6f7b03f7c2c8e2e784dcf9295400527b9b1270fd37b7e9a7285cf83b6951452d\nDEVKIT_NODE_VERSION=24.18.0\nDEVKIT_RUST_VERSION=1.99.1\nDEVKIT_RUST_RECIPE_HASH=' +
  '0'.repeat(64) +
  '\n';

export function fixture(): { root: string; close: () => void } {
  const root = mkdtempSync(join(tmpdir(), 'devkit project tests '));
  return { root, close: () => rmSync(root, { recursive: true, force: true }) };
}

export function put(root: string, path: string, bytes: string): void {
  const target = join(root, path);
  mkdirSync(dirname(target), { recursive: true });
  writeFileSync(target, bytes);
}

export function release(root: string, version: string, options: { runtime?: string } = {}): string {
  const destination = join(root, `release-${version}-${options.runtime ?? 'normal'}`);
  mkdirSync(destination);
  for (const name of ['devkit.mk', 'devkit.sh', 'guard.sh', 'compose.tools.yaml']) {
    put(destination, `runtime/${name}`, `${options.runtime ?? 'runtime'} ${version} ${name}\n`);
  }
  put(destination, 'runtime/toolchains.env', `DEVKIT_VERSION=${version}\n${toolchain}`);
  const packageRoot = join(destination, 'packing');
  put(
    packageRoot,
    'package/package.json',
    JSON.stringify({
      name: '@docouno/env',
      version,
      type: 'module',
      exports: './index.js',
    }),
  );
  put(packageRoot, 'package/index.js', 'export const fixture = true;\n');
  mkdirSync(join(destination, 'npm'));
  const archive = `npm/docouno-env-${version}.tgz`;
  execFileSync('tar', ['-czf', join(destination, archive), '-C', packageRoot, 'package']);
  rmSync(packageRoot, { recursive: true });
  for (const template of ['node-api', 'react-web']) {
    const manifest = {
      name: `fixture-${template}`,
      version: '1.0.0',
      private: true,
      type: 'module',
      scripts: Object.fromEntries(
        ['check', 'fix', 'test', 'build', 'dev'].map((key) => [key, 'node --version']),
      ),
      devDependencies: { '@docouno/env': version },
    };
    put(
      destination,
      `templates/${template}/package.json`,
      `${JSON.stringify(manifest, null, 2)}\n`,
    );
    const integrity = `sha512-${createHash('sha512')
      .update(readFileSync(join(destination, archive)))
      .digest('base64')}`;
    put(
      destination,
      `templates/${template}/package-lock.json`,
      JSON.stringify({
        name: manifest.name,
        version: manifest.version,
        lockfileVersion: 3,
        requires: true,
        packages: {
          '': {
            name: manifest.name,
            version: manifest.version,
            devDependencies: manifest.devDependencies,
          },
          'node_modules/@docouno/env': {
            version,
            resolved: `https://registry.npmjs.org/@docouno/env/-/env-${version}.tgz`,
            integrity,
            dev: true,
          },
        },
      }),
    );
    put(destination, `templates/${template}/src/index.ts`, 'export const answer = 42;\n');
  }
  put(
    destination,
    'templates/react-web/src/main.tsx',
    readFileSync(new URL('../../templates/react-web/src/main.tsx', import.meta.url), 'utf8'),
  );
  put(
    destination,
    'templates/rust-api/Cargo.toml',
    '[package]\nname = "fixture-api"\nversion = "1.0.0"\nedition = "2024"\n',
  );
  put(destination, 'templates/rust-api/Cargo.lock', '# fixture native lock\nversion = 4\n');
  put(destination, 'templates/rust-api/rustfmt.toml', 'edition = "2024"\n');
  put(destination, 'templates/rust-api/rust-toolchain.toml', '[toolchain]\nchannel = "1.99.1"\n');
  put(destination, 'templates/rust-api/src/main.rs', 'fn main() {}\n');
  seal(destination, version, archive);
  return destination;
}

export function seal(
  destination: string,
  version: string,
  archive = `npm/docouno-env-${version}.tgz`,
): void {
  rmSync(join(destination, 'release.json'), { force: true });
  const manifest: Release = {
    schemaVersion: 1,
    version,
    rustVersion: '1.99.1',
    files: fileMap(destination),
    environment: {
      name: '@docouno/env',
      version,
      archive,
      integrity: `sha512-${createHash('sha512')
        .update(readFileSync(join(destination, archive)))
        .digest('base64')}`,
    },
  };
  writeJson(join(destination, 'release.json'), manifest);
}
