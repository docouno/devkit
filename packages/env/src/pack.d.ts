export interface PackageReceipt {
  readonly name: string;
  readonly version: string;
  readonly filename: string;
  readonly sha256: string;
  readonly shasum: string;
  readonly integrity: string;
  readonly size: number;
  readonly entryCount: number;
  readonly publintPassed: boolean;
  readonly attwProfile: 'esm-only';
  readonly attwPassed: boolean;
}

export interface VerifyPackableOptions {
  readonly outputDir?: string | undefined;
  readonly allowPrivateLocalDeps?: boolean | undefined;
}

export interface VerifyPackableResult {
  readonly ok: boolean;
  readonly errors: string[];
  readonly tarballPath: string | null;
  readonly files: string[];
  readonly receipt: PackageReceipt;
}

export interface PackComponentOptions {
  readonly outputDir?: string | undefined;
}

export interface PackComponentResult {
  readonly tarballPath: string;
  readonly receiptPath: string;
  readonly receipt: PackageReceipt;
}

export function verifyPackableComponent(
  componentDir: string,
  options?: VerifyPackableOptions,
): Promise<VerifyPackableResult>;

export function packComponent(
  componentDir: string,
  options?: PackComponentOptions,
): Promise<PackComponentResult>;
