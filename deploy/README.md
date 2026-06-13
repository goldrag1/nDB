# Deploying nDB

Three ways to run nDB, smallest to largest. All use the same image
(`ghcr.io/goldrag1/ndb`) and the follower mode from
[PROTOCOL.md](../docs/PROTOCOL.md) / [PRODUCTION.md](../docs/PRODUCTION.md).

## 1. Single binary / systemd

```sh
ndb-server --path /var/lib/ndb --bind 0.0.0.0:8742
```

A minimal systemd unit:

```ini
[Unit]
Description=nDB
After=network.target
[Service]
ExecStart=/usr/local/bin/ndb-server --path /var/lib/ndb --bind 0.0.0.0:8742
Restart=on-failure
User=ndb
[Install]
WantedBy=multi-user.target
```

## 2. docker-compose — a leader + auto-following replica

From the repo root:

```sh
docker compose up --build
```

- leader   → http://localhost:8742 (writes + reads)
- follower → http://localhost:8743 (reads only; streams from the leader)

Write to `:8742`, read from either. A write to `:8743` returns `403 read_only`.
The follower bootstraps from the leader's base backup, then streams commits.

## 3. Kubernetes — Helm chart (StatefulSet)

```sh
helm install ndb ./deploy/helm/ndb \
  --set replicaCount=3 \
  --set persistence.size=20Gi
```

- Pod `ndb-0` is the **leader** (writes); `ndb-1..N` are read-only **replicas**
  that bootstrap from and follow `ndb-0`.
- Reads: the `ndb` Service load-balances across all pods.
- Writes: target the leader pod via the headless Service
  (`ndb-0.ndb-headless`).
- Probes: liveness `GET /v1/health`, readiness `GET /v1/ready`.
- Auth: set `auth.existingSecret` (a Secret with key `token`) for production,
  or `auth.token` inline for dev.

See `deploy/helm/ndb/values.yaml` for all knobs.

## Automatic failover

Not yet automated — promoting a replica to leader is a supervised procedure
(see the runbook in [PRODUCTION.md](../docs/PRODUCTION.md)). Automatic leader
election (Raft) is a planned follow-up; the byte-identical replication makes a
coordinator tractable.
