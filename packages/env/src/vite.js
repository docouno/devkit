import fs from 'node:fs';
import path from 'node:path';
import { within } from './paths.js';

const object = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);

function findDevkitProjectRoot(startDir) {
  let current = path.resolve(startDir);
  while (true) {
    const manifest = path.join(current, '.devkit', 'manifest.json');
    if (fs.existsSync(manifest) && fs.statSync(manifest).isFile()) {
      return current;
    }
    const parent = path.dirname(current);
    if (parent === current) {
      return null;
    }
    current = parent;
  }
}

function runtimeExportTarget(value) {
  if (typeof value === 'string') {
    return value;
  }
  if (!object(value)) {
    return null;
  }
  for (const key of ['development', 'import', 'module', 'default']) {
    if (key in value) {
      const resolved = runtimeExportTarget(value[key]);
      if (resolved) {
        return resolved;
      }
    }
  }
  return null;
}

function existingRegularFile(root, candidate) {
  const full = path.resolve(root, candidate);
  if (!within(root, full) || !fs.existsSync(full)) {
    return null;
  }
  const stat = fs.lstatSync(full);
  return stat.isFile() && !stat.isSymbolicLink() ? full : null;
}

function resolveCssSource(srcRoot, exportKey, distRelPath) {
  const candidates = [];
  if (exportKey.startsWith('./')) {
    candidates.push(exportKey.slice(2));
  }
  candidates.push(distRelPath, 'styles.css', 'style.css', 'index.css');
  for (const candidate of candidates) {
    const found = existingRegularFile(srcRoot, candidate);
    if (found) {
      return found;
    }
  }
  return null;
}

function resolveModuleSource(srcRoot, distRelPath) {
  const stem = distRelPath.replace(/\.(?:mjs|cjs|js)$/u, '');
  for (const ext of ['.tsx', '.ts', '.mts', '.jsx', '.js', '.mjs']) {
    const found = existingRegularFile(srcRoot, `${stem}${ext}`);
    if (found) {
      return found;
    }
  }
  return null;
}

function collectWorkspaceSourceMap(startDir) {
  const sourceMap = new Map();
  const allowedDirs = new Set();
  const excludedDeps = new Set();
  const projectRoot = findDevkitProjectRoot(startDir);
  if (!projectRoot) {
    return { projectRoot: null, sourceMap, allowedDirs, excludedDeps };
  }

  let manifest;
  try {
    manifest = JSON.parse(fs.readFileSync(path.join(projectRoot, '.devkit/manifest.json'), 'utf8'));
  } catch {
    return { projectRoot: null, sourceMap, allowedDirs, excludedDeps };
  }
  if (!object(manifest) || !object(manifest.components)) {
    return { projectRoot: null, sourceMap, allowedDirs, excludedDeps };
  }

  for (const [compId, compMeta] of Object.entries(manifest.components)) {
    if (!compId.startsWith('packages/') || !object(compMeta) || compMeta.role !== 'library') {
      continue;
    }
    const pkgRoot = path.join(projectRoot, compId);
    const srcRoot = path.join(pkgRoot, 'src');
    const pkgJsonPath = path.join(pkgRoot, 'package.json');
    if (
      !fs.existsSync(pkgJsonPath) ||
      !fs.existsSync(srcRoot) ||
      !fs.statSync(srcRoot).isDirectory()
    ) {
      continue;
    }
    let pkgJson;
    try {
      pkgJson = JSON.parse(fs.readFileSync(pkgJsonPath, 'utf8'));
    } catch {
      continue;
    }
    if (!object(pkgJson) || typeof pkgJson.name !== 'string' || !pkgJson.name) {
      continue;
    }
    const pkgName = pkgJson.name;
    const rawExports =
      typeof pkgJson.exports === 'string'
        ? { '.': pkgJson.exports }
        : object(pkgJson.exports)
          ? pkgJson.exports
          : null;
    if (!rawExports) {
      continue;
    }

    let mappedAny = false;
    for (const [exportKey, exportVal] of Object.entries(rawExports)) {
      if ((exportKey !== '.' && !exportKey.startsWith('./')) || exportKey === './package.json') {
        continue;
      }
      if (exportKey.includes('*')) {
        continue;
      }
      const target = runtimeExportTarget(exportVal);
      if (!target || !target.startsWith('./dist/')) {
        continue;
      }
      const distRel = target.slice('./dist/'.length);
      let resolvedSource = null;
      if (distRel.endsWith('.css')) {
        resolvedSource = resolveCssSource(srcRoot, exportKey, distRel);
      } else if (/\.(?:mjs|cjs|js)$/u.test(distRel)) {
        resolvedSource = resolveModuleSource(srcRoot, distRel);
      }
      if (resolvedSource) {
        const specifier = exportKey === '.' ? pkgName : `${pkgName}/${exportKey.slice(2)}`;
        sourceMap.set(specifier, resolvedSource);
        mappedAny = true;
      }
    }
    if (mappedAny) {
      excludedDeps.add(pkgName);
      allowedDirs.add(pkgRoot);
      allowedDirs.add(srcRoot);
    }
  }

  return { projectRoot, sourceMap, allowedDirs, excludedDeps };
}

export function devkitWorkspaceSource(projectDir) {
  let activeSourceMap = new Map();

  return {
    name: 'devkit:workspace-source',
    enforce: 'pre',
    apply: 'serve',
    config(userConfig, env) {
      if (env && env.command !== 'serve') {
        activeSourceMap = new Map();
        return undefined;
      }
      const appRoot = path.resolve(projectDir ?? userConfig?.root ?? process.cwd());
      const { projectRoot, sourceMap, allowedDirs, excludedDeps } =
        collectWorkspaceSourceMap(appRoot);
      activeSourceMap = sourceMap;
      if (!projectRoot || excludedDeps.size === 0) {
        return undefined;
      }
      const existingAllow = Array.isArray(userConfig?.server?.fs?.allow)
        ? userConfig.server.fs.allow
        : [];
      const existingExclude = Array.isArray(userConfig?.optimizeDeps?.exclude)
        ? userConfig.optimizeDeps.exclude
        : [];
      return {
        server: {
          fs: {
            allow: [...new Set([...existingAllow, projectRoot, appRoot, ...allowedDirs])],
          },
        },
        optimizeDeps: {
          exclude: [...new Set([...existingExclude, ...excludedDeps])],
        },
      };
    },
    resolveId(source) {
      if (activeSourceMap.size === 0 || typeof source !== 'string') {
        return null;
      }
      const queryIndex = source.indexOf('?');
      const clean = queryIndex === -1 ? source : source.slice(0, queryIndex);
      const suffix = queryIndex === -1 ? '' : source.slice(queryIndex);
      const resolved = activeSourceMap.get(clean);
      return resolved ? `${resolved}${suffix}` : null;
    },
  };
}
