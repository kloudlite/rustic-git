# syntax=docker/dockerfile:1
#
# Build context is the web/ workspace root, so the lockfile and every workspace
# package is visible. Built with bun, run with node from Next's standalone output:
# the runtime image carries the app and its runtime deps, not the toolchain.

FROM oven/bun:1.3-alpine AS build
WORKDIR /src
# The whole workspace at once — not just the package.json files. Bun installs in
# ~2s, and copying a hand-listed set of manifests silently breaks the day a new
# workspace package appears.
COPY . .
RUN bun install --frozen-lockfile
ENV NEXT_TELEMETRY_DISABLED=1
RUN bun run --cwd apps/web build

FROM node:22-alpine AS run
WORKDIR /app
ENV NODE_ENV=production NEXT_TELEMETRY_DISABLED=1 PORT=3000 HOSTNAME=0.0.0.0
RUN addgroup -S app && adduser -S app -G app

# Standalone keeps the monorepo shape, so the server lives under apps/web and the
# shared node_modules sits at the root.
COPY --from=build --chown=app:app /src/apps/web/.next/standalone ./
COPY --from=build --chown=app:app /src/apps/web/.next/static ./apps/web/.next/static
COPY --from=build --chown=app:app /src/apps/web/public ./apps/web/public

USER app
EXPOSE 3000
# AUTH_SECRET and AUTH_URL must be supplied at run time; see apps/web/.env.example.
CMD ["node", "apps/web/server.js"]
