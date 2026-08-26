import "server-only";

/** Outbound mail, through Resend's HTTP API. Two emails, one sender.
 *
 *  Unconfigured is a legitimate state — dev, or a deployment that has not set up a sending
 *  domain yet. Then nothing is sent and the caller is told so, and it falls back to showing
 *  the link for the inviter to pass on by hand. What must never happen is a silent no-op that
 *  leaves someone believing an email went out. */
export async function sendInvite(args: {
  to: string;
  teamName: string;
  invitedBy: string;
  role: string;
  link: string;
}): Promise<SendResult> {
  return send(args.to, `You're invited to ${args.teamName}`, [
    `${args.invitedBy} invited you to join ${args.teamName} as ${args.role}.`,
    "",
    `Accept: ${args.link}`,
    "",
    "The link works once, for the address it was sent to, and expires in 7 days.",
  ]);
}

/** The sign-in link. Short-lived and single-use, and it says so, because a link that arrives
 *  after someone stopped waiting should explain itself. */
export async function sendSignInLink(to: string, link: string): Promise<SendResult> {
  return send(to, "Sign in to kloudlite", [
    "Click to sign in:",
    "",
    link,
    "",
    "The link works once and expires in 15 minutes. If you did not ask for it, ignore this email.",
  ]);
}

type SendResult = { sent: true } | { sent: false; reason: string };

async function send(to: string, subject: string, lines: string[]): Promise<SendResult> {
  const key = process.env.RESEND_API_KEY;
  const from = process.env.RESEND_FROM;
  if (!key || !from) return { sent: false, reason: "Email is not configured on this deployment." };
  const text = lines.join("\n");

  const r = await fetch("https://api.resend.com/emails", {
    method: "POST",
    headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
    body: JSON.stringify({ from, to: [to], subject, text }),
  }).catch((e: unknown) => ({ ok: false, status: 0, text: async () => String(e) }));
  if (r.ok) return { sent: true };
  const detail = (await r.text()).slice(0, 200);
  console.error("resend", r.status, detail);
  return { sent: false, reason: "The email could not be sent." };
}
