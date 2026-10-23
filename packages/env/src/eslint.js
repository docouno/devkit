import fs from 'node:fs';
import path from 'node:path';
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
