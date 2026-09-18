---
name: images
description: Use when a container image must be built, pushed or listed
---

# Images

An image is built from a directory in a workspace and pushed to the platform's registry under your
owner. The builder runs elsewhere; nobody needs a daemon of their own.

## From the bench

You have no directory to build from, so every build names the workspace that holds the context —
`kl_container_build {workspace, context, tag}` and `kl_container_push {workspace, from, to}` both
hand the work to that workspace's session, and so does a plain
`ask {to: "<workspace>", task: <the person's words>}`. The only verb that is yours alone is
`kl_images` — what the registry has.

## In a workspace

Verbs: `kl_container_build` (context and tag; it detaches, because a build is long — the answer is a
process id and `process {action: "logs"}` shows how it is going), `kl_container_push` (copy an image
the registry already holds to another tag), `kl_images` (what is there).

A tag is `name:tag`; it is pushed under your own owner, so `api:1` becomes your registry's `api:1`.
An environment service can then name that image.

Example:

    kl_container_build {context: ".", tag: "api:1"}
    process {action: "logs", id: "p3"}
    kl_container_push {from: "api:1", to: "api:latest"}
