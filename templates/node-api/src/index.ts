import { env } from 'node:process';
import { z } from 'zod';
import { createApplication } from './http.ts';

const { HOST: host, PORT: port } = z
  .object({
    HOST: z.string().min(1).default('0.0.0.0'),
    PORT: z
      .string()
      .regex(/^\d+$/u)
      .default('8080')
      .transform(Number)
      .pipe(z.number().int().min(0).max(65_535)),
  })
  .parse(env);
const server = createApplication();
server.on('error', (error: Error) => {
  console.error(`Server error: ${error.message}`);
  process.exitCode = 1;
});
server.listen(port, host, () => {
  const address = server.address();
  if (address && typeof address !== 'string') {
    console.log(
      `Listening on ${address.address.includes(':') ? `[${address.address}]` : address.address}:${address.port}`,
    );
  }
});
for (const signal of ['SIGINT', 'SIGTERM'] as const) {
  process.once(signal, () => {
    server.close((error) => {
      if (error) {
        console.error(`Unable to close the server: ${error.message}`);
        process.exitCode = 1;
      }
    });
  });
}
