# 部署

[English](deploy.md) | 简体中文

## Docker 镜像

仓库根目录的 `Dockerfile` 构建一个包含 `scion`（服务端、区域缓存、管理命令）和 `scn`（客户端）的镜像，以非 root 用户（uid 10001）运行。

预构建的镜像是 `opencsg-registry.cn-beijing.cr.aliyuncs.com/opencsghq/scion`（标签 `latest`、`0.1.0`），包含 `linux/amd64` 与 `linux/arm64` 两种架构，Linux 服务器和 Mac（Docker Desktop，Apple 芯片与 Intel）都能直接运行。自己构建多架构镜像时，编译阶段在构建机的架构上交叉编译，不需要 QEMU 模拟：

```sh
docker buildx build --platform linux/amd64,linux/arm64 -t <registry>/scion:latest --push .
# 访问不了 deb.debian.org 时指定 Debian 镜像源
docker buildx build --platform linux/amd64,linux/arm64 --build-arg DEBIAN_MIRROR=mirrors.ustc.edu.cn ...
```

```sh
docker build -t scion .
# 拉不到 Docker Hub 时换成镜像站
docker build -t scion \
  --build-arg RUST_IMAGE=docker.m.daocloud.io/library/rust:1.99-slim-bookworm \
  --build-arg RUNTIME_IMAGE=docker.m.daocloud.io/library/debian:bookworm-slim .
```

默认命令是 Lite 档：SQLite 加本地目录，数据放在卷 `/data`，并且**要求认证**（`--auth-required`）。第一次启动后，在容器里写入管理员策略、签发第一个令牌：

```sh
docker run -d --name scion -p 7080:7080 -v scion-data:/data scion

# 策略：admins 组管理一切，任何已认证身份可以读写、合入所有仓库（30 秒内生效）
docker exec scion sh -c 'printf "%s\n" "[[rule]]" "subjects = [\"group:admins\"]" "repos = [\"*\"]" "actions = [\"admin\"]" \
  "[[rule]]" "subjects = [\"*\"]" "repos = [\"*\"]" "actions = [\"land\"]" > /data/acl.toml \
  && scion admin policy set --lite /data/acl.toml'
# 第一个令牌（只显示这一次）
docker exec scion scion admin token create --lite --subject admin --group admins

scn --endpoint http://localhost:7080 login --token scn_pat_…
scn --endpoint http://localhost:7080 repo create mono
```

服务端监听在回环地址以外、又没有开认证时拒绝启动，因为能连上的人都能读写所有仓库。确实要匿名运行（例如只在内网测试）时加 `--allow-anonymous`（`SCION_ALLOW_ANONYMOUS=true`）：

```sh
docker run -d -p 7080:7080 -v scion-data:/data scion --log-format json server --listen 0.0.0.0:7080 \
  --lite --data-dir /data --allow-anonymous --repo mono      # 匿名，启动时预建仓库
```

| 端口 | 用途 |
|---|---|
| 7080 | `scion server` 的 gRPC |
| 7090 | `scion cache` 的 gRPC |
| 9464 | `--metrics-listen` 的 Prometheus 指标 |

所有参数都有对应的环境变量（`SCION_META`、`SCION_BLOBS`、`SCION_S3_*`、`SCION_AUTH_*`、`SCION_METRICS_LISTEN` 等），见 `scion server --help`。

## docker compose

[`deploy/compose/docker-compose.yml`](../../deploy/compose/docker-compose.yml) 用于开发和试用：密钥是固定的开发密钥，并且以 `SCION_ALLOW_ANONYMOUS=true` 匿名运行。它包含以下服务：
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
