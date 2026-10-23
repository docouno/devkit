import { z } from 'zod';

const responseSchema = z.object({ message: z.string().min(1) });

export async function fetchGreeting(
  name: string,
  options: { fetcher?: typeof fetch; signal?: AbortSignal } = {},
): Promise<{ message: string }> {
  const query = new URLSearchParams({ name });
  const response = await (options.fetcher ?? fetch)(`/api/greeting?${query.toString()}`, {
    headers: { accept: 'application/json' },
    ...(options.signal ? { signal: options.signal } : {}),
  });
  if (!response.ok) {
    throw new Error(`API request failed (${response.status}).`);
  }
  let payload: unknown;
  try {
    payload = await response.json();
  } catch {
    throw new Error('API returned invalid JSON.');
  }
  const parsed = responseSchema.safeParse(payload);
  if (!parsed.success) {
    throw new Error('API returned an invalid greeting.');
  }
  return parsed.data;
}
