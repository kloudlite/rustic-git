# Packages

A workspace's toolchain is a Nix profile built from `packages`. Every entry is a nixpkgs attribute name; a pinned entry is locked to one store path before the workspace is written, so what you asked for is what you get, on every start, on every node.

## Syntax

| Entry | Meaning |
|---|---|
| `nodejs` | The attribute at the region's nixpkgs pin |
| `nodejs@22` | Newest 22.x the binary cache holds |
| `nodejs@22.12` | Newest 22.12.x |
| `nodejs@22.12.0` | Exactly that release |
| `nodejs@latest` | Newest release known to any index |

Anything else after `@` is refused naming the entry.

## Resolution

A pin is resolved at request time, not at build time:

1. A 24-hour cache in the platform.
2. Nixhub.
3. A mirrored nixpkgs version index, refreshed daily.

A candidate is checked against `cache.nixos.org` before it is accepted: a release the cache never built (an EOL version marked insecure) is skipped for a prefix pin and refused with `422` for an exact one, naming the versions that are cached. The platform never builds a package from source.

## Locks

The resolved entries are stored beside `packages` as locks (entry, version, attribute path, nixpkgs revision, store path). An unchanged entry keeps its lock through every other edit, so `pnpm` today is `pnpm` next month until you ask otherwise.

```bash [API]
# Re-resolve every pin to what is newest now.
curl -sS -X POST https://dev.kloudlite.io/v1/workspaces/$WS/packages/update \
  -H "Authorization: Bearer $KL_TOKEN"
```

An index outage during an update keeps the locks it had.

## Changing the list

```bash [API]
curl -sS -X PATCH https://dev.kloudlite.io/v1/workspaces/$WS \
  -H "Authorization: Bearer $KL_TOKEN" -H 'content-type: application/json' \
  -d '{"packages": ["nodejs@22", "pnpm", "postgresql_16", "redis"]}'
```

The profile is rebuilt and swapped under the running pod; open shells pick up the new `PATH` on their next login.

## Status

`packages_status` on the workspace document says what is installed: the base set, the observed list, and the profile path. A workspace whose pin is not in the binary cache reports `PackagesReady=False/NotCached` and waits for an edit.

## Sharing

A profile is keyed by its inputs, so a second workspace or a clone with the same list is published from the node's index without evaluating nixpkgs again.
