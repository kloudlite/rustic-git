import type { AuthState } from "../auth/controller";

/** What the login screen shows for a state: kept pure so the transitions are tested without a DOM. */
export function screen(s: AuthState): { title: string; body?: string; code?: string; url?: string; actions: ("signIn" | "cancel" | "openBrowser" | "retry" | "address")[]; busy: boolean } {
  switch (s.phase) {
    case "starting":
    case "ready":
      return { title: "Kloudlite", actions: [], busy: true };
    case "signed-out":
      return { title: "Sign in to Kloudlite", ...(s.reason ? { body: s.reason } : {}), actions: ["signIn", "address"], busy: false };
    case "waiting":
      return { title: "Confirm this code in your browser", body: "waiting for approval", code: s.code, url: s.url, actions: ["openBrowser", "cancel"], busy: true };
    case "connecting":
      return { title: "Connecting", body: s.step, actions: [], busy: true };
    case "error":
      return { title: "Can't continue", body: s.message, actions: s.retry === "none" ? [] : ["retry"], busy: false };
  }
}
