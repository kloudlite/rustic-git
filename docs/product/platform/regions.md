# Regions

A region is one Kubernetes cluster: nodes with fast local disk, a shared home export, its own nixpkgs pin, and a gateway. Every workspace and environment names one and stays in it.

```bash [API]
curl -sS https://dev.kloudlite.io/v1/regions -H "Authorization: Bearer $KL_TOKEN"
```

```json
[{ "id": "centralindia-k3s", "name": "Central India", "status": "active" }]
```

## What is per region

- Nodes, and the replicas of every volume.
- Your home directory.
- The builder.
- The nixpkgs pin a bare package name resolves against.

Nothing replicates between regions. A snapshot in one region cannot be restored in another.

## Asking for a region

A region you do not see is a [request](requests.md) of kind `region`.
