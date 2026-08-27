"use client";

import { useState } from "react";

/** Open/closed for a dialog whose form is a `useActionState` action.
 *
 *  A dialog cannot close itself on submit: the action's result arrives later, and
 *  a plain `useState(false)` never hears about it — the dialog just sat there after
 *  a successful clone. So "open" is "opened since the last successful result":
 *  remember which result was current when it opened, and a NEW success closes it.
 *  Reopening after a success does not re-close, because that success is then the
 *  one it was opened on. Actions signal success by returning `{ ok: true }`. */
export function useDialogUntilSuccess<S extends { ok?: true } | null>(state: S) {
  const [openedOn, setOpenedOn] = useState<S | undefined>(undefined);
  const open = openedOn !== undefined && !(state?.ok && state !== openedOn);
  const setOpen = (next: boolean) => setOpenedOn(next ? state : undefined);
  return [open, setOpen] as const;
}
