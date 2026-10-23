import fs from 'node:fs';
import path from 'node:path';
import { ESLint } from 'eslint';
import ts from 'typescript';
import { relativeList, within } from './paths.js';

const semanticExtension = /\.(?:[cm]?[jt]s|[jt]sx)$/i;
const typescriptExtension = /\.(?:[cm]?ts|tsx)$/i;
const strictOptions = [
  'alwaysStrict',
  'noImplicitAny',
  'noImplicitThis',
  'strictBindCallApply',
  'strictBuiltinIteratorReturn',
  'strictFunctionTypes',
  'strictNullChecks',
  'strictPropertyInitialization',
  'useUnknownInCatchVariables',
];

const object = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);
const severity = (rule) => (Array.isArray(rule) ? rule[0] : rule);
const enabledError = (rule) => [2, 'error'].includes(severity(rule));

function metadata(packageJson) {
  const value = packageJson.devkit === undefined ? {} : packageJson.devkit;
  if (!object(value)) {
    throw new Error('devkit must be an object.');
  }
  for (const key of Object.keys(value)) {
    if (!['sourceDirectories', 'typecheckConfigs', 'boundaries'].includes(key)) {
      throw new Error(`Unknown devkit option: ${key}.`);
    }
  }
  const boundaries = value.boundaries === undefined ? {} : value.boundaries;
  if (!object(boundaries)) {
    throw new Error('devkit.boundaries must be an object.');
  }
  for (const key of Object.keys(boundaries)) {
    if (!['forbidCrossAppImports', 'forbiddenDirectories'].includes(key)) {
      throw new Error(`Unknown boundary option: ${key}.`);
    }
  }
  if (
    boundaries.forbidCrossAppImports !== undefined &&
    typeof boundaries.forbidCrossAppImports !== 'boolean'
  ) {
    throw new Error('forbidCrossAppImports must be a boolean.');
  }
  const forbidden =
    boundaries.forbiddenDirectories === undefined ? [] : boundaries.forbiddenDirectories;
  if (
    !Array.isArray(forbidden) ||
    forbidden.some(
      (name) =>
        typeof name !== 'string' || !/^[a-zA-Z0-9_.-]+$/u.test(name) || ['.', '..'].includes(name),
    )
  ) {
    throw new Error('forbiddenDirectories must be an array of directory basenames.');
  }
  if (new Set(forbidden).size !== forbidden.length) {
    throw new Error('forbiddenDirectories contains duplicates.');
  }
  return {
    sourceDirectories: relativeList(value.sourceDirectories, ['src'], 'sourceDirectories'),
    typecheckConfigs: relativeList(value.typecheckConfigs, ['tsconfig.json'], 'typecheckConfigs'),
    boundaries: {
      forbidCrossAppImports: boundaries.forbidCrossAppImports ?? true,
      forbiddenDirectories: forbidden,
    },
  };
}

function uncheckedHeader(text) {
  // Match the supported compiler's leading, single-line pragma grammar. Block
  // comments and fourth slashes are not ts-nocheck directives in TypeScript 6.0.
  return (ts.getLeadingCommentRanges(text, 0) ?? []).some((range) => {
    if (range.kind !== ts.SyntaxKind.SingleLineCommentTrivia) {
      return false;
    }
    const pragma = /^\/\/\/?\s*@([^\s:]+)((?:[^\S\r\n]|:).*)?$/m.exec(
      text.slice(range.pos, range.end),
    );
    return pragma?.[1].toLowerCase() === 'ts-nocheck';
  });
}

function importLiterals(source) {
  const values = [];
  const visit = (node) => {
    if (
      (ts.isImportDeclaration(node) || ts.isExportDeclaration(node)) &&
      node.moduleSpecifier &&
      ts.isStringLiteralLike(node.moduleSpecifier)
    ) {
      values.push(node.moduleSpecifier.text);
    } else if (
      ts.isImportEqualsDeclaration(node) &&
      ts.isExternalModuleReference(node.moduleReference) &&
      node.moduleReference.expression &&
      ts.isStringLiteralLike(node.moduleReference.expression)
    ) {
      values.push(node.moduleReference.expression.text);
    } else if (
      ts.isCallExpression(node) &&
      (node.expression.kind === ts.SyntaxKind.ImportKeyword ||
        (ts.isIdentifier(node.expression) && node.expression.text === 'require')) &&
      node.arguments[0] &&
      ts.isStringLiteralLike(node.arguments[0])
    ) {
      values.push(node.arguments[0].text);
    }
    ts.forEachChild(node, visit);
  };
  visit(source);
  return values;
}

export async function verifyProject(directory = process.cwd()) {
  const errors = [];
  let root;
  let options;
  let parsedConfigs = 0;
  const files = new Set();
  const add = (file, rule, message) =>
    errors.push({
      path: root && path.isAbsolute(file) ? path.relative(root, file) || '.' : file,
      rule,
      message,
    });
  const result = () => ({
    ok: errors.length === 0,
    fileCount: files.size,
    configCount: parsedConfigs,
    errors,
  });
  try {
    root = fs.realpathSync(path.resolve(directory));
    if (!fs.statSync(root).isDirectory()) {
      throw new Error('Application root must be a directory.');
    }
    const packagePath = path.join(root, 'package.json');
    if (!fs.lstatSync(packagePath).isFile() || fs.realpathSync(packagePath) !== packagePath) {
      throw new Error('package.json must be a real application-owned file.');
    }
    options = metadata(JSON.parse(fs.readFileSync(packagePath, 'utf8')));
  } catch (error) {
    add(
      'package.json',
      'metadata',
      error instanceof Error ? error.message : 'Unable to read application metadata.',
    );
    return result();
  }
  if (!/^6\.0\.(?:[3-9]|[1-9]\d+)$/u.test(ts.version)) {
    add(
      'package.json',
      'toolchain',
      `Unsupported TypeScript ${ts.version}; expected >=6.0.3 <6.1.0.`,
    );
  }
  const eslintVersion = /^10\.(\d+)\.\d+$/u.exec(ESLint.version);
  if (!eslintVersion || Number(eslintVersion[1]) < 13) {
    add('package.json', 'toolchain', `Unsupported ESLint ${ESLint.version}; expected ^10.13.0.`);
  }
  const scan = (entry) => {
    const stat = fs.lstatSync(entry);
    if (stat.isSymbolicLink()) {
      add(entry, 'source-boundary', 'Symlinks are not allowed inside declared source directories.');
    } else if (stat.isDirectory()) {
      if (options.boundaries.forbiddenDirectories.includes(path.basename(entry))) {
        add(
          entry,
          'forbidden-directory',
          'Directory is forbidden by the explicit application policy.',
        );
      }
      for (const name of fs.readdirSync(entry).sort()) {
        scan(path.join(entry, name));
      }
    } else if (stat.isFile() && semanticExtension.test(entry)) {
      files.add(entry);
    }
  };
  for (const relative of options.sourceDirectories) {
    const full = path.resolve(root, relative);
    try {
      if (
        !within(root, full) ||
        fs.realpathSync(full) !== full ||
        !fs.statSync(full).isDirectory()
      ) {
        throw new Error('Expected a real directory inside the application.');
      }
      const before = files.size;
      scan(full);
      if (files.size === before && ![...files].some((file) => within(full, file))) {
        add(
          relative,
          'source-coverage',
          'Declared source directory has no JavaScript or TypeScript files.',
        );
      }
    } catch (error) {
      add(
        relative,
        'source-coverage',
        error instanceof Error ? error.message : 'Cannot read declared source.',
      );
    }
  }

  const checked = new Set();
  for (const relative of options.typecheckConfigs) {
    const full = path.resolve(root, relative);
    try {
      if (!within(root, full) || !fs.lstatSync(full).isFile() || fs.realpathSync(full) !== full) {
        throw new Error('Typecheck config must be a real file inside the application.');
      }
      const read = ts.readConfigFile(full, ts.sys.readFile);
      if (read.error) {
        throw new Error(ts.flattenDiagnosticMessageText(read.error.messageText, '\n'));
      }
      const config = ts.parseJsonConfigFileContent(
        read.config,
        ts.sys,
        path.dirname(full),
        undefined,
        full,
      );
      if (config.errors.length) {
        throw new Error(
          config.errors
            .map(
              (entry) =>
                `TS${entry.code}: ${ts.flattenDiagnosticMessageText(entry.messageText, '\n')}`,
            )
            .join('\n'),
        );
      }
      parsedConfigs += 1;
      if (!config.fileNames.length) {
        add(
          relative,
          'typecheck-coverage',
          'Typecheck config has no input files; list leaf configs explicitly.',
        );
      }
      if (
        config.options.noCheck ||
        config.options.strict !== true ||
        strictOptions.some((key) => config.options[key] === false)
      ) {
        add(
          relative,
          'strict-typecheck',
          'Typecheck configs must keep strict checking enabled and must not use noCheck.',
        );
      }
      const program = ts.createProgram({
        rootNames: config.fileNames,
        options: config.options,
        projectReferences: config.projectReferences,
      });
      for (const file of program.getSourceFiles()) {
        if (file.isDeclarationFile && config.options.skipLibCheck) {
          continue;
        }
        if (typescriptExtension.test(file.fileName) || config.options.checkJs === true) {
          checked.add(path.resolve(file.fileName));
        }
      }
    } catch (error) {
      add(
        relative,
        'typecheck-config',
        error instanceof Error ? error.message : 'Cannot read typecheck config.',
      );
    }
  }

  let eslint;
  try {
    eslint = new ESLint({ cwd: root });
  } catch (error) {
    add(
      'eslint.config.*',
      'lint-config',
      error instanceof Error ? error.message : 'Cannot load ESLint.',
    );
  }
  for (const file of [...files].sort()) {
    if (!checked.has(file)) {
      add(
        file,
        'typecheck-coverage',
        'Source is not covered by a declared typecheck config; JavaScript also requires checkJs.',
      );
    }
    try {
      const text = fs.readFileSync(file, 'utf8');
      if (uncheckedHeader(text)) {
        add(file, 'unchecked-source', 'Declared source must not disable checking with ts-nocheck.');
      }
      if (
        options.boundaries.forbidCrossAppImports &&
        path.basename(path.dirname(root)) === 'apps'
      ) {
        const apps = path.dirname(root);
        const source = ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true);
        for (const specifier of importLiterals(source)) {
          if (specifier.startsWith('.') || path.isAbsolute(specifier)) {
            const target = path.resolve(path.dirname(file), specifier);
            if (within(apps, target) && !within(root, target)) {
              add(
                file,
                'cross-app-import',
                `Import ${JSON.stringify(specifier)} crosses an application source boundary.`,
              );
            }
          }
        }
      }
      if (eslint) {
        if (await eslint.isPathIgnored(file)) {
          add(file, 'lint-coverage', 'Declared source is ignored by ESLint.');
          continue;
        }
        const config = await eslint.calculateConfigForFile(file);
        if (!config) {
          add(file, 'lint-coverage', 'No ESLint configuration applies to declared source.');
          continue;
        }
        if (config.linterOptions?.noInlineConfig !== true) {
          add(
            file,
            'lint-policy',
            'Declared source must use noInlineConfig so local disable comments cannot weaken gates.',
          );
        }
        if (typescriptExtension.test(file)) {
          const parser = config.languageOptions?.parser;
          const parserOptions = config.languageOptions?.parserOptions;
          if (
            parser?.meta?.name !== 'typescript-eslint/parser' ||
            !(parserOptions?.projectService || parserOptions?.project)
          ) {
            add(
              file,
              'typed-lint-coverage',
              'Declared TypeScript must use the type-aware TypeScript parser and project configuration.',
            );
          }
          for (const rule of [
            '@typescript-eslint/no-floating-promises',
            '@typescript-eslint/no-unsafe-assignment',
          ]) {
            if (!enabledError(config.rules?.[rule])) {
              add(
                file,
                'typed-lint-coverage',
                `Mandatory type-aware rule ${rule} must remain an error.`,
              );
            }
          }
        }
      }
    } catch (error) {
      add(
        file,
        'source-config',
        error instanceof Error ? error.message : 'Cannot verify source configuration.',
      );
    }
  }
  return result();
}
