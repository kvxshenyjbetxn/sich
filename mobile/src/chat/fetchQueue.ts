/** Runs async fetches with at most `max` in flight, de-duplicating by key and caching results. */
export class FetchQueue<T> {
  private running = 0;
  private waiting: Array<() => void> = [];
  private inflight = new Map<string, Promise<T>>();
  private cache = new Map<string, T>();

  constructor(
    private readonly max = 2,
    private readonly cacheMax = 60,
  ) {}

  get active(): number {
    return this.running;
  }

  cached(key: string): T | undefined {
    return this.cache.get(key);
  }

  load(key: string, fn: () => Promise<T>): Promise<T> {
    if (this.cache.has(key)) return Promise.resolve(this.cache.get(key)!);
    const existing = this.inflight.get(key);
    if (existing) return existing;
    const p = this.acquire()
      .then(() => fn())
      .then(
        (v) => {
          this.remember(key, v);
          return v;
        },
        (e) => {
          throw e;
        },
      )
      .finally(() => {
        this.inflight.delete(key);
        this.release();
      });
    this.inflight.set(key, p);
    return p;
  }

  private remember(key: string, v: T) {
    this.cache.set(key, v);
    if (this.cache.size > this.cacheMax) {
      const oldest = this.cache.keys().next().value;
      if (oldest !== undefined) this.cache.delete(oldest);
    }
  }

  private acquire(): Promise<void> {
    if (this.running < this.max) {
      this.running++;
      return Promise.resolve();
    }
    return new Promise((resolve) => this.waiting.push(() => resolve()));
  }

  private release() {
    const next = this.waiting.shift();
    if (next) next(); // hand the slot over: `running` stays the same
    else this.running--;
  }
}
