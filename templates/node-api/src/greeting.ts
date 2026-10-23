import { z } from 'zod';

const nameSchema = z.string().trim().max(80).default('world');

export function greeting(name: unknown): { message: string } {
  const parsed = nameSchema.parse(name);
  return { message: `Hello, ${parsed || 'world'}!` };
}
