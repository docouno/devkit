import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';
import ts from 'typescript';
import { within } from './paths.js';

const read = (file) => JSON.parse(fs.readFileSync(file, 'utf8'));
const canonical = (file) => (fs.existsSync(file) ? fs.realpathSync(file) : path.resolve(file));
const depFields = ['dependencies', 'optionalDependencies', 'peerDependencies', 'devDependencies'];
const object = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);

export function workspaceContext(componentRoot) {
  if (!['apps', 'packages'].includes(path.basename(path.dirname(componentRoot)))) {
    return null;
  }
  const projectRoot = path.resolve(componentRoot, '../..');
  const file = path.join(projectRoot, '.devkit/manifest.json');
  if (!fs.existsSync(file)) {
    return null;
  }
  const state = read(file);
  if (state.schemaVersion === 1) {
    return null;
  }
  if (state.schemaVersion !== 2 || !object(state.components)) {
    throw new Error('Unsupported Devkit workspace metadata.');
  }
  const components = Object.entries(state.components).map(([id, value]) => {
    if (
      !/^(apps|packages)\/[a-z][a-z0-9]*(?:-[a-z0-9]+)*$/u.test(id) ||
      !object(value) ||
      !['app', 'library'].includes(value.role)
    ) {
      throw new Error('Invalid workspace component.');
    }
    const root = path.join(projectRoot, id);
    if (canonical(root) !== root) {
      throw new Error(`Component path is redirected: ${id}`);
    }
    return { id, role: value.role, root };
  });
  const current = components.find((entry) => entry.root === componentRoot);
  if (!current) {
    throw new Error('The current component is not registered with Devkit.');
  }
  const manifest = read(path.join(componentRoot, 'package.json'));
  const declared = new Set(depFields.flatMap((field) => Object.keys(manifest[field] ?? {})));
  const owner = (file) => {
    const actual = canonical(file);
    return components.find(
      (entry) =>
        within(entry.root, actual) &&
        !path.relative(entry.root, actual).split(path.sep).includes('node_modules'),
    );
  };
  return { projectRoot, components, current, declared, owner };
}

function imports(source, options) {
  const result = [];
  source.impliedNodeFormat = ts.getImpliedNodeFormatForFile(
    source.fileName,
    undefined,
    ts.sys,
    options,
  );
  const add = (node, override) => {
    if (node && ts.isStringLiteralLike(node)) {
      result.push({
        specifier: node.text,
        mode: override ?? ts.getModeForUsageLocation(source, node, options),
      });
    }
  };
  const visit = (node) => {
    if (ts.isImportDeclaration(node) || ts.isExportDeclaration(node)) {
      add(node.moduleSpecifier);
    } else if (
      ts.isImportEqualsDeclaration(node) &&
      ts.isExternalModuleReference(node.moduleReference)
    ) {
      add(node.moduleReference.expression, ts.ModuleKind.CommonJS);
    } else if (ts.isImportTypeNode(node) && ts.isLiteralTypeNode(node.argument)) {
      add(node.argument.literal);
    } else if (ts.isCallExpression(node) && node.arguments[0]) {
      if (node.expression.kind === ts.SyntaxKind.ImportKeyword) {
        add(node.arguments[0], ts.ModuleKind.ESNext);
      } else if (ts.isIdentifier(node.expression) && node.expression.text === 'require') {
        add(node.arguments[0], ts.ModuleKind.CommonJS);
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(source);
  return result;
}

function withoutPathAliases(options) {
  const clean = { ...options, resolvePackageJsonExports: true, resolvePackageJsonImports: true };
  delete clean.paths;
  delete clean.baseUrl;
  delete clean.rootDirs;
  return clean;
}

function assetCandidate(file) {
  const candidate = file
    .replace(/\.d\.(css|svg|png|jpg|jpeg|webp)\.ts$/iu, '.$1')
    .replace(/\.(css|svg|png|jpg|jpeg|webp)\.(?:[cm]?[jt]sx?)$/iu, '.$1');
  return candidate !== file && fs.existsSync(candidate) && fs.statSync(candidate).isFile()
    ? canonical(candidate)
    : undefined;
}

// Let TypeScript choose path/condition order, using in-memory declarations for assets.
// No declaration file is written and no application module is evaluated.
function resolveAsset(specifier, file, options, mode) {
  const assets = new Map();
  const host = {
    ...ts.sys,
    fileExists(candidate) {
      if (ts.sys.fileExists(candidate)) {
        return true;
      }
      const asset = assetCandidate(candidate);
      if (asset) {
        assets.set(candidate, asset);
        return true;
      }
      return false;
    },
    realpath(candidate) {
      return assets.has(candidate) ? candidate : ts.sys.realpath(candidate);
    },
  };
  const resolved = ts.resolveModuleName(
    specifier,
    file,
    { ...options, allowArbitraryExtensions: true },
    host,
    undefined,
    undefined,
    mode,
  ).resolvedModule?.resolvedFileName;
  return resolved ? (assets.get(resolved) ?? assetCandidate(resolved)) : undefined;
}

function installedPackage(file, specifier) {
  if (
    specifier.startsWith('.') ||
    specifier.startsWith('#') ||
    path.isAbsolute(specifier) ||
    specifier.includes(':')
  ) {
    return null;
  }
  const segments = specifier.split('/');
  const name = specifier.startsWith('@') ? segments.slice(0, 2).join('/') : segments[0];
  if (!name || !/^(?:@[a-z0-9._-]+\/)?[a-z0-9._-]+$/u.test(name)) {
    return null;
  }
  for (const directory of createRequire(file).resolve.paths(name) ?? []) {
    const root = path.join(directory, name);
    const manifest = path.join(root, 'package.json');
    if (fs.existsSync(manifest)) {
      return {
        name,
        root: canonical(root),
        manifest: read(manifest),
        subpath: specifier.slice(name.length) || '.',
      };
    }
  }
  return null;
}

/** Validate static imports against actual installed resolution and declared workspace edges. */
export function workspaceImportErrors(context, file, text, compilerOptions) {
  if (!context) {
    return [];
  }
  const errors = [];
  const source = ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true);
  for (const { specifier, mode } of imports(source, compilerOptions)) {
    const resolved = ts.resolveModuleName(
      specifier,
      file,
      compilerOptions,
      ts.sys,
      undefined,
      undefined,
      mode,
    );
    let target = resolved.resolvedModule?.resolvedFileName;
    const relative = specifier.startsWith('.') || path.isAbsolute(specifier);
    const lexical = relative ? context.owner(path.resolve(path.dirname(file), specifier)) : null;
    const installed = relative ? null : installedPackage(file, specifier);
    const installedOwner = installed ? context.owner(installed.root) : null;
    let asset = false;
    let publicAsset;
    if (!target && installedOwner && /\.(?:css|svg|png|jpg|jpeg|webp)$/iu.test(specifier)) {
      asset = true;
      publicAsset = resolveAsset(specifier, file, withoutPathAliases(compilerOptions), mode);
      target = resolveAsset(specifier, file, compilerOptions, mode) ?? publicAsset;
    }
    if (asset && target && (!publicAsset || canonical(target) !== canonical(publicAsset))) {
      errors.push(
        `Import ${JSON.stringify(specifier)} redirects a public asset export through a path alias.`,
      );
      continue;
    }
    const owner = target ? context.owner(target) : (lexical ?? installedOwner);
    if (!owner || owner.id === context.current.id) {
      if (
        !target &&
        !relative &&
        resolved.failedLookupLocations?.some((entry) => {
          const candidate = context.owner(entry);
          return candidate && candidate.id !== context.current.id;
        })
      ) {
        errors.push(
          `Import ${JSON.stringify(specifier)} cannot establish a public workspace boundary.`,
        );
      }
      continue;
    }
    const reject = (reason) => errors.push(`Import ${JSON.stringify(specifier)} ${reason}`);
    if (owner.role !== 'library') {
      reject('targets another application.');
      continue;
    }
    if (relative) {
      reject('must use the library public package export.');
      continue;
    }
    if (!installed || installed.root !== owner.root || !context.declared.has(installed.name)) {
      reject('is not a declared dependency on the resolved workspace library.');
      continue;
    }
    if (!object(installed.manifest.exports) && typeof installed.manifest.exports !== 'string') {
      reject('targets a library without explicit public exports.');
      continue;
    }
    if (!target) {
      reject('does not resolve through a public library export.');
      continue;
    }
    if (!asset) {
      const options = withoutPathAliases(compilerOptions);
      const publicResolution = ts.resolveModuleName(
        specifier,
        file,
        options,
        ts.sys,
        undefined,
        undefined,
        mode,
      ).resolvedModule?.resolvedFileName;
      if (!publicResolution || canonical(publicResolution) !== canonical(target)) {
        reject('bypasses the library public export through a path alias.');
      }
    }
  }
  return errors;
}
