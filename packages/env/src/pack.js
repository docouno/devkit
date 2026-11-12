import { spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import { createRequire } from 'node:module';
import os from 'node:os';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import ts from 'typescript';

const object = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);
const assetEntrypointPattern = /\.(?:css|svg|png|jpg|jpeg|webp|woff2?|ttf|eot)$/iu;

function collectExportLeafTargets(exportsField) {
  const targets = [];
  const visit = (value) => {
    if (typeof value === 'string') {
      targets.push(value);
      return;
    }
    if (Array.isArray(value)) {
      for (const item of value) {
        visit(item);
      }
      return;
    }
    if (object(value)) {
      for (const child of Object.values(value)) {
        visit(child);
      }
    }
  };
  visit(exportsField);
  return targets;
}

function isForbiddenPackedFile(relPath) {
  const normalized = relPath.split(path.sep).join('/');
  const segments = normalized.split('/');
  const base = segments[segments.length - 1] ?? '';
  if (
    normalized.startsWith('src/') ||
    normalized.startsWith('test/') ||
    normalized.startsWith('tests/') ||
    segments.includes('__tests__') ||
    segments.includes('__mocks__')
  ) {
    return true;
  }
  if (/\.(?:test|spec)\.[^/]+$/iu.test(base) || /\.tsbuildinfo$/iu.test(base)) {
    return true;
  }
  if (segments.some((seg) => seg === '.env' || seg.startsWith('.env.'))) {
    return true;
  }
  if (
    /^tsconfig(?:\..+)?\.json$/iu.test(base) ||
    /^(?:tsdown|vitest|eslint|vite|prettier)\.config\.[^/]+$/iu.test(base)
  ) {
    return true;
  }
  return false;
}

function declarationLeakErrors(relPath, content) {
  const issues = [];
  const ambientMatch = content.match(
    /\/\/\/\s*<reference\s+types\s*=\s*["'](vite\/client|vitest(?:\/[^\s"']+)?|@testing-library\/[^\s"']+)["']/iu,
  );
  if (ambientMatch) {
    issues.push(`Declaration file ${relPath} leaks a "${ambientMatch[1]}" reference.`);
  }
  if (/\/\/\/\s*<reference\s+path\s*=\s*["'][^"']*(?:^|\/)\.\.\/(?:\.\.\/)*src\//iu.test(content)) {
    issues.push(`Declaration file ${relPath} leaks a relative ../src/ reference.`);
  }
  const source = ts.createSourceFile(relPath, content, ts.ScriptTarget.Latest, true);
  const checkSpecifier = (spec) => {
    if (/(?:^|\/)\.\.\/(?:\.\.\/)*src(?:\/|$)/u.test(spec)) {
      issues.push(`Declaration file ${relPath} imports relative source path "${spec}".`);
    }
  };
  const visit = (node) => {
    if (
      (ts.isImportDeclaration(node) || ts.isExportDeclaration(node)) &&
      node.moduleSpecifier &&
      ts.isStringLiteralLike(node.moduleSpecifier)
    ) {
      checkSpecifier(node.moduleSpecifier.text);
    } else if (ts.isImportTypeNode(node) && ts.isLiteralTypeNode(node.argument)) {
      checkSpecifier(node.argument.literal.text);
    }
    ts.forEachChild(node, visit);
  };
  visit(source);
  return issues;
}

async function loadConsumerValidator(componentDir, specifier) {
  const fallbackReq = createRequire(import.meta.url);
  let resolvedPath = null;
  const pkgJsonPath = path.join(componentDir, 'package.json');
  if (fs.existsSync(pkgJsonPath)) {
    try {
      const consumerReq = createRequire(pathToFileURL(pkgJsonPath).href);
      resolvedPath = consumerReq.resolve(specifier);
    } catch {
      resolvedPath = null;
    }
  }
  if (!resolvedPath) {
    try {
      resolvedPath = fallbackReq.resolve(specifier);
    } catch {
      throw new Error(
        `Missing required packaging validator "${specifier}"; declare its base package in devDependencies and run make setup.`,
      );
    }
  }
  return import(pathToFileURL(resolvedPath).href);
}

function formatAttwProblem(problem, problemKindInfo) {
  const title = problemKindInfo[problem.kind]?.title ?? problem.kind;
  const details = [];
  if ('entrypoint' in problem && typeof problem.entrypoint === 'string') {
    details.push(`entrypoint "${problem.entrypoint}"`);
  }
  if ('resolutionKind' in problem && typeof problem.resolutionKind === 'string') {
    details.push(`mode ${problem.resolutionKind}`);
  }
  if ('fileName' in problem && typeof problem.fileName === 'string') {
    details.push(`file ${problem.fileName}`);
  }
  if ('typesFileName' in problem && typeof problem.typesFileName === 'string') {
    details.push(`types ${problem.typesFileName}`);
  }
  if ('moduleSpecifier' in problem && typeof problem.moduleSpecifier === 'string') {
    details.push(`specifier "${problem.moduleSpecifier}"`);
  }
  return `[attw:${problem.kind}] ${title}${details.length ? ` (${details.join(', ')})` : ''}`;
}

export async function verifyPackableComponent(componentDir, options = {}) {
  const resolvedComponent = fs.realpathSync(path.resolve(componentDir));
  const ownWorkDir = !options.outputDir;
  const workDir = options.outputDir
    ? path.resolve(options.outputDir)
    : fs.mkdtempSync(path.join(os.tmpdir(), 'devkit-pack-verify-'));
  fs.mkdirSync(workDir, { recursive: true });
  const cacheDir = process.env.NPM_CONFIG_CACHE ?? path.join(workDir, '.npm-cache');

  try {
    const npmCmd = process.platform === 'win32' ? 'npm.cmd' : 'npm';
    const packed = spawnSync(
      npmCmd,
      ['pack', '--pack-destination', workDir, '--json', '--ignore-scripts'],
      {
        cwd: resolvedComponent,
        encoding: 'utf8',
        timeout: 60_000,
        env: {
          ...process.env,
          NPM_CONFIG_CACHE: cacheDir,
          NPM_CONFIG_OFFLINE: 'true',
          NPM_CONFIG_UPDATE_NOTIFIER: 'false',
          NPM_CONFIG_FUND: 'false',
          NPM_CONFIG_AUDIT: 'false',
        },
      },
    );
    if (packed.status !== 0) {
      throw new Error(
        `npm pack failed (${packed.status ?? 'signal'}): ${(packed.stderr || packed.stdout || '').trim()}`,
      );
    }

    let packList;
    try {
      packList = JSON.parse(packed.stdout);
    } catch {
      throw new Error(`npm pack returned invalid JSON: ${packed.stdout.slice(0, 200)}`);
    }
    if (!Array.isArray(packList) || packList.length !== 1 || !object(packList[0])) {
      throw new Error('npm pack returned an unexpected archive descriptor.');
    }
    const descriptor = packList[0];
    if (typeof descriptor.filename !== 'string' || !descriptor.filename) {
      throw new Error('npm pack did not report an output filename.');
    }
    const tarballPath = path.join(workDir, path.basename(descriptor.filename));
    if (!fs.existsSync(tarballPath) || !fs.statSync(tarballPath).isFile()) {
      throw new Error(`Packed archive does not exist: ${tarballPath}`);
    }

    const bytes = fs.readFileSync(tarballPath);
    const sha256 = crypto.createHash('sha256').update(bytes).digest('hex');
    const shasum = crypto.createHash('sha1').update(bytes).digest('hex');
    const integrity = `sha512-${crypto.createHash('sha512').update(bytes).digest('base64')}`;
    const size = bytes.length;

    const [{ publint }, { formatMessage }, attwCore, attwProblems] = await Promise.all([
      loadConsumerValidator(resolvedComponent, 'publint'),
      loadConsumerValidator(resolvedComponent, 'publint/utils'),
      loadConsumerValidator(resolvedComponent, '@arethetypeswrong/core'),
      loadConsumerValidator(resolvedComponent, '@arethetypeswrong/core/problems'),
    ]);

    const attwPkg = attwCore.createPackageFromTarballData(new Uint8Array(bytes));
    const virtualPrefix = `/node_modules/${attwPkg.packageName}/`;
    const relFiles = attwPkg
      .listFiles()
      .filter((entry) => entry.startsWith(virtualPrefix))
      .map((entry) => entry.slice(virtualPrefix.length))
      .sort();

    const errors = [];
    if (!relFiles.some((file) => file !== 'package.json')) {
      errors.push('Packed tarball contains no distributable files besides package.json.');
    }

    for (const file of relFiles) {
      if (isForbiddenPackedFile(file)) {
        errors.push(`Packed tarball leaks forbidden source, test, or config file: ${file}`);
      }
      if (/\.d\.[cm]?ts$/iu.test(file)) {
        const text = attwPkg.readFile(`${virtualPrefix}${file}`);
        for (const issue of declarationLeakErrors(file, text)) {
          errors.push(issue);
        }
      }
    }

    const packedManifest = JSON.parse(attwPkg.readFile(`${virtualPrefix}package.json`));
    const exportTargets = collectExportLeafTargets(packedManifest.exports);
    if (exportTargets.length === 0) {
      errors.push('Packed package.json must declare explicit public exports.');
    }
    const relFileSet = new Set(relFiles);
    for (const target of exportTargets) {
      if (!target.startsWith('./')) {
        errors.push(`Invalid export target "${target}"; expected "./..." relative path.`);
        continue;
      }
      if (target.includes('*')) {
        continue;
      }
      const cleanTarget = target.slice(2);
      if (!relFileSet.has(cleanTarget)) {
        errors.push(`Export target "${target}" is missing from the packed tarball.`);
      }
    }

    const arrayBuffer = bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
    const publintResult = await publint({
      pack: { tarball: arrayBuffer },
      strict: true,
      level: 'error',
    });
    const allowPrivateLocal =
      options.allowPrivateLocalDeps === true && packedManifest.private === true;
    const publintErrors = publintResult.messages.filter(
      (msg) => !(allowPrivateLocal && msg.code === 'LOCAL_DEPENDENCY'),
    );
    for (const msg of publintErrors) {
      const formatted = formatMessage(msg, publintResult.pkg, { color: false }) ?? msg.code;
      errors.push(`[publint:${msg.code}] ${formatted}`);
    }

    const analysis = await attwCore.checkPackage(attwPkg, {
      excludeEntrypoints: [assetEntrypointPattern],
    });
    let attwPassed = false;
    if (analysis.types === false) {
      errors.push(
        `[attw:Untyped] Package ${attwPkg.packageName} does not ship TypeScript declaration files.`,
      );
    } else {
      const esmProblems = analysis.problems.filter(
        (problem) =>
          attwProblems.problemAffectsResolutionKind(problem, 'node16-esm', analysis) ||
          attwProblems.problemAffectsResolutionKind(problem, 'bundler', analysis),
      );
      for (const problem of esmProblems) {
        errors.push(formatAttwProblem(problem, attwProblems.problemKindInfo));
      }
      attwPassed = esmProblems.length === 0;
    }

    const receipt = {
      name: String(packedManifest.name ?? descriptor.name),
      version: String(packedManifest.version ?? descriptor.version),
      filename: path.basename(descriptor.filename),
      sha256,
      shasum,
      integrity,
      size,
      entryCount: relFiles.length,
      publintPassed: publintErrors.length === 0,
      attwProfile: 'esm-only',
      attwPassed,
    };

    return {
      ok: errors.length === 0,
      errors,
      tarballPath: ownWorkDir ? null : tarballPath,
      files: relFiles,
      receipt,
    };
  } finally {
    if (ownWorkDir) {
      fs.rmSync(workDir, { recursive: true, force: true });
    }
  }
}

export async function packComponent(componentDir, options = {}) {
  const resolvedComponent = fs.realpathSync(path.resolve(componentDir));
  const pkgPath = path.join(resolvedComponent, 'package.json');
  if (!fs.existsSync(pkgPath)) {
    throw new Error(`Missing package.json in ${resolvedComponent}`);
  }
  const manifest = JSON.parse(fs.readFileSync(pkgPath, 'utf8'));
  if (manifest.private !== false) {
    throw new Error(
      `Package ${manifest.name ?? resolvedComponent} has "private": true in package.json; set "private": false and run devkit sync before packaging.`,
    );
  }

  const stageDir = fs.mkdtempSync(path.join(os.tmpdir(), 'devkit-pack-stage-'));
  try {
    const verification = await verifyPackableComponent(resolvedComponent, {
      outputDir: stageDir,
      allowPrivateLocalDeps: false,
    });
    if (!verification.ok || !verification.tarballPath) {
      throw new Error(verification.errors.join('\n'));
    }

    const releaseDir = path.resolve(options.outputDir ?? path.join(resolvedComponent, 'release'));
    fs.mkdirSync(releaseDir, { recursive: true });

    const finalTarballPath = path.join(releaseDir, verification.receipt.filename);
    const tmpTarball = path.join(releaseDir, `.tmp-${crypto.randomUUID()}.tgz`);
    const receiptPath = path.join(releaseDir, 'package-receipt.json');
    const tmpReceipt = path.join(releaseDir, `.tmp-${crypto.randomUUID()}.json`);
    try {
      fs.copyFileSync(verification.tarballPath, tmpTarball);
      fs.renameSync(tmpTarball, finalTarballPath);

      for (const existing of fs.readdirSync(releaseDir)) {
        if (existing.endsWith('.tgz') && existing !== verification.receipt.filename) {
          fs.rmSync(path.join(releaseDir, existing), { force: true });
        }
      }

      fs.writeFileSync(tmpReceipt, `${JSON.stringify(verification.receipt, null, 2)}\n`, 'utf8');
      fs.renameSync(tmpReceipt, receiptPath);
    } finally {
      fs.rmSync(tmpTarball, { force: true });
      fs.rmSync(tmpReceipt, { force: true });
    }

    return {
      tarballPath: finalTarballPath,
      receiptPath,
      receipt: verification.receipt,
    };
  } finally {
    fs.rmSync(stageDir, { recursive: true, force: true });
  }
}
