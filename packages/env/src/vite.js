import fs from 'node:fs';
import path from 'node:path';
import { within } from './paths.js';
import { configureApiProxy, configurePreviewHosts, hostPolicy } from './preview.js';
import { hmrRecovery } from './hmr.js';

const object = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);

function entryExists(target) {
  try {
    fs.lstatSync(target);
    return true;
  } catch {
    return false;
  }
}

function findDevkitProjectRoot(startDir) {
  let current = path.resolve(startDir);
  while (true) {
    const devkitDir = path.join(current, '.devkit');
    if (entryExists(devkitDir)) {
      const manifest = path.join(devkitDir, 'manifest.json');
      return regularDirectory(devkitDir) && regularFile(manifest) ? current : null;
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
  for (const key of ['development', 'browser', 'import', 'module', 'default']) {
    if (key in value) {
      const resolved = runtimeExportTarget(value[key]);
      if (resolved) {
        return resolved;
      }
    }
  }
  return null;
}

function regularDirectory(target) {
  try {
    const stat = fs.lstatSync(target);
    return stat.isDirectory() && !stat.isSymbolicLink();
  } catch {
    return false;
  }
}

function regularFile(target) {
  try {
    const stat = fs.lstatSync(target);
    return stat.isFile() && !stat.isSymbolicLink();
  } catch {
    return false;
  }
}

function existingRegularFile(root, candidate) {
  const full = path.resolve(root, candidate);
  if (!within(root, full)) {
    return null;
  }
  const rel = path.relative(root, full);
  if (!rel || rel.startsWith('..') || path.isAbsolute(rel)) {
    return null;
  }
  let current = root;
  const parts = rel.split(path.sep);
  for (let i = 0; i < parts.length; i += 1) {
    current = path.join(current, parts[i]);
    let stat;
    try {
      stat = fs.lstatSync(current);
    } catch {
      return null;
    }
    if (stat.isSymbolicLink()) {
      return null;
    }
    if (i + 1 < parts.length ? !stat.isDirectory() : !stat.isFile()) {
      return null;
    }
  }
  try {
    if (fs.realpathSync(full) !== path.resolve(fs.realpathSync(root), rel)) {
      return null;
    }
  } catch {
    return null;
  }
  return full.replaceAll('\\', '/');
}

function resolveCssSource(srcRoot, exportKey, distRelPath) {
  const candidates = [];
  if (exportKey.startsWith('./')) {
    candidates.push(exportKey.slice(2));
  }
  candidates.push(distRelPath);
  if (
    ['./styles.css', './style.css', './index.css'].includes(exportKey) ||
    distRelPath === 'style.css'
  ) {
    candidates.push('styles.css', 'style.css', 'index.css');
  }
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
  const exts = ['.tsx', '.ts', '.mts', '.jsx', '.js', '.mjs'];
  for (const ext of exts) {
    const found = existingRegularFile(srcRoot, `${stem}${ext}`);
    if (found) {
      return found;
    }
  }
  for (const ext of exts) {
    const found = existingRegularFile(srcRoot, `${stem}/index${ext}`);
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

  const packagesDir = path.join(projectRoot, 'packages');
  for (const [compId, compMeta] of Object.entries(manifest.components)) {
    if (
      !/^packages\/[a-z][a-z0-9]*(?:-[a-z0-9]+)*$/u.test(compId) ||
      !object(compMeta) ||
      compMeta.role !== 'library'
    ) {
      continue;
    }
    const pkgRoot = path.join(projectRoot, compId);
    if (!within(packagesDir, pkgRoot)) {
      continue;
    }
    const srcRoot = path.join(pkgRoot, 'src');
    const pkgJsonPath = path.join(pkgRoot, 'package.json');
    if (
      !regularDirectory(packagesDir) ||
      !regularDirectory(pkgRoot) ||
      !regularFile(pkgJsonPath) ||
      !regularDirectory(srcRoot)
    ) {
      continue;
    }
    try {
      const realProject = fs.realpathSync(projectRoot);
      if (fs.realpathSync(srcRoot) !== path.resolve(realProject, compId, 'src')) {
        continue;
      }
    } catch {
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
      if (!target) {
        continue;
      }
      let resolvedSource = null;
      if (target.startsWith('./src/')) {
        const srcRel = target.slice('./src/'.length);
        if (/\.(?:tsx|ts|mts|jsx|js|mjs|css)$/u.test(srcRel)) {
          resolvedSource = existingRegularFile(srcRoot, srcRel);
        }
      } else if (target.startsWith('./dist/')) {
        const distRel = target.slice('./dist/'.length);
        if (distRel.endsWith('.css')) {
          resolvedSource = resolveCssSource(srcRoot, exportKey, distRel);
        } else if (/\.(?:mjs|cjs|js)$/u.test(distRel)) {
          resolvedSource = resolveModuleSource(srcRoot, distRel);
        }
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
  let fallbackConfig;
  const recoveries = new WeakMap();

  return {
    name: 'devkit:workspace-source',
    enforce: 'pre',
    apply: 'serve',
    config(userConfig, env) {
      fallbackConfig = undefined;
      if (env?.command !== 'serve' || process.env.VITEST) {
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
    configResolved(config) {
      if (config.command !== 'serve' || process.env.VITEST) {
        return;
      }
      fallbackConfig = config;
      recoveries.set(config, hmrRecovery());
      configurePreviewHosts(config.server);
    },
    configureServer(server) {
      if (!recoveries.has(server.config)) {
        return;
      }
      const options = server.config.server;
      const allowsHost = hostPolicy(options.allowedHosts ?? []);
      configureApiProxy(options, allowsHost);
      // Vite's native HTTP guard already uses the resolved allowlist. It skips
      // HTTPS listeners, so apply the same policy there before serving requests.
      if (options.https && options.allowedHosts !== true) {
        server.middlewares.use((request, response, next) => {
          if (allowsHost(request.headers.host)) {
            next();
            return;
          }
          response.statusCode = 403;
          response.end('Blocked request: host is not allowed.');
        });
      }
    },
    transform(code, id) {
      const config = this.environment?.getTopLevelConfig?.() ?? fallbackConfig;
      return (
        recoveries.get(config)?.(code, id, (message) => this.warn(message), config.server) ?? null
      );
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
