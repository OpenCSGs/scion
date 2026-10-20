# Deployment

English | [简体中文](deploy.zh-CN.md)

## Docker image

The `Dockerfile` at the repository root builds an image with `scion` (the server, the regional cache and the admin commands) and `scn` (the client). It runs as an unprivileged user (uid 10001).

The prebuilt image is `opencsg-registry.cn-beijing.cr.aliyuncs.com/opencsghq/scion` (tags `latest` and `0.1.0`), for `linux/amd64` and `linux/arm64`: it runs on Linux servers and in Docker Desktop on Macs, Apple silicon and Intel alike. A multi-arch build of your own cross-compiles on the build machine's architecture, without QEMU emulation:

```sh
docker buildx build --platform linux/amd64,linux/arm64 -t <registry>/scion:latest --push .
# When deb.debian.org is unreachable, name a Debian mirror
docker buildx build --platform linux/amd64,linux/arm64 --build-arg DEBIAN_MIRROR=mirrors.ustc.edu.cn ...
```

```sh
docker build -t scion .
# When Docker Hub is unreachable, use a mirror
docker build -t scion \
  --build-arg RUST_IMAGE=docker.m.daocloud.io/library/rust:1.99-slim-bookworm \
  --build-arg RUNTIME_IMAGE=docker.m.daocloud.io/library/debian:bookworm-slim .
```

The default command runs the Lite tier, SQLite and a local directory, with its data in the volume `/data`, and **requires authentication** (`--auth-required`). After the first start, write an administrator policy and issue the first token inside the container:

```sh
docker run -d --name scion -p 7080:7080 -v scion-data:/data scion

# Policy: the admins group manages everything; anyone authenticated can read, write and land in
# every repository (takes effect within 30 s)
docker exec scion sh -c 'printf "%s\n" "[[rule]]" "subjects = [\"group:admins\"]" "repos = [\"*\"]" "actions = [\"admin\"]" \
  "[[rule]]" "subjects = [\"*\"]" "repos = [\"*\"]" "actions = [\"land\"]" > /data/acl.toml \
  && scion admin policy set --lite /data/acl.toml'
# The first token (shown only this once)
docker exec scion scion admin token create --lite --subject admin --group admins

scn login http://localhost:7080 --token scn_pat_…
scn repo create http://localhost:7080/mono
```

The server refuses to start when it listens beyond loopback without authentication, since anyone who can reach it could then read and write every repository. To run anonymously anyway (for example, for a test on an internal network), add `--allow-anonymous` (`SCION_ALLOW_ANONYMOUS=true`):

```sh
docker run -d -p 7080:7080 -v scion-data:/data scion --log-format json server --listen 0.0.0.0:7080 \
  --lite --data-dir /data --allow-anonymous --repo mono      # anonymous, creating a repository at start
```

| Port | Use |
|---|---|
| 7080 | gRPC of `scion server` |
| 7090 | gRPC of `scion cache` |
| 9464 | Prometheus metrics of `--metrics-listen` |

Every option has an environment variable (`SCION_META`, `SCION_BLOBS`, `SCION_S3_*`, `SCION_AUTH_*`, `SCION_METRICS_LISTEN` and so on); see `scion server --help`.

## docker compose

[`deploy/compose/docker-compose.yml`](../../deploy/compose/docker-compose.yml) is for development and trials: its secrets are fixed development secrets, and it runs anonymously with `SCION_ALLOW_ANONYMOUS=true`. Its services:
- PostgreSQL, the metadata database of the Standard tier;
- SeaweedFS, S3-compatible object storage, which creates the bucket `scion-objects` at start;
- `scion server`;
- optionally, the regional cache `scion cache`.

```sh
docker compose -f deploy/compose/docker-compose.yml up -d --build
# Through a registry mirror
REGISTRY=docker.m.daocloud.io docker compose -f deploy/compose/docker-compose.yml up -d --build
# With the regional cache as well (7090)
docker compose -f deploy/compose/docker-compose.yml --profile cache up -d

scn doctor http://localhost:7080
scn repo create http://localhost:7080/demo
scn clone http://localhost:7080/demo
curl -s localhost:9464/metrics | head
```

### What to change for production

- **Secrets:** the database password (`SCION_DB_PASSWORD`) and the S3 keys (`deploy/dev/s3.json`) are for development only.
- **Authentication:** turn on `--auth-required` and configure a signing key or an external issuer; see [auth.md](auth.md).
- **Object storage:** use a real S3 or MinIO service; see [object-storage.md](object-storage.md).
- **Several nodes:** set `--advertise-url` and `--cluster-secret`; see [cluster.md](cluster.md).

## Health checks

- **The standard gRPC health check:** `grpc.health.v1.Health` needs no authentication and suits Kubernetes' gRPC probes.
- **The check in compose:** the image's own `scn doctor --endpoint http://127.0.0.1:7080`, which completes a capability negotiation.
