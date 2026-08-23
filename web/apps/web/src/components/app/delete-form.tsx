"use client";

import { useActionState } from "react";

export type DeleteState = { error?: string } | null;

/** A one-button form that can fail. The six destructive actions used to return
 *  nothing, so a refused delete looked like a click that did not register. This
 *  holds the action state so a server component can still render the row.
 *
 *  `confirm` is the browser's own dialog: a delete that cannot be undone gets one
 *  question, and a custom modal would be a component for one sentence. */
export function DeleteForm({
  action,
  fields,
  confirm,
  className,
  children,
}: {
  action: (prev: DeleteState, formData: FormData) => Promise<DeleteState>;
  /** Hidden inputs: what the action is about has to travel with the request. */
  fields: Record<string, string>;
  confirm?: string;
  className?: string;
  children: React.ReactNode;
}) {
  const [state, act, pending] = useActionState<DeleteState, FormData>(action, null);
  return (
    <form
      action={act}
      onSubmit={(e) => {
        if (confirm && !window.confirm(confirm)) e.preventDefault();
      }}
      className={className}
    >
      {Object.entries(fields).map(([name, value]) => (
        <input key={name} type="hidden" name={name} value={value} />
      ))}
      {state?.error && (
        <p role="alert" className="mr-3 inline text-caption font-medium text-destructive">{state.error}</p>
      )}
      {/* `contents` so the fieldset adds no box; `disabled` so the button goes
          inert while the request is out without each caller wiring `pending`. */}
      <fieldset disabled={pending} className="contents">{children}</fieldset>
    </form>
  );
}
