import type { Linter } from 'eslint';

export interface DevkitConfigOptions {
  readonly appRoot?: string | undefined;
  readonly root?: string | undefined;
}

export function nodeConfig(appRoot: string): Linter.Config[];
export function browserConfig(appRoot: string): Linter.Config[];
export function reactLibraryConfig(appRoot: string | DevkitConfigOptions): Linter.Config[];
export function desktopConfig(appRoot: string): Linter.Config[];
