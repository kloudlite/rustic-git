# Console

`https://dev.kloudlite.io` is the web console: every object the API exposes, under the owner it belongs to.

| Page | Route |
|---|---|
| Workspaces | `/{owner}/workspaces` |
| Environment | `/{owner}/environments/{id}` |
| Registry image | `/{owner}/registries/{image}` |
| Repository | `/{owner}/{repo}` |
| Docs | `/docs` |

`{owner}` is your handle or a team slug. Switch owners from the header.

## What is only in the console

- Sign-in and username claim.
- Team creation, invitations, and roles.
- Pull request review and merge.
- Quota usage bars and the request form.

Everything else is also on the API or the CLI, and the console calls the same `/v1` routes.
