import { CodeBlock } from "@/components/repo/code-block";
import { CloneMenu } from "@/components/repo/clone-menu";

/** A repo with no refs. Not an error and not an empty box: it is the state every
 *  repo starts in, and the only useful thing to show is how to leave it. Two
 *  paths, because the two situations are genuinely different — nothing on disk
 *  yet, or something on disk that needs a remote. */
export function EmptyRepo({ owner, repo, host = "kloudlite.io" }: { owner: string; repo: string; host?: string }) {
  const url = `https://${host}/${owner}/${repo}.git`;

  return (
    <div className="mx-auto max-w-2xl">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-title font-semibold tracking-title">{repo}</h1>
          <p className="mt-1 text-sm2 text-muted-foreground">
            This repo is empty. Push something to it and this page becomes the code.
          </p>
        </div>
        <CloneMenu owner={owner} repo={repo} host={host} />
      </div>

      <section className="mt-8">
        <h2 className="text-sm2 font-medium">Start something new</h2>
        <div className="mt-2.5 border border-border bg-card">
          <CodeBlock
            lang="bash"
            code={`git init
git add .
git commit -m "Initial commit"
git branch -M main
git remote add origin ${url}
git push -u origin main`}
          />
        </div>
      </section>

      <section className="mt-7">
        <h2 className="text-sm2 font-medium">Push something you already have</h2>
        <div className="mt-2.5 border border-border bg-card">
          <CodeBlock
            lang="bash"
            code={`git remote add origin ${url}
git push -u origin main`}
          />
        </div>
      </section>

      <p className="mt-7 text-caption text-muted-foreground">
        Pushing over HTTPS asks for a username and a token — any username, and an
        access token from Settings. SSH uses the keys you have added there.
      </p>
    </div>
  );
}
