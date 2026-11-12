export interface DevkitViteUserConfig {
  readonly root?: string | undefined;
  readonly server?:
    | {
        readonly fs?:
          | {
              readonly allow?: readonly string[] | undefined;
            }
          | undefined;
      }
    | undefined;
  readonly optimizeDeps?:
    | {
        readonly exclude?: readonly string[] | undefined;
      }
    | undefined;
}

export interface DevkitViteConfigEnv {
  readonly command: 'build' | 'serve';
  readonly mode: string;
}

export interface DevkitViteConfigPatch {
  readonly server: {
    readonly fs: {
      readonly allow: string[];
    };
  };
  readonly optimizeDeps: {
    readonly exclude: string[];
  };
}

export interface DevkitVitePlugin {
  readonly name: string;
  readonly enforce: 'pre';
  readonly apply: 'serve';
  config(config: DevkitViteUserConfig, env: DevkitViteConfigEnv): DevkitViteConfigPatch | undefined;
  resolveId(source: string, importer?: string | undefined): string | null;
}

export function devkitWorkspaceSource(projectDir?: string): DevkitVitePlugin;
