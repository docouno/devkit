#!/usr/bin/env node
import path from 'node:path';
import { verifyProject } from '../src/verify.js';

const args = process.argv.slice(2);
let root = process.cwd();
let json = false;
try {
  for (let index = 0; index < args.length; index += 1) {
    if (args[index] === '--root') {
      const value = args[index + 1];
      if (!value || value.startsWith('--')) {
        throw new Error('--root requires an application path.');
      }
      root = path.resolve(value);
      index += 1;
    } else if (args[index] === '--json') {
      json = true;
    } else if (args[index] === '--help') {
      console.log('Usage: devkit-verify [--root <application>] [--json]');
      process.exit(0);
    } else {
      throw new Error(`Unknown argument: ${args[index]}`);
    }
  }
  const result = await verifyProject(root);
  if (json) {
    console.log(JSON.stringify(result));
  } else if (result.ok) {
    console.log(
      `Verified ${result.fileCount} declared source files across ${result.configCount} TypeScript configs.`,
    );
  } else {
    for (const error of result.errors) {
      console.error(`${error.path}: [${error.rule}] ${error.message}`);
    }
  }
  process.exitCode = result.ok ? 0 : 1;
} catch (error) {
  console.error(error instanceof Error ? error.message : 'Unable to verify the application.');
  process.exitCode = 1;
}
