export type Config = { timeoutMs: number; retries: number };

export const defaults: Config = { timeoutMs: 10000, retries: 2 };

export function withTimeout(config: Config, timeoutMs: number): Config {
  return { ...config, timeoutMs };
}
