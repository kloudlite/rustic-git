import type { Metadata } from "next";
import { redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { UsernameForm } from "@/components/onboarding/username-form";

export const metadata: Metadata = { title: "Pick a handle" };

/** Shown once, after the first sign-in. A person exists the moment they sign in,
 *  but they have no namespace until they choose one — and almost every page in the
 *  product builds a URL from it, so this comes before any of them. */
export default async function WelcomePage() {
  const session = await getSession();
  if (!session) redirect("/login");
  if (session.user.username) redirect("/");

  return (
    <div className="w-full max-w-auth">
      <UsernameForm
        name={session.user.name}
        suggestion={session.user.email.split("@")[0].replace(/[^a-z0-9-]/gi, "-").toLowerCase()}
      />
    </div>
  );
}
