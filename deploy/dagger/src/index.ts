// Reproduces deploy/dev/pod/ship.sh, one gate stage per function, driven from the laptop through
// the dagger-engine sidecar in the dev pod (deploy/dev/builder.yaml) instead of a remote exec.
// Read ship.sh before touching this file — every command here is copied from it, not reinvented.
import { dag, object, func, argument, Directory, Container, Secret } from "@dagger.io/dagger"

// The staging list ship.sh's `--source` walk must never see: worktree noise, build output and
// caches too big to upload, and the docs this repo keeps out of git already.
const IGNORE = [".git", "target", ".local", "**/node_modules", "web/.next", "harness/**/dist", ".superpowers"]

const RUST_IMAGE = "rust:1-bookworm"
// The five images the pod's ship.sh loops over, target:image, unchanged from that loop.
const IMAGE_TARGETS: [string, string][] = [
  ["server", "kloudlite"],
  ["agent", "kloudlite-agent"],
  ["gateway", "kloudlite-gateway"],
  ["builder-gate", "kloudlite-builder-gate"],
  ["slo", "kloudlite-slo"],
  ["workspace", "kloudlite-workspace"],
]

@object()
export class Kloudlite {
  // The rust container both check() and build() start from: apt deps clippy needs to link
  // (openssl, git's own build has none here — this is compiling OUR crates), plus musl-tools for
  // the `kl` cross-build build() does later on the same container.
  private rustBase(source: Directory): Container {
    return dag
      .container()
      .from(RUST_IMAGE)
      .withExec(["apt-get", "update"])
      .withExec([
        "apt-get", "install", "-y", "--no-install-recommends",
        "pkg-config", "libssl-dev", "clang", "cmake", "python3", "musl-tools",
      ])
      .withExec(["rustup", "component", "add", "clippy"])
      // In the base, not build(): a deterministic exec is a cached layer, so the musl target is
      // downloaded once per engine, not once per build.
      .withExec(["rustup", "target", "add", "x86_64-unknown-linux-musl"])
      // Three caches, each named for what it holds and shared by every stage that touches it:
      // crate downloads, git dependencies, and the compiled target dir — check() and build()
      // compile into the same tree (debug and dev-image are separate subdirs), so a green gate
      // warms the build that follows it.
      .withMountedCache("/usr/local/cargo/registry", dag.cacheVolume("kloudlite-cargo-registry"))
      .withMountedCache("/usr/local/cargo/git", dag.cacheVolume("kloudlite-cargo-git"))
      .withMountedCache("/work/target", dag.cacheVolume("kloudlite-cargo-target"))
      .withEnvVariable("CARGO_TARGET_DIR", "/work/target")
      .withEnvVariable("CARGO_INCREMENTAL", "0")
      .withMountedDirectory("/work/src", source)
      .withWorkdir("/work/src")
  }

  // ship.sh's gate: clippy -D warnings, then tests. nextest is the pod's runner (parallel test
  // binaries); a fresh container has no nextest install step in this brief, so plain `cargo test`
  // stands in — slower, same coverage, no doctests lost either way (the workspace has none).
  // ponytail: no nextest here and no gdb hang watcher — both are pod-only concerns (parallel test
  // binaries, a stuck process to attach to); a Dagger container run either finishes or times out.
  @func()
  async check(@argument({ ignore: IGNORE }) source: Directory): Promise<string> {
    const ctr = this.rustBase(source)
      .withExec(["cargo", "clippy", "--workspace", "--all-targets", "--locked", "--", "-D", "warnings"])
      .withExec(["cargo", "test", "--workspace", "--locked"])
    const out = await ctr.stdout()
    return out.split("\n").slice(-40).join("\n")
  }

  // web.yml's exact steps (ship.sh's own web gate), run from web/ under bun.
  @func()
  async checkWeb(@argument({ ignore: IGNORE }) source: Directory): Promise<string> {
    const ctr = dag
      .container()
      .from("oven/bun:1")
      .withMountedDirectory("/work/src", source)
      .withWorkdir("/work/src/web")
      // node_modules is IGNOREd on upload, so every run installs; bun's global cache makes
      // that a link step rather than a download.
      .withMountedCache("/root/.bun/install/cache", dag.cacheVolume("kloudlite-bun"))
      // The install itself and turbo's task cache persist too, so an unchanged lockfile is a
      // no-op install and an untouched package's typecheck/lint/test are cache hits.
      .withMountedCache("/work/src/web/node_modules", dag.cacheVolume("kloudlite-web-node-modules"))
      .withMountedCache("/work/src/web/.turbo", dag.cacheVolume("kloudlite-web-turbo"))
      .withExec(["bun", "install", "--frozen-lockfile"])
      .withExec(["bun", "run", "typecheck"])
      .withExec(["bun", "run", "lint"])
      .withExec(["bun", "run", "test"])
    const out = await ctr.stdout()
    return out.split("\n").slice(-40).join("\n")
  }

  // Lays out a Directory exactly as ship.sh's /work/ctx staging dir, so publish() can hand it
  // straight to dag.container().build() as the Dockerfile context — read the Dockerfile's COPY
  // lines before changing a single path here.
  @func()
  build(@argument({ ignore: IGNORE }) source: Directory): Directory {
    const built = this.rustBase(source)
      .withExec(["cargo", "build", "--profile", "dev-image", "--locked", "--bins"])
      .withExec([
        "cargo", "build", "--profile", "dev-image", "--locked",
        "-p", "kl", "--target", "x86_64-unknown-linux-musl",
      ])

    const bins = [
      "kloudlite", "kloudlite-api", "kloudlite-worker", "kloudlite-agent",
      "kloudlite-gateway", "kloudlite-builder-gate", "kloudlite-slo", "kl-connect",
    ]
    let ctx = dag
      .directory()
      .withFile("Dockerfile", source.file("Dockerfile"))
      .withFile(".dockerignore", source.file(".dockerignore"))
      .withDirectory("deploy/workspace-image", source.directory("deploy/workspace-image"))
    for (const b of bins) {
      ctx = ctx.withFile(`target/dev-image/${b}`, built.file(`/work/target/dev-image/${b}`))
    }
    ctx = ctx.withFile(
      "target/x86_64-unknown-linux-musl/dev-image/kl",
      built.file("/work/target/x86_64-unknown-linux-musl/dev-image/kl"),
    )
    return ctx
  }

  // ship.sh's per-image loop, plus the web image (docs/product copied into the web build context
  // exactly as ship.sh does it — on the Directory, so the laptop tree is never touched).
  @func()
  async publish(
    @argument({ ignore: IGNORE }) source: Directory,
    tag: string,
    ghcrUser: string,
    ghcrToken: Secret,
  ): Promise<string> {
    const ctx = this.build(source)
    const refs: string[] = []

    for (const [target, image] of IMAGE_TARGETS) {
      const built = dag
        .container()
        .build(ctx, { dockerfile: "Dockerfile", target, buildArgs: [{ name: "PROFILE", value: "dev-image" }] })
        .withRegistryAuth("ghcr.io", ghcrUser, ghcrToken)
      for (const t of [tag, "latest"]) {
        const ref = `ghcr.io/kloudlite/${image}:${t}`
        await built.publish(ref)
        refs.push(ref)
      }
    }

    // web/apps/web/content/docs is git-ignored and read at runtime (lib/docs.ts); ship.sh
    // populates it by copying docs/product in before the build — done here on the Directory.
    const webCtx = source
      .directory("web")
      .withDirectory("apps/web/content/docs", source.directory("docs/product"))
    const webBuilt = dag
      .container()
      .build(webCtx)
      .withRegistryAuth("ghcr.io", ghcrUser, ghcrToken)
    for (const t of [tag, "latest"]) {
      const ref = `ghcr.io/kloudlite/kloudlite-web:${t}`
      await webBuilt.publish(ref)
      refs.push(ref)
    }

    return refs.join("\n")
  }

  // check + checkWeb (fail stops it — a rejected promise here aborts before publish runs), then
  // publish. Mirrors ship.sh's default (gated) path; --no-gate in the wrapper calls publish direct.
  @func()
  async ship(
    @argument({ ignore: IGNORE }) source: Directory,
    tag: string,
    ghcrUser: string,
    ghcrToken: Secret,
  ): Promise<string> {
    await this.check(source)
    await this.checkWeb(source)
    return this.publish(source, tag, ghcrUser, ghcrToken)
  }
}
