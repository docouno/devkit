import { spawn } from 'node:child_process';

export interface CommandResult {
  stdout: string;
  stderr: string;
}

export async function run(
  executable: string,
  args: readonly string[],
  options: { cwd: string; env?: NodeJS.ProcessEnv; quiet?: boolean },
): Promise<CommandResult> {
  return await new Promise((resolve, reject) => {
    const child = spawn(executable, args, {
      cwd: options.cwd,
      env: options.env ?? process.env,
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let stdout = '';
    let stderr = '';
    child.stdout.setEncoding('utf8').on('data', (chunk: string) => {
      stdout += chunk;
      if (!options.quiet) {
        process.stdout.write(chunk);
      }
    });
    child.stderr.setEncoding('utf8').on('data', (chunk: string) => {
      stderr += chunk;
      if (!options.quiet) {
        process.stderr.write(chunk);
      }
    });
    child.once('error', reject);
    child.once('close', (code, signal) => {
      if (code === 0 && signal === null) {
        resolve({ stdout, stderr });
      } else {
        reject(
          new Error(`${executable} failed (${signal ?? code ?? 'unknown'}): ${stderr.trim()}`),
        );
      }
    });
  });
}

export function npmEnvironment(cache: string, registry?: string): NodeJS.ProcessEnv {
  const inherited = Object.fromEntries(
    Object.entries(process.env).filter(
      ([key]) =>
        !/^npm_/i.test(key) && !['NODE_OPTIONS', 'NODE_PATH', 'NODE_TEST_CONTEXT'].includes(key),
    ),
  );
  return {
    ...inherited,
    npm_config_cache: cache,
    npm_config_userconfig: '/dev/null',
    npm_config_update_notifier: 'false',
    npm_config_fund: 'false',
    npm_config_audit: 'false',
    ...(registry === undefined ? {} : { npm_config_registry: registry }),
  };
}
