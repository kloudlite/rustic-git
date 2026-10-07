import { useEffect, useRef, useState } from "react";
import { spawn } from "node:child_process";
import { useKeyboard } from "@opentui/react";
import { TextAttributes } from "@opentui/core";
import type { AuthEvent, AuthPrompt } from "@kloudlite-tui/agent";
import { loginProvider } from "@kloudlite-tui/agent";
import { theme } from "../theme.ts";
import { Input } from "./Input.tsx";

type Pending = {
  prompt: AuthPrompt;
  resolve: (v: string) => void;
  reject: (e: Error) => void;
};

/**
 * Drives a pi-ai login flow: renders notify events (auth URLs, device codes,
 * progress) and answers prompts (text/secret/select/manual_code). Esc cancels.
 */
export function Login({
  provider,
  type,
  onDone,
}: {
  provider: string;
  type: "oauth" | "api_key";
  onDone: (ok: boolean) => void;
}) {
  const [events, setEvents] = useState<AuthEvent[]>([]);
  const [pending, setPending] = useState<Pending | null>(null);
  const [value, setValue] = useState("");
  const [sel, setSel] = useState(0);
  const abort = useRef(new AbortController());

  useEffect(() => {
    loginProvider(provider, type, {
      signal: abort.current.signal,
      notify: (event) => {
        setEvents((prev) => [...prev, event]);
        if (event.type === "auth_url") {
          // best effort: open the browser (macOS/Linux). A headless box (the
          // bench) has no xdg-open: Bun throws on spawn, node emits "error";
          // either way the URL on screen is the way in.
          try {
            spawn(process.platform === "darwin" ? "open" : "xdg-open", [event.url], {
              stdio: "ignore",
              detached: true,
            })
              .on("error", () => {})
              .unref();
          } catch {}
        }
      },
      prompt: (prompt) =>
        new Promise<string>((resolve, reject) => {
          setValue("");
          setSel(0);
          setPending({ prompt, resolve, reject });
          // pi cancels a single prompt when something else answers the step —
          // a manual_code raced against the OAuth callback server. Drop the
          // input instead of leaving the user typing into a resolved prompt.
          prompt.signal?.addEventListener(
            "abort",
            () => {
              setPending((cur) => (cur?.prompt === prompt ? null : cur));
              reject(new Error("cancelled"));
            },
            { once: true },
          );
        }),
    })
      .then(() => onDone(true))
      .catch(() => onDone(false));
    return () => abort.current.abort();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const select = pending?.prompt.type === "select" ? pending.prompt : null;

  useKeyboard((key) => {
    if (key.name === "escape") {
      abort.current.abort();
      pending?.reject(new Error("cancelled"));
      onDone(false);
      return;
    }
    if (select) {
      const n = select.options.length;
      if (key.name === "down") setSel((i) => (i + 1) % n);
      if (key.name === "up") setSel((i) => (i - 1 + n) % n);
      if (key.name === "return") {
        pending!.resolve(select.options[sel]!.id);
        setPending(null);
      }
    }
  });

  function submit(text: string) {
    if (!pending) return;
    pending.resolve(text);
    setPending(null);
    setValue("");
  }

  return (
    <box flexDirection="column" flexGrow={1} paddingLeft={1} paddingRight={1} paddingTop={1} gap={1}>
      <text fg={theme.fg}>
        <b>Login</b> · <span fg={theme.accent}>{provider}</span>{" "}
        <span fg={theme.muted}>{type === "oauth" ? "OAuth" : "API key"}</span>
      </text>

      {events.map((event, i) => (
        <box key={i} flexDirection="column">
          {event.type === "info" && (
            <>
              <text fg={theme.muted}>{event.message}</text>
              {event.links?.map((link) => (
                <text key={link.url} fg={theme.accent} attributes={TextAttributes.UNDERLINE}>
                  {link.label ? `${link.label}: ${link.url}` : link.url}
                </text>
              ))}
            </>
          )}
          {event.type === "progress" && <text fg={theme.muted}>{event.message}</text>}
          {event.type === "auth_url" && (
            <>
              <text fg={theme.muted}>
                {event.instructions ?? "Open this URL to authorize:"}
              </text>
              <text fg={theme.accent} attributes={TextAttributes.UNDERLINE}>
                {event.url}
              </text>
            </>
          )}
          {event.type === "device_code" && (
            <text fg={theme.fg}>
              Enter code <span fg={theme.accent}><b>{event.userCode}</b></span>{" "}
              <span fg={theme.muted}>at {event.verificationUri}</span>
            </text>
          )}
        </box>
      ))}

      {pending && !select && (
        <box flexDirection="column">
          <text fg={theme.fg}>{pending.prompt.message}</text>
          <box flexDirection="row">
            <text fg={theme.accent}>› </text>
            <Input
              value={value}
              onChange={setValue}
              onSubmit={submit}
              placeholder={
                "placeholder" in pending.prompt ? (pending.prompt.placeholder ?? "") : ""
              }
              showCursor
              mask={pending.prompt.type === "secret"}
            />
          </box>
        </box>
      )}

      {select && (
        <box flexDirection="column">
          <text fg={theme.fg}>{select.message}</text>
          {select.options.map((opt, i) => (
            <box
              key={opt.id}
              flexDirection="row"
              paddingLeft={1}
              paddingRight={1}
              backgroundColor={i === sel ? theme.selection : undefined}
            >
              <text fg={i === sel ? theme.bg : theme.fg}>{opt.label}</text>
              {opt.description && (
                <text fg={i === sel ? theme.bg : theme.muted}> {opt.description}</text>
              )}
            </box>
          ))}
        </box>
      )}

      {!pending && (
        <text fg={theme.muted}>
          waiting… esc to cancel
        </text>
      )}
    </box>
  );
}
