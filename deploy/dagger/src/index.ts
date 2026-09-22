// Reproduces deploy/dev/pod/ship.sh, one gate stage per function, driven from the laptop through
// the dagger-engine sidecar in the dev pod (deploy/dev/builder.yaml) instead of a remote exec.
// Read ship.sh before touching this file — every command here is copied from it, not reinvented.
//
// Every image is DEFINED here (owner ruling 2026-09-22: "don't write docker files define
// everything in dagger"), not built from Dockerfile/web/Dockerfile with dag.container().build().
// The two Dockerfiles still exist, byte-identical, only because CI's image.yml builds from them —
// they are that pipeline's copy of the same definitions, never the source of truth, and a change
// to an image* method below must be mirrored into the matching Dockerfile stage in the same commit.
import { dag, object, func, argument, Directory, Container, Secret } from "@dagger.io/dagger"

// The staging list ship.sh's `--source` walk must never see: worktree noise, build output and
// caches too big to upload, and the docs this repo keeps out of git already.
const IGNORE = [".git", "target", ".local", "**/node_modules", "web/.next", "harness/**/dist", ".superpowers"]

const RUST_IMAGE = "rust:1-bookworm"
const DEBIAN_SLIM = "debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241"
const ALPINE_WORKSPACE = "alpine:3.20@sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc"
const BUN_IMAGE = "oven/bun:1.3.14@sha256:e10577f0db68676a7024391c6e5cb4b879ebd17188ab750cf10024a6d700e5c4"
const NODE_IMAGE = "node:22-bookworm-slim@sha256:d649c27dae7ba0137b3cef5dd75baa422c08dc3d9e3fc0c23dfb172dc3cc6436"

@object()
export class Kloudlite {
  // The rust container check() and compiled() start from: apt deps clippy needs to link
  // (openssl, git's own build has none here — this is compiling OUR crates), plus musl-tools for
  // the `kl` cross-build compiled() does later on the same container.
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
      // In the base, not compiled(): a deterministic exec is a cached layer, so the musl target is
      // downloaded once per engine, not once per build.
      .withExec(["rustup", "target", "add", "x86_64-unknown-linux-musl"])
      // Three caches, each named for what it holds and shared by every stage that touches it:
      // crate downloads, git dependencies, and the compiled target dir — check() and compiled()
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

  // Compiles the dev-image profile once; the image* methods below each pull their own binaries
  // out of this Container rather than re-running cargo — the binaries are the only thing that
  // crosses from compile into image.
  private compiled(source: Directory): Container {
    return this.rustBase(source)
      .withExec(["cargo", "build", "--profile", "dev-image", "--locked", "--bins"])
      .withExec([
        "cargo", "build", "--profile", "dev-image", "--locked",
        "-p", "kl", "--target", "x86_64-unknown-linux-musl",
      ])
      // /work/target is a cache mount, and a cache mount is never part of the container's
      // snapshot — Container.file() on it fails with "cannot retrieve path from cache" (run 4).
      // The binaries have to be copied onto the container's own filesystem first.
      .withExec(["sh", "-c", "mkdir -p /out/musl && cp /work/target/dev-image/kloudlite /work/target/dev-image/kloudlite-api /work/target/dev-image/kloudlite-worker /work/target/dev-image/kloudlite-agent /work/target/dev-image/kloudlite-gateway /work/target/dev-image/kloudlite-builder-gate /work/target/dev-image/kloudlite-slo /work/target/dev-image/kl-connect /out/ && cp /work/target/x86_64-unknown-linux-musl/dev-image/kl /out/musl/kl"])
  }

  // Dockerfile `server` stage: three binaries, one unprivileged user, git/ssh/curl for the
  // three processes it can run as (Dockerfile lines 20-51).
  private imageServer(built: Container): Container {
    return dag
      .container()
      .from(DEBIAN_SLIM)
      .withExec(["sh", "-c",
        "apt-get update && apt-get install -y --no-install-recommends ca-certificates openssh-client git curl " +
        "&& rm -rf /var/lib/apt/lists/*"])
      .withFile("/usr/local/bin/kloudlite", built.file("/out/kloudlite"))
      .withFile("/usr/local/bin/kloudlite-api", built.file("/out/kloudlite-api"))
      .withFile("/usr/local/bin/kloudlite-worker", built.file("/out/kloudlite-worker"))
      .withExec(["sh", "-c",
        "useradd --system --uid 1001 --user-group --no-create-home --shell /usr/sbin/nologin kloudlite " +
        "&& mkdir -p /var/cache/kloudlite /var/lib/kloudlite " +
        "&& chown kloudlite:kloudlite /var/cache/kloudlite /var/lib/kloudlite"])
      .withEnvVariable("KLOUDLITE_CACHE_DIR", "/var/cache/kloudlite")
      .withEnvVariable("KLOUDLITE_HOST_KEY", "/var/lib/kloudlite/host_key")
      .withUser("kloudlite")
      .withExposedPort(8080)
      .withExposedPort(2222)
      .withEntrypoint(["kloudlite"])
      .withDefaultArgs(["serve"])
  }

  // Dockerfile `agent` stage: root, btrfs/mount tooling (lines 56-79).
  private imageAgent(built: Container): Container {
    return dag
      .container()
      .from(DEBIAN_SLIM)
      .withExec(["sh", "-c",
        "apt-get update && apt-get install -y --no-install-recommends " +
        "btrfs-progs util-linux ca-certificates git openssh-client nfs-common netbase " +
        "&& rm -rf /var/lib/apt/lists/*"])
      .withFile("/usr/local/bin/kloudlite-agent", built.file("/out/kloudlite-agent"))
      .withEntrypoint(["kloudlite-agent"])
  }

  // Dockerfile `gateway` stage: setcap NET_BIND_SERVICE on the binary itself, then drop the
  // capability-granting package before switching to the unprivileged user (lines 84-103).
  private imageGateway(built: Container): Container {
    return dag
      .container()
      .from(DEBIAN_SLIM)
      .withExec(["sh", "-c",
        "apt-get update && apt-get install -y --no-install-recommends ca-certificates libcap2-bin " +
        "&& rm -rf /var/lib/apt/lists/*"])
      .withFile("/usr/local/bin/kloudlite-gateway", built.file("/out/kloudlite-gateway"))
      .withExec(["sh", "-c",
        "setcap cap_net_bind_service=+ep /usr/local/bin/kloudlite-gateway " +
        "&& apt-get purge -y libcap2-bin && apt-get autoremove -y"])
      .withExec(["useradd", "--system", "--uid", "1001", "--user-group",
        "--no-create-home", "--shell", "/usr/sbin/nologin", "kloudlite"])
      .withUser("kloudlite")
      .withExposedPort(443)
      .withExposedPort(8080)
      .withEntrypoint(["kloudlite-gateway"])
  }

  // Dockerfile `builder-gate` stage: same shape as gateway, no file capability (lines 107-118).
  private imageBuilderGate(built: Container): Container {
    return dag
      .container()
      .from(DEBIAN_SLIM)
      .withExec(["sh", "-c",
        "apt-get update && apt-get install -y --no-install-recommends ca-certificates " +
        "&& rm -rf /var/lib/apt/lists/*"])
      .withFile("/usr/local/bin/kloudlite-builder-gate", built.file("/out/kloudlite-builder-gate"))
      .withExec(["useradd", "--system", "--uid", "1001", "--user-group",
        "--no-create-home", "--shell", "/usr/sbin/nologin", "kloudlite"])
      .withUser("kloudlite")
      .withExposedPort(1234)
      .withExposedPort(8080)
      .withEntrypoint(["kloudlite-builder-gate"])
  }

  // Dockerfile `slo` stage: toolbox image, crane/kubectl fetched and checksummed at build time
  // (lines 180-217) — the checksum check is the RUN chain's own sha256sum -c, kept in one exec so
  // a failed checksum fails the same layer it would in docker build.
  private imageSlo(built: Container): Container {
    return dag
      .container()
      .from(DEBIAN_SLIM)
      .withExec(["sh", "-c",
        "apt-get update && apt-get install -y --no-install-recommends " +
        "bash ca-certificates git openssh-client curl openssl bind9-dnsutils " +
        "&& rm -rf /var/lib/apt/lists/*"])
      .withExec(["sh", "-c", [
        "set -eux;",
        'curl -fsSL -o /tmp/crane.tgz "https://github.com/google/go-containerregistry/releases/download/v0.20.3/go-containerregistry_Linux_x86_64.tar.gz";',
        'echo "36c67a932f489b3f2724b64af90b599a8ef2aa7b004872597373c0ad694dc059  /tmp/crane.tgz" | sha256sum -c -;',
        "tar -xzf /tmp/crane.tgz -C /usr/local/bin crane;",
        'curl -fsSL -o /usr/local/bin/kubectl "https://dl.k8s.io/release/v1.31.5/bin/linux/amd64/kubectl";',
        'echo "fbecbfd375b3686002c2e81d51c390172f5ffba3d6b47920d55342cb03f557af  /usr/local/bin/kubectl" | sha256sum -c -;',
        "chmod +x /usr/local/bin/crane /usr/local/bin/kubectl;",
        "rm -f /tmp/crane.tgz",
      ].join(" ")])
      .withFile("/usr/local/bin/kloudlite-slo", built.file("/out/kloudlite-slo"))
      .withFile("/usr/local/bin/kl-connect", built.file("/out/kl-connect"))
      .withExec(["useradd", "--system", "--uid", "1001", "--user-group",
        "--no-create-home", "--shell", "/usr/sbin/nologin", "kloudlite"])
      .withUser("kloudlite")
      .withEntrypoint(["kloudlite-slo"])
  }

  // Dockerfile `workspace` stage: the default workspace image, alpine base, no USER kl (root
  // entrypoint is k8s::prelude, which drops to kl itself before exec'ing sshd) (lines 138-176).
  private imageWorkspace(source: Directory, built: Container): Container {
    return dag
      .container()
      .from(ALPINE_WORKSPACE)
      .withExec(["sh", "-c", [
        "apk add --no-cache libstdc++ libgcc docker-cli docker-cli-buildx nodejs npm",
        "mkdir -p /var/empty",
        "adduser -D -u 1000 -s /nix/profile/current/bin/zsh kl",
        "sed -i 's/^kl:!:/kl:*:/' /etc/shadow",
        "printf '%s\\n' 'Kloudlite workspace — you are kl (no root, no sudo).' > /etc/motd",
      ].join(" && ")])
      .withFile(
        "/usr/local/bin/docker-credential-kl",
        source.file("deploy/workspace-image/docker-credential-kl"),
        { permissions: 0o755 },
      )
      .withFile(
        "/usr/local/bin/kl",
        built.file("/out/musl/kl"),
        { permissions: 0o755 },
      )
      .withFile("/etc/profile.d/kl-build.sh", source.file("deploy/workspace-image/kl-build.sh"))
      .withFile("/etc/kloudlite/gitignore-global", source.file("deploy/workspace-image/gitignore-global"))
      .withExec(["sh", "-c",
        "apk add --no-cache --virtual .gyp python3 make g++ " +
        "&& npm install -g @nanonets/graft@0.18.0 " +
        "&& npm cache clean --force " +
        "&& apk del .gyp"])
      .withEnvVariable("DO_NOT_TRACK", "1")
  }

  // deploy/bench/Dockerfile: harness-bench from its TypeScript source under Node 24 type
  // stripping, pi from the harness's own lockfile, and the musl kl. npm's cache is a named
  // volume so a lockfile-unchanged rebuild never re-downloads.
  private imageBench(source: Directory, built: Container): Container {
    const deps = dag
      .container()
      .from("node:24-bookworm-slim")
      .withMountedCache("/root/.npm", dag.cacheVolume("kloudlite-npm"))
      .withWorkdir("/opt/harness")
      .withFile("package.json", source.file("harness/package.json"))
      .withFile("package-lock.json", source.file("harness/package-lock.json"))
      .withExec(["npm", "ci", "--omit=dev"])
      // The bench's own deps (ai, @ai-sdk/*, @sinclair/typebox) live in bench/package.json, not the
      // harness root — the fleet's first 228b7610 bench died at import with ERR_MODULE_NOT_FOUND.
      .withFile("bench/package.json", source.file("harness/bench/package.json"))
      .withFile("bench/package-lock.json", source.file("harness/bench/package-lock.json"))
      .withExec(["sh", "-c", "cd bench && npm ci --omit=dev"])
    return dag
      .container()
      .from("node:24-bookworm-slim")
      .withExec(["sh", "-c",
        "apt-get update && apt-get install -y --no-install-recommends ca-certificates util-linux " +
        "&& rm -rf /var/lib/apt/lists/*"])
      .withFile("/usr/local/bin/kl", built.file("/out/musl/kl"), { permissions: 0o755 })
      .withFile("/opt/harness/package.json", source.file("harness/package.json"))
      .withFile("/opt/harness/package-lock.json", source.file("harness/package-lock.json"))
      .withDirectory("/opt/harness/node_modules", deps.directory("/opt/harness/node_modules"))
      .withFile("/opt/harness/bench/package.json", source.file("harness/bench/package.json"))
      .withDirectory("/opt/harness/bench/node_modules", deps.directory("/opt/harness/bench/node_modules"))
      .withDirectory("/opt/harness/bench/src", source.directory("harness/bench/src"))
      .withDirectory("/opt/harness/pi", source.directory("harness/pi"))
      .withExec(["sh", "-c",
        "chmod 0755 /opt/harness/bench/src/main.ts " +
        "&& ln -s /opt/harness/bench/src/main.ts /usr/local/bin/harness-bench " +
        "&& mkdir -p /bench /home/kl && chown 1000:1000 /bench /home/kl"])
      .withUser("1000:1000")
      .withWorkdir("/home/kl")
      .withEntrypoint([])
  }

  // deploy/kompress/Dockerfile: the model (chopratejas/kompress-v2-base, int8 ONNX) is baked in
  // at build time — the owner's call, no runtime download for something this small. pip's cache
  // is a named volume; the offline-load RUN is what proves the runtime image needs no network.
  private imageKompress(source: Directory): Container {
    const builder = dag
      .container()
      .from("python:3.13-slim")
      .withMountedCache("/root/.cache/pip", dag.cacheVolume("kloudlite-pip"))
      .withExec(["pip", "install", "--no-cache-dir", "headroom-ai[proxy]==0.38.0", "huggingface-hub>=1.5.0,<2.0"])
      .withEnvVariable("HF_HOME", "/opt/hf")
      .withExec(["python", "-c",
        "from headroom.transforms.kompress_compressor import KompressCompressor as K; K().preload(allow_download=True)"])
      .withEnvVariable("HF_HUB_OFFLINE", "1")
      .withEnvVariable("TRANSFORMERS_OFFLINE", "1")
      .withExec(["python", "-c",
        "from headroom.transforms.kompress_compressor import KompressCompressor as K; " +
        "k = K(); k.preload(allow_download=False); assert k.is_ready()"])
      .withExec(["sh", "-c", "find /opt/hf -iname '*fp32*onnx*' -delete"])
    return dag
      .container()
      .from("python:3.13-slim")
      .withDirectory("/usr/local/lib/python3.13/site-packages", builder.directory("/usr/local/lib/python3.13/site-packages"))
      .withDirectory("/usr/local/bin", builder.directory("/usr/local/bin"))
      .withDirectory("/opt/hf", builder.directory("/opt/hf"))
      .withEnvVariable("HF_HOME", "/opt/hf")
      .withEnvVariable("HF_HUB_OFFLINE", "1")
      .withEnvVariable("TRANSFORMERS_OFFLINE", "1")
      .withEnvVariable("PYTHONUNBUFFERED", "1")
      .withEnvVariable("HEADROOM_KOMPRESS_ONNX_INTRA_THREADS", "2")
      .withWorkdir("/opt/kompress")
      .withFile("server.py", source.file("deploy/kompress/server.py"))
      .withUser("1001:1001")
      .withExposedPort(8787)
      .withEntrypoint(["uvicorn", "server:app", "--host", "0.0.0.0", "--port", "8787"])
  }

  // web/Dockerfile `deps`+`build` stages: bun installs (owns the lockfile), node runs `next
  // build` (bun SIGILLs under GitHub's runners for this step) (web/Dockerfile lines 6-19).
  private webBuild(webCtx: Directory): Container {
    return dag
      .container()
      .from(NODE_IMAGE)
      .withFile("/usr/local/bin/bun", dag.container().from(BUN_IMAGE).file("/usr/local/bin/bun"))
      .withWorkdir("/src")
      .withDirectory("/src", webCtx)
      .withMountedCache("/root/.bun/install/cache", dag.cacheVolume("kloudlite-bun"))
      .withExec(["bun", "install", "--frozen-lockfile", "--linker=hoisted"])
      .withEnvVariable("NEXT_TELEMETRY_DISABLED", "1")
      .withMountedCache("/src/apps/web/.next/cache", dag.cacheVolume("kloudlite-web-next-cache"))
      .withExec(["node", "node_modules/next/dist/bin/next", "build", "apps/web"])
  }

  // web/Dockerfile `run` stage: standalone output, one unprivileged user (web/Dockerfile lines
  // 21-35).
  private imageWeb(webCtx: Directory): Container {
    const build = this.webBuild(webCtx)
    return dag
      .container()
      .from(NODE_IMAGE)
      .withWorkdir("/app")
      .withEnvVariable("NODE_ENV", "production")
      .withEnvVariable("NEXT_TELEMETRY_DISABLED", "1")
      .withEnvVariable("PORT", "3000")
      .withEnvVariable("HOSTNAME", "0.0.0.0")
      .withExec(["useradd", "--system", "--create-home", "--uid", "1001", "app"])
      .withDirectory("/app", build.directory("/src/apps/web/.next/standalone"), { owner: "app:app" })
      .withDirectory("/app/apps/web/.next/static", build.directory("/src/apps/web/.next/static"), { owner: "app:app" })
      .withDirectory("/app/apps/web/public", build.directory("/src/apps/web/public"), { owner: "app:app" })
      .withDirectory("/app/apps/web/content", build.directory("/src/apps/web/content"), { owner: "app:app" })
      .withUser("app")
      .withExposedPort(3000)
      .withEntrypoint(["node"])
      .withDefaultArgs(["apps/web/server.js"])
  }

  // ship.sh's per-image loop, plus the web image (docs/product copied into the web build context
  // exactly as ship.sh does it — on the Directory, so the laptop tree is never touched). Every
  // image is now built by the image* methods above, never dag.container().build().
  @func()
  async publish(
    @argument({ ignore: IGNORE }) source: Directory,
    tag: string,
    ghcrUser: string,
    ghcrToken: Secret,
  ): Promise<string> {
    const built = this.compiled(source)
    const refs: string[] = []

    const IMAGE_BUILDERS: [() => Container, string][] = [
      [() => this.imageServer(built), "kloudlite"],
      [() => this.imageAgent(built), "kloudlite-agent"],
      [() => this.imageGateway(built), "kloudlite-gateway"],
      [() => this.imageBuilderGate(built), "kloudlite-builder-gate"],
      [() => this.imageSlo(built), "kloudlite-slo"],
      [() => this.imageWorkspace(source, built), "kloudlite-workspace"],
      [() => this.imageBench(source, built), "kloudlite-bench"],
      [() => this.imageKompress(source), "kloudlite-kompress"],
    ]

    for (const [make, image] of IMAGE_BUILDERS) {
      const ctr = make().withRegistryAuth("ghcr.io", ghcrUser, ghcrToken)
      for (const t of [tag, "latest"]) {
        const ref = `ghcr.io/kloudlite/${image}:${t}`
        await ctr.publish(ref)
        refs.push(ref)
      }
    }

    // web/apps/web/content/docs is git-ignored and read at runtime (lib/docs.ts); ship.sh
    // populates it by copying docs/product in before the build — done here on the Directory.
    const webCtx = source
      .directory("web")
      .withDirectory("apps/web/content/docs", source.directory("docs/product"))
    const webBuilt = this.imageWeb(webCtx).withRegistryAuth("ghcr.io", ghcrUser, ghcrToken)
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
