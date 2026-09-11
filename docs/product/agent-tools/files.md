# Move files

Files move over ssh with the tools you already have. There is no file API; the tree is a directory and ssh reaches it.

## Copy

```bash
scp ./fixtures/users.json api:/home/kl/workspaces/api/fixtures/
scp -r api:/home/kl/workspaces/api/dist ./dist
```

## Sync

```bash
rsync -az --delete ./src/ api:/home/kl/workspaces/api/src/
rsync -az api:/home/kl/workspaces/api/coverage/ ./coverage/
```

## Read and write without a copy

```bash
ssh api 'cat ~/workspaces/api/package.json' | jq .version
ssh api 'cat > ~/workspaces/api/.env.local' < ./.env.local
```

## Editors

An editor's remote mode does the same over the same ssh config; see [Editors](../human-tools/editors.md).

## What lives where

| Path | Snapshotted by push | Shared across your workspaces |
|---|---|---|
| `/home/kl/workspaces/{name}` | yes | no |
| `/home/kl` (dotfiles, editor settings) | no | yes, within the region |
| `~/.cache`, `~/.local/state`, build caches | no | no |
