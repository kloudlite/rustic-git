# Kloudlite architecture

[Live review task board](architecture-view/tasks.html)

Source: `desktop-login` at `695f8484`, reviewed 18 September 2026. This describes the code on that branch; it is not a claim that every change is deployed. The last verified fleet image during this session was `858b99ee`.

## Platform overview

```mermaid
flowchart TB
  subgraph Clients[Clients]
    Browser[Web browser]
    Desktop[Electron desktop · Solid renderer]
    CLI[Git / Docker / kl-connect]
  end

  subgraph Central[Central services · AKS]
    Web[Next.js web app]
    API[User API · /v1]
    Admin[Admin API · /admin]
    Server[Git + OCI server fleet]
    Worker[Merge and maintenance workers]
    Directory[(Cosmos DB · Mongo API\nPeople and teams)]
    Objects[(Object storage\nRepo and image SlateDBs\nGit packs · OCI blobs · auth keys)]
    Redis[(Redis\nCache and event notifications)]
    SLO[Synthetic user probes]
    History[(ClickHouse / ClickStack\nHistory · SLOs · telemetry)]
  end

  subgraph Region[Workspace region · k3s]
    Gateway[Gateway\nAuthenticated tunnels]
    Kube[(Kubernetes API / CRDs\nDesired and observed state)]
    Controller[Elected cluster controller\nShared resources and policies]
    Agent[Node agent · one per pool node\nPlacement · lifecycle · replication]
    Bench[Bench pods\nRemote AI sessions]
    Workspace[Workspace and environment pods\nCode · processes · services]
    Pool[(Node btrfs pools\nWorktrees · snapshots · replicas)]
    Homes[(Regional NFS homes\nSmall user configuration)]
  end

  Browser --> Web --> API
  Desktop -->|login and platform operations| API
  Desktop -->|kl-connect tunnel| Gateway
  CLI -->|Git HTTP / SSH and OCI| Server
  API -->|routed peer calls| Server
  API --> Directory
  API -->|desired state| Kube
  Admin -->|management and observation| Kube
  Server --> Objects
  Server <--> Redis
  Worker -->|merge and maintenance| Server
  Worker <--> Redis
  Admin --> History
  SLO -->|exercise public journeys| API
  SLO -->|report results| Admin
  Gateway --> Bench
  Gateway --> Workspace
  Kube --> Controller
  Kube --> Agent
  Controller -->|shared state| Kube
  Agent -->|reconcile| Bench
  Agent -->|reconcile| Workspace
  Agent --> Pool
  Workspace --> Homes
  Bench --> Pool
  Workspace --> Pool
```

The two storage paths are separate. Repositories and registry images use object storage and SlateDB. Workspace snapshots use btrfs and node-to-node replication; they do not pass through the Git server or an object-store snapshot service.

Exactly one server node may open each repository/image database as its writer. Requests route to that owner before touching the database. Kubernetes CRDs are the record for workspace state; Redis is a cache and notification path.

## Desktop, sessions, and tools

```mermaid
flowchart LR
  UI[Electron desktop\nTabs · transcript · plan · approvals]
  Tunnel[kl-connect + regional gateway]

  subgraph BenchPod[Bench pod · per person and team]
    Runtime[harness-bench · port 7789\nHTTP API + WebSocket events]
    BenchSession[Bench session\nIntent and coordination]
    WorkspaceSession[Workspace session\nImplementation context]
    AgentSession[Agent session\nOne workspace tree]
    State[(Bench worktree / .bench\nSessions · queues · plans · memory)]
    PersonShell[Shell sidecar · ttyd\nPerson's home and profile]
  end

  subgraph WorkspacePod[Workspace pod]
    Tools[kl ide serve · port 7788\nWorkspace token required]
    MainTree[Main workspace tree]
    AgentTree[Agent tree · .agents/name\nNested btrfs snapshot]
    Exec[bwrap-wrapped execution\nProcesses and file operations]
  end

  Platform[User API\nWorkspace and environment operations]
  Model[Configured model provider]

  UI <--> Tunnel <--> Runtime
  Runtime --> BenchSession
  Runtime --> WorkspaceSession
  Runtime --> AgentSession
  Runtime --> State
  BenchSession -->|ask / report| WorkspaceSession
  BenchSession -->|platform tools| Platform
  WorkspaceSession -->|authenticated remote tools| Tools
  AgentSession -->|authenticated tools with tree| Tools
  Tools --> MainTree
  Tools --> AgentTree
  Tools --> Exec
  BenchSession <--> Model
  WorkspaceSession <--> Model
  AgentSession <--> Model
  UI -.->|separate terminal connection| PersonShell
```

All model processes run in the bench runtime, including workspace and agent sessions. A workspace session's tools execute remotely in its workspace. The bench session coordinates work and holds product intent; it has no direct file or execution tools.

The shell sidecar is a separate user surface. Its home-only mounts exclude workspace code and the workspace tool token. It is not the model's execution environment.

State ownership follows four levels: app (connection/team), workspace (files/processes), session (transcript/plan/queue/questions), and tab (open file/scroll/draft).

## Workspace builds and persistence

```mermaid
flowchart LR
  API[User API] -->|write spec| CRD[(Workspace / Volume / Snapshot CRDs)]
  CRD --> Agent[Node agent]
  Agent -->|materialize and reconcile| Live[(Writable btrfs worktree)]
  Live -->|push or sync cut| Snap[(Read-only btrfs snapshot)]
  Snap -->|btrfs send / receive| Replica[(Replica on another pool node)]
  Snap -->|clone or restore| Other[(New writable worktree)]
  Workspace[Workspace build client] --> Gate[Builder gate\nIdentify caller by pod IP]
  Gate -->|start and proxy| Buildkit[Owner's on-demand BuildKit]
  Buildkit -->|push image| Registry[OCI registry server]
  Registry --> Blobs[(Object-store image blobs)]
```

Pushed snapshots are retained until explicitly deleted; sync points support replication and lifecycle operations. Workspace caches travel with the worktree. Regional NFS homes keep configuration. Agent trees are separate nested subvolumes and are not ordinary files inside the parent's replicated snapshot.

## Source map

| Component | Primary code |
|---|---|
| Web app | `web/apps/web` |
| Desktop | `harness/src` |
| Session runtime, tools and skills | `harness/bench/src`, `harness/pi`, `harness/skills` |
| User/admin API | `bins/api`, `crates/api`, `crates/workspaces/src/api` |
| Git and OCI servers | `bins/server`, `crates/app`, `crates/git`, `crates/gitbase`, `crates/registry`, `crates/storage` |
| Merge and maintenance | `bins/worker`, `crates/pulls` |
| Regional controller and node agents | `bins/controller`, `bins/agent`, `crates/workspaces` |
| Gateway, tool server and build access | `bins/gateway`, `bins/kl-connect`, `crates/ide`, `bins/builder-gate` |
| Probes and history | `bins/slo`, `crates/workspaces/src/history` |
| Deployment | `deploy/kloudlite.yaml`, `deploy/k3s`, `deploy/dev` |

Arrows show principal interactions, not every HTTP request or telemetry export. OpenTelemetry collectors across both clusters send telemetry to ClickStack; the admin process writes the product's history and SLO records. External OAuth, email delivery, package indexes and build caches are omitted from the overview.


## Registry generation hardening

The diagrams above describe baseline `695f8484`. The subsequent hardening work adds an object-store CAS record for each owner/digest and immutable physical blob generations. This is an implementation/migration design, not a statement that the fleet has migrated. Completion evidence lives in the [hardening tracker](review-hardening-2026-09-18.md).

```mermaid
flowchart LR
  Publisher[Manifest publisher] -->|CAS publication pin| State[Per-digest state and nonce]
  Publisher -->|check pinned bytes| Active[Active immutable generation]
  Publisher -->|store original bytes| Manifests[Manifest objects]
  Publisher -->|release pin after known publication| State
  Collector[Registry collector] -->|capture versions before scan| State
  Collector -->|scan references| Manifests
  Collector -->|CAS retire if unchanged and unpinned| State
  State -->|record retired physical keys| Retired[Retired generations]
  Collector -->|delete exact retired key| Retired
  Upload[Verified upload] -->|fresh generation key| Active
  Upload -->|CAS installation or deduplication| State
```

Legacy canonical blobs can be adopted, but an existing state record is authoritative. Reader/writer/collector compatibility and pending operations constrain migration and rollback. Use the proposed [maintenance and recovery procedure](registry-generation-rollout.md); an ordinary mixed-version rolling upgrade is not sufficient.

The current backup inventory is [deploy/BACKUPS.md](../deploy/BACKUPS.md). Workspace data continues to use btrfs replication, independently of this registry change. Generation records and referenced physical bytes must be retained/restored together.
