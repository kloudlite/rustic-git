# syntax=docker/dockerfile:1
#
# Build context is the web/ workspace root, so the lockfile and every workspace
# package is visible.

# 1.3.14 exactly, matching CI's setup-bun, so the lockfile is read by one bun everywhere.
FROM oven/bun:1.4.2@sha256:9114c058aeae42162ee16dd5084b95fe9473970bb6bcb5b232ab1630f0546895 AS bun

# Node, with bun copied in. bun installs (it owns the lockfile) but does not run
# the build: `next build` under bun's runtime dies with SIGILL on GitHub's
# runners, while the same build under node is fine — which is what CI's check
# job does too.
FROM node:26-bookworm-slim@sha256:367679cf9792759492a486e4aa4b421764d71a9546a6dae8aab81a99eb797b3e AS deps
COPY --from=bun /usr/local/bin/bun /usr/local/bin/bun
WORKDIR /src
# The whole workspace at once, not a hand-listed set of manifests: that would
# silently break the day a new workspace package appears. bun installs in ~2s.
COPY . .
# --linker=hoisted, not bun's default isolated layout: isolated puts every package
# behind a node_modules/.bun/<pkg>@<ver> symlink, which Next's file tracer cannot
# follow, so the standalone output silently omits transitive deps (@swc/helpers)
# and the server dies at startup with MODULE_NOT_FOUND.
RUN bun install --frozen-lockfile --linker=hoisted

FROM deps AS build
ENV NEXT_TELEMETRY_DISABLED=1
RUN node node_modules/next/dist/bin/next build apps/web

# Same libc as the build stage: the standalone output carries native modules
# (sharp, shiki's oniguruma) compiled against it.
FROM node:26-bookworm-slim@sha256:367679cf9792759492a486e4aa4b421764d71a9546a6dae8aab81a99eb797b3e AS run
WORKDIR /app
ENV NODE_ENV=production NEXT_TELEMETRY_DISABLED=1 PORT=3000 HOSTNAME=0.0.0.0
RUN useradd --system --create-home --uid 1001 app

# Standalone keeps the monorepo shape, so the server lives under apps/web and the
# shared node_modules sits at the root.
COPY --from=build --chown=app:app /src/apps/web/.next/standalone ./
COPY --from=build --chown=app:app /src/apps/web/.next/static ./apps/web/.next/static
COPY --from=build --chown=app:app /src/apps/web/public ./apps/web/public

USER app
EXPOSE 3000
# AUTH_SECRET and AUTH_URL must be supplied at run time; see apps/web/.env.example.
CMD ["node", "apps/web/server.js"]
