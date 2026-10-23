import fs from 'node:fs';
import path from 'node:path';
import { builtinModules } from 'node:module';
import js from '@eslint/js';
import { defineConfig } from 'eslint/config';
import reactHooks from 'eslint-plugin-react-hooks';
import reactRefresh from 'eslint-plugin-react-refresh';
import globals from 'globals';
import tseslint from 'typescript-eslint';
import { relativeList } from './paths.js';

const sourceFiles = ['**/*.{js,jsx,mjs,cjs,ts,tsx,mts,cts}'];
const typedFiles = ['**/*.{ts,tsx,mts,cts}'];

function config(appRoot, browser) {
  if (typeof appRoot !== 'string' || !path.isAbsolute(appRoot)) {
    throw new TypeError('Pass the absolute application directory to the Devkit config factory.');
  }
  const root = fs.realpathSync(appRoot);
  if (!fs.statSync(root).isDirectory()) {
    throw new TypeError('Application root must be a directory.');
  }
  const packageJson = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'));
  const configs = relativeList(
    packageJson.devkit?.typecheckConfigs,
    ['tsconfig.json'],
    'typecheckConfigs',
  );
  const parserOptions =
    configs.length === 1 && configs[0] === 'tsconfig.json'
      ? { projectService: true, tsconfigRootDir: root }
      : { project: configs.map((file) => path.resolve(root, file)), tsconfigRootDir: root };
  const definitions = [
    {
      name: 'devkit/generated-output',
      ignores: ['node_modules/**', 'dist/**', 'coverage/**', '.devkit/**'],
    },
    {
      name: 'devkit/javascript',
      files: sourceFiles,
      extends: [js.configs.recommended],
      languageOptions: {
        ecmaVersion: 'latest',
        sourceType: 'module',
        globals: browser ? globals.browser : globals.node,
        parserOptions: { ecmaFeatures: { jsx: browser } },
      },
      linterOptions: { noInlineConfig: true, reportUnusedDisableDirectives: 'error' },
      rules: {
        curly: ['error', 'all'],
        eqeqeq: ['error', 'always'],
        'no-var': 'error',
        'prefer-const': 'error',
      },
    },
    {
      name: 'devkit/typescript',
      files: typedFiles,
      extends: [tseslint.configs.recommendedTypeChecked],
      languageOptions: { parserOptions },
      rules: {
        '@typescript-eslint/consistent-type-imports': ['error', { prefer: 'type-imports' }],
        '@typescript-eslint/no-floating-promises': ['error', { ignoreVoid: false }],
        '@typescript-eslint/no-misused-promises': 'error',
      },
    },
  ];
  if (browser) {
    definitions.push({
      name: 'devkit/react',
      files: ['**/*.{tsx,jsx}'],
      plugins: { 'react-hooks': reactHooks, 'react-refresh': reactRefresh },
      rules: {
        ...reactHooks.configs.flat.recommended.rules,
        'react-refresh/only-export-components': ['error', { allowConstantExport: true }],
      },
    });
    definitions.push({
      name: 'devkit/react-tests',
      files: ['**/*.{test,spec}.{tsx,jsx}'],
      rules: { 'react-refresh/only-export-components': 'off' },
    });
  }
  return defineConfig(definitions);
}

export const nodeConfig = (appRoot) => config(appRoot, false);
export const browserConfig = (appRoot) => config(appRoot, true);

/** Electron's renderer uses browser globals even though its main process uses Node. */
export const desktopConfig = (appRoot) => {
  const definitions = config(appRoot, false);
  const browser = config(appRoot, true);
  const renderer = ['src/renderer/**/*.{js,jsx,mjs,cjs,ts,tsx,mts,cts}'];
  const forbidden = [
    ...new Set([
      ...builtinModules,
      ...builtinModules.map((name) => (name.startsWith('node:') ? name : `node:${name}`)),
      'electron',
    ]),
  ];
  const nativeModules = new Set(forbidden);
  const desktopPlugin = {
    rules: {
      'no-native-dynamic-import': {
        meta: {
          type: 'problem',
          schema: [],
          messages: { native: 'Renderer code cannot import native module {{name}}.' },
        },
        create(context) {
          return {
            ImportExpression(node) {
              const source = node.source;
              const name =
                source.type === 'Literal'
                  ? source.value
                  : source.type === 'TemplateLiteral' && source.expressions.length === 0
                    ? source.quasis[0].value.cooked
                    : undefined;
              if (
                typeof name === 'string' &&
                (nativeModules.has(name) || name.startsWith('electron/'))
              ) {
                context.report({ node, messageId: 'native', data: { name } });
              }
            },
          };
        },
      },
    },
  };
  definitions.push({
    name: 'devkit/electron-renderer',
    files: renderer,
    plugins: { 'devkit-desktop': desktopPlugin },
    languageOptions: {
      globals: {
        ...Object.fromEntries(Object.keys(globals.node).map((name) => [name, 'off'])),
        ...globals.browser,
      },
      parserOptions: { ecmaFeatures: { jsx: true } },
    },
    rules: {
      'no-restricted-globals': [
        'error',
        'process',
        'Buffer',
        'require',
        '__dirname',
        '__filename',
        'global',
        'module',
        'exports',
      ],
      'no-restricted-imports': ['error', { paths: forbidden, patterns: ['electron/*'] }],
      'no-restricted-modules': ['error', { paths: forbidden, patterns: ['electron/*'] }],
      'devkit-desktop/no-native-dynamic-import': 'error',
    },
  });
  for (const definition of browser.filter((entry) =>
    ['devkit/react', 'devkit/react-tests'].includes(entry.name),
  )) {
    definitions.push({
      ...definition,
      name: `${definition.name}-electron-renderer`,
      files:
        definition.name === 'devkit/react'
          ? ['src/renderer/**/*.{tsx,jsx}']
          : ['src/renderer/**/*.{test,spec}.{tsx,jsx}'],
    });
  }
  return defineConfig(definitions);
};
