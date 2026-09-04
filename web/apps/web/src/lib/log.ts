/** The web tier's logger, emitting the same line shape the Rust tiers do.
 *
 *  One JSON object per line on stderr with `timestamp`, `level`, `target`, `message` and the
 *  call-site fields flattened next to them — `crates/core/src/log.rs` with `json()` +
 *  `flatten_event(true)` — so the collectors that already parse the Rust pods' output turn the
 *  web's lines into the same columns rather than a second format nobody queries.
 *
 *  The message is the EVENT (`subject.verb`), everything specific is a field, per
 *  docs/superpowers/reviews/logging-events.md. Never interpolate an id into the message.
 *
 *  stdout is left alone: Next writes its own startup and request noise there, and a log line
 *  interleaved into it is a line the parser drops.
 */

type Fields = Record<string, unknown>;

function emit(level: "INFO" | "WARN" | "ERROR", target: string, message: string, fields?: Fields) {
  let line: string;
  try {
    line = JSON.stringify({ timestamp: new Date().toISOString(), level, target, message, ...fields });
  } catch {
    // A field that cannot be serialized (a cycle, a BigInt) must not lose the event itself.
    line = JSON.stringify({ timestamp: new Date().toISOString(), level, target, message });
  }
  // Client components share this module; a browser has no `process.stderr`, and there the
  // developer console is the only sink there is.
  const stderr = globalThis.process?.stderr;
  if (stderr) stderr.write(`${line}\n`);
  else console.error(line);
}

/** A logger bound to one module. `target` is the module path, as `tracing` derives it. */
export function log(target: string) {
  return {
    info: (message: string, fields?: Fields) => emit("INFO", target, message, fields),
    warn: (message: string, fields?: Fields) => emit("WARN", target, message, fields),
    error: (message: string, fields?: Fields) => emit("ERROR", target, message, fields),
  };
}

/** An `unknown` caught value as one field. `Error` prints its message, never its stack: a stack
 *  is where a token or a path leaks into a log line. */
export function reason(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
