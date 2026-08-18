import Link from "next/link";
import type { Metadata } from "next";
import { OAuthButtons, OrDivider } from "@/components/auth/oauth-buttons";
import { LoginForm } from "@/components/auth/login-form";

export const metadata: Metadata = { title: "Sign in" };

export default function LoginPage() {
  return (
    <div>
      <div className="mb-7">
        <h1 className="text-[24px] font-bold leading-tight tracking-tight">Sign in</h1>
        <p className="mt-2 text-[14.5px] text-muted-foreground">
          Continue to your repositories.
        </p>
      </div>

      <OAuthButtons verb="Sign in" />

      <div className="my-6">
        <OrDivider />
      </div>

      <LoginForm />

      <p className="mt-7 text-[14px] text-muted-foreground">
        New to kloudlite?{" "}
        <Link href="/signup" className="font-semibold text-primary underline-offset-4 hover:underline">
          Create an account
        </Link>
      </p>
    </div>
  );
}
