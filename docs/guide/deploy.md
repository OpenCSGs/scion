# 部署

## Docker 镜像

仓库根目录的 `Dockerfile` 构建一个包含 `scion`（服务端、区域缓存、管理命令）和 `scn`（客户端）的镜像，以非 root 用户（uid 10001）运行。

```sh
docker build -t scion .
# 拉不到 Docker Hub 时换成镜像站
docker build -t scion \
  --build-arg RUST_IMAGE=docker.m.daocloud.io/library/rust:1.99-slim-bookworm \
  --build-arg RUNTIME_IMAGE=docker.m.daocloud.io/library/debian:bookworm-slim .
```

默认命令是 Lite 档：SQLite 加本地目录，数据放在卷 `/data`。

```sh
docker run -d -p 7080:7080 -v scion-data:/data scion
docker run -d -p 7080:7080 -v scion-data:/data scion --log-format json server --listen 0.0.0.0:7080 \
  --lite --data-dir /data --repo mono      # 启动时预建仓库
```

| 端口 | 用途 |
|---|---|
| 7080 | `scion server` 的 gRPC |
| 7090 | `scion cache` 的 gRPC |
| 9464 | `--metrics-listen` 的 Prometheus 指标 |

所有参数都有对应的环境变量（`SCION_META`、`SCION_BLOBS`、`SCION_S3_*`、`SCION_AUTH_*`、`SCION_METRICS_LISTEN` 等），见 `scion server --help`。

## docker compose

[`deploy/compose/docker-compose.yml`](../../deploy/compose/docker-compose.yml) 包含以下服务：
- PostgreSQL：Standard 档的元数据库；
- SeaweedFS：S3 兼容对象存储，启动时创建 bucket `scion-objects`；
- `scion server`；
- 可选的区域缓存 `scion cache`。

```sh
docker compose -f deploy/compose/docker-compose.yml up -d --build
# 镜像站
REGISTRY=docker.m.daocloud.io docker compose -f deploy/compose/docker-compose.yml up -d --build
# 同时启动区域缓存（7090）
docker compose -f deploy/compose/docker-compose.yml --profile cache up -d

scn --endpoint http://localhost:7080 doctor
scn --endpoint http://localhost:7080 repo create demo
scn --endpoint http://localhost:7080 clone demo
curl -s localhost:9464/metrics | head
```

### 生产环境需要修改的地方

- **密钥**：数据库密码（`SCION_DB_PASSWORD`）和 S3 密钥（`deploy/dev/s3.json`）只能用于开发。
- **认证**：开启 `--auth-required`，并配置签名密钥或外部签发方，见 [auth.md](auth.md)。
- **对象存储**：改用正式的 S3 或 MinIO 服务，见 [object-storage.md](object-storage.md)。
- **多节点**：设置 `--advertise-url` 和 `--cluster-secret`，见 [cluster.md](cluster.md)。

## 健康检查

- **gRPC 标准健康检查**：`grpc.health.v1.Health` 不需要认证，可以用于 Kubernetes 的 gRPC probe。
- **compose 中的检查**：用镜像自带的 `scn doctor --endpoint http://127.0.0.1:7080`，它会完成一次能力协商。
