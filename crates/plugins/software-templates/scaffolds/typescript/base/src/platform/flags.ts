// This service's feature flags and runtime configuration, read from DOC. Everything that applies to
// it is read in one call and kept here; the call carries an ETag, so a poll that finds nothing new
// costs a 304 and nothing else.
//
// Every read names the value to fall back to, so a service whose flags cannot be reached keeps
// running on its own defaults rather than stopping.

import type { Config } from "./config.js";

interface Answer {
  flags?: Record<string, unknown>;
  config?: Record<string, unknown>;
  version?: string;
  refresh_seconds?: number;
}

export class Flags {
  private values: Record<string, unknown> = {};
  private version = "";
  private timer: NodeJS.Timeout | undefined;

  private constructor(private readonly config: Config) {}

  /** Reads them once, then keeps them up to date until `stop` is called. */
  static async start(config: Config): Promise<Flags> {
    const flags = new Flags(config);
    await flags.read();
    flags.schedule(config.flagsPollSeconds);
    return flags;
  }

  stop(): void {
    if (this.timer !== undefined) clearTimeout(this.timer);
  }

  private schedule(seconds: number): void {
    this.timer = setTimeout(() => {
      void this.read().then(() => this.schedule(seconds));
    }, seconds * 1000);
    this.timer.unref();
  }

  /** One read. A 304 means nothing has changed, and costs nothing else. */
  async read(): Promise<void> {
    const url = new URL(this.config.flagsUrl);
    url.searchParams.set("service", this.config.service);
    url.searchParams.set("environment", this.config.environment);

    const headers: Record<string, string> = { accept: "application/json" };
    if (this.version !== "") headers["if-none-match"] = `"${this.version}"`;
    if (this.config.flagsToken !== undefined) {
      headers.authorization = `Bearer ${this.config.flagsToken}`;
    }

    try {
      const answer = await fetch(url, {
        headers,
        signal: AbortSignal.timeout(5000),
      });
      if (answer.status === 304) return;
      if (!answer.ok) throw new Error(`the platform answered ${answer.status}`);
      const read = (await answer.json()) as Answer;
      this.values = { ...(read.flags ?? {}), ...(read.config ?? {}) };
      this.version = read.version ?? "";
    } catch (problem) {
      console.warn("the flags could not be read; running on this service's defaults", problem);
    }
  }

  /** A switch: on, off, or the fallback when the platform holds nothing for this service. */
  boolean(key: string, fallback: boolean): boolean {
    const value = this.values[key];
    return typeof value === "boolean" ? value : fallback;
  }

  /** A value read while the service runs, such as a message or a mode. */
  string(key: string, fallback: string): string {
    const value = this.values[key];
    return typeof value === "string" ? value : fallback;
  }

  /** A number read while the service runs, such as a limit or a timeout. */
  number(key: string, fallback: number): number {
    const value = this.values[key];
    return typeof value === "number" ? value : fallback;
  }

  /** A structured value, or the fallback. */
  json<T>(key: string, fallback: T): T {
    const value = this.values[key];
    return value === undefined ? fallback : (value as T);
  }
}
