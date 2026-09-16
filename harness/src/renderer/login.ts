import type { AuthState } from "../auth/controller";

export type LoginAction = "signIn" | "cancel" | "openBrowser" | "retry" | "address" | "signOut";
export type TeamChoice = { slug: string; label: string; disabled: boolean; note?: string };

/** What the login screen shows for a state: kept pure so the transitions are tested without a DOM. */
export function screen(s: AuthState): { title: string; body?: string; code?: string; url?: string; teams?: TeamChoice[]; actions: LoginAction[]; busy: boolean } {
  switch (s.phase) {
    case "starting":
    case "ready":
      return { title: "Kloudlite", actions: [], busy: true };
    case "signed-out":
      return { title: "Sign in to Kloudlite", body: s.reason ?? "Use your browser to sign in. The app never sees your password.", actions: ["signIn", "address"], busy: false };
    case "waiting":
      return { title: "Confirm this code in your browser", body: "Waiting for your approval…", code: s.code, url: s.url, actions: ["openBrowser", "cancel"], busy: true };
    case "choose-team": {
      const body = s.reason ?? (s.teams.length ? "a bench belongs to a team" : "you are not in any team yet — ask an admin to add you");
      const teams = s.teams.map((t) => ({ slug: t.slug, label: t.name || t.slug, disabled: !t.region, ...(t.region ? {} : { note: "no region yet — ask an admin" }) }));
      return { title: "Choose a team", body, teams, actions: ["signOut"], busy: false };
    }
    case "connecting":
      return { title: "Connecting", body: s.step, actions: ["signOut"], busy: true };
    case "error":
      return { title: "Can't continue", body: s.message, actions: s.retry === "none" ? ["signOut"] : ["retry", "signOut"], busy: false };
  }
}
