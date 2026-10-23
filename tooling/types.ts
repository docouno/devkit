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

export interface ProjectStateV1 {
  schemaVersion: 1;
  version: string;
  releaseIdentity: string;
  generation: string;
  packageMode: PackageMode;
  managed: Record<string, FileIdentity>;
  apps: Record<string, AppState>;
}

export type ComponentPath = `apps/${string}` | `packages/${string}`;
export type ComponentRole = 'app' | 'library';
export type ToolingProfile = 'node' | 'web' | 'electron' | 'react-library' | 'rust';
export interface ComponentState {
  role: ComponentRole;
  toolchain: AppKind;
  toolingProfile: ToolingProfile;
  origin: { template: string; version: string };
  toolDependencies: Record<string, string>;
  dependencies: ComponentPath[];
  development?: {
    owner: 'managed' | 'project';
    kind: 'compose' | 'desktop';
    service?: string;
    api?: ComponentPath;
  };
}
export interface ProjectState {
  schemaVersion: 2;
  version: string;
  releaseIdentity: string;
  generation: string;
  packageMode: PackageMode;
  managed: Record<string, FileIdentity>;
  components: Record<ComponentPath, ComponentState>;
  /** Historical migration evidence; never used to weaken ordinary native admission. */
  legacyIdentityNormalization?: Record<string, unknown>;
}
export type AnyProjectState = ProjectState | ProjectStateV1;
export interface InputIdentitySet {
  schemaVersion: 1;
  files: Record<string, FileIdentity | null>;
  directories: string[];
  digest: string;
}
export interface ComposeInventory {
  schemaVersion: 1;
  protocol: 'compose-local-v1';
  entrypoints: string[];
  inputs: InputIdentitySet;
  services: string[];
}

export interface PlannedFile {
  path: string;
  before: FileIdentity | null;
  after: FileIdentity | null;
  backup: string | null;
  staged: string | null;
}

export interface UpdateJournal {
  schemaVersion: 1 | 2;
  stage: string;
  files: PlannedFile[];
  createdDirectories?: string[];
  createdParents?: string[];
}
