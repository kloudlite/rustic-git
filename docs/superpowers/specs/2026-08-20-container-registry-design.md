# A container registry on the git nodes

The shell already promises one. `sections.ts` ships a Container Images tab pointing at
`/{owner}/registries`, and there is no such route and no registry code anywhere in `src/`. This
design implements the thing the tab claims: an OCI registry, spec v1.1, served by the same binary
on the same port, `docker push` and `docker pull` against it.

Extends the ownership design. The map, the leases, the forwarding are untouched — images route
through them exactly as repositories do, under a key that cannot collide with one.

## What an image is

An image is `{owner}/{name}` in a namespace of its own. It is NOT tied to a git repo: pushing
`acme/nginx` requires no repository called `nginx`, and a repository called `nginx` grants no
claim on the image name. Names are one segment (`store::valid_segment`), so `/v2/{owner}/{name}`
is the whole shape and the nested names other registries allow are refused. Two segments keep the
routing middleware unambiguous, which is the property everything below depends on.

Images are created implicitly by the first push, the way every registry does it. There is no
create step and no reserved-name list — the registry's URL space has nothing else in it.

## The one decision everything follows from

`acme/nginx` the repository and `acme/nginx` the image are different objects that would derive
the same routing key, the same pool entry, and adjacent storage paths. If that key were shared,
one would be served out of the other's database.

So registry keys are namespaced at every layer:

```
routing key       img/{owner}/{name}        (repo_of, for /v2 paths)
ownership map     own/img/{owner}/{name}    (ownership::key already prefixes own/)
pool coordinates  ("img", "{owner}/{name}") (so pool::path yields repo/img/{owner}/{name})
blobs             blobs/{owner}/sha256/{digest}
manifest bytes    manifests/{owner}/{name}/{digest}
```

`img` and `v2` both join `api` as reserved owner names (`store::valid_owner`). That is what makes
the pool coordinates safe: every place in `lib.rs` that turns a routing key back into a repo does
`repo.split_once('/')`, so `img/acme/nginx` round-trips to `("img", "acme/nginx")` and back
unchanged — the image key behaves like any other repo key through claim, renew, evict, and
release, with no change to any of them. Reserving `img` is what stops a git repo owned by `img`
from nesting its database inside an image's prefix. The two namespaces cannot meet.

## Routing

`/v2` joins `api` as a reserved first segment in `http.rs`. `repo_of` maps
`/v2/{owner}/{name}/...` to `img/{owner}/{name}`; the existing middleware then claims, forwards,
or serves exactly as it does for a repo. A misrouted registry request is impossible for the same
reason a misrouted git request is.

Two endpoints carry no image and must NOT route — they are answered locally by whichever node
receives them:

```
GET  /v2/                             → 200, or 401 with the challenge
GET  /v2/token                        → a scoped bearer token
GET  /v2/_catalog?n=&last=            → the caller's images
```

`_catalog` is answered locally too: it is an object-store list, not a database read, so it needs
no owning node.

Everything else is image-scoped and routes:

```
GET  /v2/{o}/{n}/blobs/{digest}                        pull a layer
HEAD /v2/{o}/{n}/blobs/{digest}
POST /v2/{o}/{n}/blobs/uploads/                        start a session (or mount, or single-POST)
PATCH/v2/{o}/{n}/blobs/uploads/{uuid}                  one chunk
PUT  /v2/{o}/{n}/blobs/uploads/{uuid}?digest=          complete
GET  /v2/{o}/{n}/blobs/uploads/{uuid}                  session status
DELETE /v2/{o}/{n}/blobs/uploads/{uuid}                cancel
DELETE /v2/{o}/{n}/blobs/{digest}
GET/HEAD/PUT/DELETE /v2/{o}/{n}/manifests/{reference}  tag or digest
GET  /v2/{o}/{n}/tags/list?n=&last=
GET  /v2/{o}/{n}/referrers/{digest}?artifactType=
```

Errors are the spec's JSON envelope — `{"errors":[{"code","message","detail"}]}` — with the
codes the conformance suite checks (`BLOB_UNKNOWN`, `MANIFEST_UNKNOWN`, `NAME_UNKNOWN`,
`DIGEST_INVALID`, `SIZE_INVALID`, `UNAUTHORIZED`, `DENIED`, `UNSUPPORTED`).

## Storage

**Blobs are per-owner**, at `blobs/{owner}/sha256/{digest}`. A team that pushes twenty images off
one base layer stores that layer once, and cross-repo mount inside the team is a metadata write.
Across teams the layer is stored twice — the price of a garbage collector that only ever has to
read one team's images to know what is unreferenced, and of a team deletion that is one prefix
delete. Cross-owner mount is refused with the spec's fallback: 202 and a normal upload session.

**Manifest bytes** go to the object store, content-addressed. **Tags** do not: the tag map lives
in the image's SlateDB, so two concurrent pushes to `:latest` order against each other instead of
racing in the object store. Upload sessions and the referrers index live there too.

**Listing** — `_catalog` and the web page — is an object-store list with a delimiter over
`images/{owner}/`. There is no separate index of images, so there is no index to fall out of sync
with what was actually pushed. `_catalog` is scoped to what the caller can read.

## Authentication, both ways

A 401 carries the standard challenge:

```
WWW-Authenticate: Bearer realm="https://{host}/v2/token",service="{host}",scope="repository:{o}/{n}:pull,push"
```

`/v2/token` authenticates the credential through `auth.rs` (`owner_for_token`) and mints a
short-lived JWT through `jwt.rs`, carrying the granted scope. This is what ghcr and ECR do and
what tooling that assumes a token flow requires.

Every `/v2` handler ALSO accepts `Authorization: Basic` directly against `owner_for_token`, so a
client that ignores the challenge — or a `curl` in a debugging session — works without the round
trip. Both paths end at the same authorization call.

Images carry visibility as repositories do. A public image pulls anonymously; a private one
answers 401 with the challenge on every method, including HEAD, so a probe cannot distinguish
"absent" from "not yours".

## Uploads

`POST /blobs/uploads/` opens an S3 multipart upload and records its id, the session uuid, and the
bytes received in the image's database. `PATCH` uploads one part; ranges must be sequential, per
the spec, and a gap is `416`. `PUT` completes the multipart and verifies the digest against the
`digest` query parameter, `400 DIGEST_INVALID` on mismatch. The single-request forms —
`POST?digest=` and `POST` then `PUT` with the whole body — bypass multipart and put the object
directly.

The running sha256 is held in memory on the node that owns the image; `sha2`'s state does not
serialize. If the image moves nodes mid-upload the session is gone and the client gets `404
BLOB_UPLOAD_UNKNOWN`, which the spec permits and every client retries. Marked with a `ponytail:`
comment naming the upgrade path — re-hash the assembled object from the object store on
completion, at the cost of reading every layer twice.

Layer size is bounded by an env-configured maximum, checked against `Content-Length` before the
first byte is accepted, so an unbounded push cannot fill a node's disk.

## Deletes and garbage

`DELETE /manifests/{digest}` removes the manifest and every tag pointing at it. `DELETE
/manifests/{tag}` removes the tag alone. `DELETE /blobs/{digest}` removes the object.

Nothing deletes a blob because a manifest stopped referencing it — that is the sweeper's job.
The existing worker (`bin/worker.rs`, `gc.rs`) gains a pass that, for one owner, reads every
image's manifests, collects the referenced digests, and deletes blobs under `blobs/{owner}/` that
are older than a grace window and appear in none of them. The grace window is what keeps it from
deleting a layer that an in-flight push has stored but not yet referenced.

## The web page

`/{owner}/registries` — the route the tab has been pointing at — lists the team's images with tag
count and last push, and an image page lists tags with digest, size, and the `docker pull` line.
Read through the browse API on the peer listener, like every other page. No new Service, no new
ingress: same binary, same port, `deploy/` unchanged.

## Testing

- **Routing**: unit tests that `img/` keys derive correctly and that no git path can produce one —
  the bug class here serves one image's data under another's name, silently.
- **Endpoints**: integration tests per conformance group — push, pull, content discovery, content
  management — asserting status codes and error codes, not just success paths.
- **Round trip**: a real `docker push` and `docker pull` against a local node, plus `docker login`
  through the token flow and through Basic.
- **Concurrency**: two simultaneous pushes to the same tag resolve to one winner and one
  consistent read.
- **GC**: a blob referenced by a second image is not swept when the first is deleted.
