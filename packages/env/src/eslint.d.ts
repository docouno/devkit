import type { Linter } from 'eslint';

export interface DevkitConfigOptions {
  readonly appRoot?: string | undefined;
  readonly root?: string | undefined;
}

export function nodeConfig(appRoot: string | DevkitConfigOptions): Linter.Config[];
export function browserConfig(appRoot: string | DevkitConfigOptions): Linter.Config[];
export function reactLibraryConfig(appRoot: string | DevkitConfigOptions): Linter.Config[];
export function desktopConfig(appRoot: string | DevkitConfigOptions): Linter.Config[];
