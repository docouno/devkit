export type PackageMode = 'local' | 'registry';
export type AppKind = 'node' | 'rust';

export interface FileIdentity {
  sha256: string;
  mode: number;
}

export interface Release {
  schemaVersion: 1;
  version: string;
  files: Record<string, FileIdentity>;
  environment: {
    name: string;
    version: string;
    archive: string;
    integrity: string;
  };
  rustVersion: string;
}

export interface AppState {
  kind: AppKind;
  template: string;
  toolDependencies: Record<string, string>;
}

export interface ProjectState {
  schemaVersion: 1;
  version: string;
  releaseIdentity: string;
  generation: string;
  packageMode: PackageMode;
  managed: Record<string, FileIdentity>;
  apps: Record<string, AppState>;
}

export interface PlannedFile {
  path: string;
  before: FileIdentity | null;
  after: FileIdentity | null;
  backup: string | null;
  staged: string | null;
}

export interface UpdateJournal {
  schemaVersion: 1;
  stage: string;
  files: PlannedFile[];
}
