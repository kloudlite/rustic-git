/** A desktop login: the same CLI credential kl-connect holds, kept apart from it. */
export type Credential = { api: string; token: string; expiresAt: string; username: string };
