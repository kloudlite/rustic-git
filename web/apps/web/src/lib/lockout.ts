/** Failed attempts per key; past `max` inside one window the key is refused until the window
 *  from the last failure has passed. A correct guess while locked is still refused — otherwise
 *  the lock only slows the guessing, it does not stop it.
 *
 *  ponytail: per process, so a deployment with N web replicas allows N× the guesses. The
 *  shared preview password is a no-mail-yet convenience; move the count to the api if it ever
 *  outlives that. */
export class Lockout {
  private readonly seen = new Map<string, { fails: number; last: number }>();

  constructor(
    private readonly max = 5,
    private readonly windowMs = 60_000,
  ) {}

  locked(key: string, now = Date.now()): boolean {
    const s = this.seen.get(key);
    if (!s) return false;
    if (now - s.last >= this.windowMs) {
      this.seen.delete(key);
      return false;
    }
    return s.fails >= this.max;
  }

  fail(key: string, now = Date.now()) {
    const s = this.seen.get(key);
    const fails = s && now - s.last < this.windowMs ? s.fails + 1 : 1;
    this.seen.set(key, { fails, last: now });
  }

  clear(key: string) {
    this.seen.delete(key);
  }
}
