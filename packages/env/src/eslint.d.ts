import type { Linter } from 'eslint';
export function nodeConfig(appRoot: string): Linter.Config[];
export function browserConfig(appRoot: string): Linter.Config[];
export function desktopConfig(appRoot: string): Linter.Config[];
