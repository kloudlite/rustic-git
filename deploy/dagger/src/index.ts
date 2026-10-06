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
const IGNORE = [".git", "target", ".local", "**/node_modules", "web/.next", ".superpowers"]

const RUST_IMAGE = "rust:1-bookworm"
const DEBIAN_SLIM = "debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241"
const WORKSPACE_NODE = "node:22-bookworm-slim@sha256:83f487e0a63425e5b4d146fb5e5be574bcbe1b7b843d3ebafdd95eaf7767a7e5"
const DOCKER_CLI = "docker:28-cli@sha256:625d9431a9f54c5a2bc90f24f0e1c3d55b1349fd857dd85035f98c2c9acbdd4d"
const BUILDX_BIN = "docker/buildx-bin:0.20.1@sha256:ead27bfcde6308a757b4a5a4a931937363c1fa0091f7e2994b9114521853cf69"
const BUN_IMAGE = "oven/bun:1.3.14@sha256:e10577f0db68676a7024391c6e5cb4b879ebd17188ab750cf10024a6d700e5c4"
// graphcode TUI (kloudlite/harness) pinned by commit; bump = edit here and in deploy/bench/Dockerfile.
const HARNESS_REV = "d4b46e030b862cecc9d250976ebb3fbbb5d0d3cc"
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
      // `test:node` needs real Node 22 (web.yml's setup-node); the bun image's `node` is bun.
      .withFile("/usr/local/bin/node", dag.container().from(NODE_IMAGE).file("/usr/local/bin/node"))
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
        "-p", "kl", "-p", "kloudlite-intercept-proxy", "--target", "x86_64-unknown-linux-musl",
      ])
      // /work/target is a cache mount, and a cache mount is never part of the container's
      // snapshot — Container.file() on it fails with "cannot retrieve path from cache" (run 4).
      // The binaries have to be copied onto the container's own filesystem first.
      .withExec(["sh", "-c", "mkdir -p /out/musl && cp /work/target/dev-image/kloudlite /work/target/dev-image/kloudlite-api /work/target/dev-image/kloudlite-worker /work/target/dev-image/kloudlite-agent /work/target/dev-image/kloudlite-gateway /work/target/dev-image/kloudlite-builder-gate /work/target/dev-image/kloudlite-controller /work/target/dev-image/kloudlite-slo /work/target/dev-image/kl-connect /out/ && cp /work/target/x86_64-unknown-linux-musl/dev-image/kl /work/target/x86_64-unknown-linux-musl/dev-image/kloudlite-intercept-proxy /out/musl/"])
  }

  // Dockerfile `server` stage: three binaries, one unprivileged user, git/ssh/curl for the
  // three processes it can run as.
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

  // Dockerfile `agent` stage: root, btrfs/mount tooling.
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
  // capability-granting package before switching to the unprivileged user.
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

  // Dockerfile `builder-gate` stage: same shape as gateway, no file capability.
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

  // Dockerfile `slo` stage: toolbox image, crane/kubectl fetched and checksummed at build time —
  // the checksum check is the RUN chain's own sha256sum -c, kept in one exec so a failed checksum
  // fails the same layer it would in docker build.
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

  // Dockerfile `controller` stage: no capability, no hostPath, no secret — ca-certificates only,
  // same unprivileged-user shape as server/gateway/builder-gate.
  private imageController(built: Container): Container {
    return dag
      .container()
      .from(DEBIAN_SLIM)
      .withExec(["sh", "-c",
        "apt-get update && apt-get install -y --no-install-recommends ca-certificates " +
        "&& rm -rf /var/lib/apt/lists/*"])
      .withFile("/usr/local/bin/kloudlite-controller", built.file("/out/kloudlite-controller"))
      .withExec(["useradd", "--system", "--uid", "1001", "--user-group",
        "--no-create-home", "--shell", "/usr/sbin/nologin", "kloudlite"])
      .withUser("kloudlite")
      .withExposedPort(8080)
      .withEntrypoint(["kloudlite-controller"])
  }

  // Dockerfile `intercept-proxy` stage: FROM scratch — a static musl binary, no libc, no shell,
  // no USER (a scratch image has no /etc/passwd to name an account in; the pod spec runs it as
  // uid 1000 with a read-only root).
  private imageInterceptProxy(built: Container): Container {
    return dag
      .container()
      .withFile("/kloudlite-intercept-proxy", built.file("/out/musl/kloudlite-intercept-proxy"))
      .withEntrypoint(["/kloudlite-intercept-proxy"])
  }

  // Dockerfile `workspace` stage: the default workspace image, debian:bookworm-slim (glibc, for
  // npm's native-binding loaders), node/docker/buildx copied in from their own upstream images by
  // digest, no USER kl (root entrypoint is k8s::prelude, which drops to kl itself before exec'ing
  // sshd).
  private imageWorkspace(source: Directory, built: Container): Container {
    return dag
      .container()
      .from(DEBIAN_SLIM)
      .withDirectory("/usr/local", dag.container().from(WORKSPACE_NODE).directory("/usr/local"))
      .withFile("/usr/bin/docker", dag.container().from(DOCKER_CLI).file("/usr/local/bin/docker"))
      .withFile(
        "/usr/libexec/docker/cli-plugins/docker-buildx",
        dag.container().from(BUILDX_BIN).file("/buildx"),
      )
      .withExec(["sh", "-c", [
        "apt-get update",
        "apt-get install -y --no-install-recommends ca-certificates libstdc++6 libgcc-s1",
        "rm -rf /var/lib/apt/lists/*",
        "mkdir -p /var/empty",
        "groupadd -g 1000 kl",
        "useradd -u 1000 -g 1000 -m -d /home/kl -s /nix/profile/current/bin/zsh -p '*' kl",
        "useradd -r -d /var/empty -s /usr/sbin/nologin -p '*' sshd",
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
        "apt-get update && apt-get install -y --no-install-recommends python3 make g++ " +
        "&& npm install -g @nanonets/graft@0.18.0 " +
        "&& npm cache clean --force " +
        "&& apt-get purge -y python3 make g++ && apt-get autoremove -y " +
        "&& rm -rf /var/lib/apt/lists/*"])
      .withEnvVariable("DO_NOT_TRACK", "1")
  }

  // deploy/bench/Dockerfile: the graphcode TUI (run under bun), ttyd and sshd, supervised by
  // runit, plus the `sessions` node service (idle/readiness probe only). The TUI's deps install in
  // their own stage so a rebuild at the same HARNESS_REV is a cache hit.
  private imageBench(source: Directory, built: Container): Container {
    const harness = dag
      .container()
      .from(BUN_IMAGE)
      .withDirectory("/opt/kl/harness", dag.git("https://github.com/kloudlite/harness.git").commit(HARNESS_REV).tree())
      .withWorkdir("/opt/kl/harness")
      .withExec(["bun", "install", "--frozen-lockfile"])
    return dag
      .container()
      .from("node:24-bookworm-slim")
      .withExec(["sh", "-c",
        "apt-get update && apt-get install -y --no-install-recommends ca-certificates curl runit openssh-server " +
        "&& rm -rf /var/lib/apt/lists/*"])
      // ttyd: no Debian release packages it, so the upstream static build, pinned by digest.
      .withExec(["sh", "-c",
        "curl -fsSL -o /usr/local/bin/ttyd https://github.com/tsl0922/ttyd/releases/download/1.7.7/ttyd.x86_64 " +
        '&& echo "8a217c968aba172e0dbf3f34447218dc015bc4d5e59bf51db2f2cd12b7be4f55  /usr/local/bin/ttyd" | sha256sum -c - ' +
        "&& chmod 0755 /usr/local/bin/ttyd"])
      .withFile("/usr/local/bin/bun", dag.container().from(BUN_IMAGE).file("/usr/local/bin/bun"), { permissions: 0o755 })
      .withDirectory("/opt/kl/harness", harness.directory("/opt/kl/harness"))
      .withFile("/usr/local/bin/kl", built.file("/out/musl/kl"), { permissions: 0o755 })
      .withDirectory("/opt/kl/sessions", source.directory("bench/sessions"))
      .withDirectory("/opt/kl/term", source.directory("bench/term"))
      .withExec(["install", "-m", "0755", "/opt/kl/term/xclip", "/usr/local/bin/xclip"])
      .withDirectory("/etc/kl/sv", source.directory("bench/sv"), { owner: "1000:1000" })
      .withExec(["chmod", "0755", "/etc/kl/sv/sessions/run", "/etc/kl/sv/sshd/run", "/etc/kl/sv/ttyd/run", "/etc/kl/sv/term/run"])
      .withFile("/etc/kl/sshd_config", source.file("bench/sshd_config"))
      // Private TMPDIR, same as deploy/bench/Dockerfile.
      .withEnvVariable("TMPDIR", "/tmp/kl")
      .withExec(["sh", "-c",
        "usermod -l kl -d /home/kl node && groupmod -n kl node " +
        "&& mkdir -p /home/kl && chown kl:kl /home/kl"])
      .withUser("1000:1000")
      .withWorkdir("/home/kl")
      .withEntrypoint([])
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
      [() => this.imageController(built), "kloudlite-controller"],
      [() => this.imageBuilderGate(built), "kloudlite-builder-gate"],
      [() => this.imageSlo(built), "kloudlite-slo"],
      [() => this.imageWorkspace(source, built), "kloudlite-workspace"],
      [() => this.imageBench(source, built), "kloudlite-bench"],
      [() => this.imageInterceptProxy(built), "kloudlite-intercept-proxy"],
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
