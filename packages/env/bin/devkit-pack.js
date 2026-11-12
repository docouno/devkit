#!/usr/bin/env node
import path from 'node:path';
import { packComponent } from '../src/pack.js';

const args = process.argv.slice(2);
let root = process.cwd();
let outputDir;
let json = false;
try {
  for (let index = 0; index < args.length; index += 1) {
    if (args[index] === '--root') {
      const value = args[index + 1];
      if (!value || value.startsWith('--')) {
        throw new Error('--root requires a package directory path.');
      }
      root = path.resolve(value);
      index += 1;
    } else if (args[index] === '--output') {
      const value = args[index + 1];
      if (!value || value.startsWith('--')) {
        throw new Error('--output requires a destination directory path.');
      }
      outputDir = path.resolve(value);
      index += 1;
    } else if (args[index] === '--json') {
      json = true;
    } else if (args[index] === '--help') {
      console.log('Usage: devkit-pack [--root <package-dir>] [--output <release-dir>] [--json]');
      process.exit(0);
    } else {
      throw new Error(`Unknown argument: ${args[index]}`);
    }
  }
  const packed = await packComponent(root, outputDir ? { outputDir } : {});
  if (json) {
    console.log(JSON.stringify(packed.receipt));
  } else {
    console.log(
      `Packed ${packed.receipt.name}@${packed.receipt.version} -> ${path.relative(root, packed.tarballPath) || packed.receipt.filename} (${packed.receipt.size} bytes, publint+attw passed).`,
    );
  }
} catch (error) {
  console.error(error instanceof Error ? error.message : 'Unable to package component.');
  process.exitCode = 1;
}
