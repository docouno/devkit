import { env } from 'node:process';
import { z } from 'zod';
import { createApplication } from './http.ts';

const port = z.coerce.number().int().min(0).max(65_535).default(8080).parse(env.PORT);
const server = createApplication();
server.on('error', (error: Error) => {
  console.error(`Server error: ${error.message}`);
  process.exitCode = 1;
});
server.listen(port, '0.0.0.0', () => {
  const address = server.address();
  if (address && typeof address !== 'string') {
    console.log(`Listening on 0.0.0.0:${address.port}`);
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
