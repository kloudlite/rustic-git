# Move files

Files move over ssh with the tools you already have. There is no file API; the tree is a directory and ssh reaches it.

## Copy

```bash
scp ./fixtures/users.json api:/home/kl/workspace/fixtures/
scp -r api:/home/kl/workspace/dist ./dist
```

## Sync

```bash
rsync -az --delete ./src/ api:/home/kl/workspace/src/
rsync -az api:/home/kl/workspace/coverage/ ./coverage/
```

## Read and write without a copy

```bash
ssh api 'cat ~/workspace/package.json' | jq .version
ssh api 'cat > ~/workspace/.env.local' < ./.env.local
```

## Editors

An editor's remote mode does the same over the same ssh config; see [Editors](../human-tools/editors.md).

## What lives where

| Path | Snapshotted by push |
|---|---|
| `/home/kl/workspace` | yes |
| `/home/kl` (the whole volume: dotfiles, editor settings) | yes |
| `~/workspace/.cache`, build caches | no |

A clone or restore carries the whole home, credentials included; it is always your own
workspace, never shared with another.
