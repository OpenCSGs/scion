# Scion 镜像：包含服务端 `scion` 和客户端 `scn`。
#
#   docker build -t scion .
#   docker run -p 7080:7080 -v scion-data:/data scion            # Lite 档（SQLite + 本地目录）
#
# 拉不到 Docker Hub 时把基础镜像换成镜像站，例如
#   --build-arg RUST_IMAGE=docker.m.daocloud.io/library/rust:1.99-slim-bookworm \
#   --build-arg RUNTIME_IMAGE=docker.m.daocloud.io/library/debian:bookworm-slim
ARG RUST_IMAGE=rust:1.99-slim-bookworm
ARG RUNTIME_IMAGE=debian:bookworm-slim

FROM ${RUST_IMAGE} AS build
# release 用于发布；CI 的冒烟测试用 dev 缩短编译时间
ARG CARGO_PROFILE=release
WORKDIR /src
COPY . .
# 依赖与编译结果放在 BuildKit 缓存里，改代码后重新构建只编译变化的 crate
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --profile "${CARGO_PROFILE}" --locked -p scion -p scion-cli \
    && dir=$([ "${CARGO_PROFILE}" = dev ] && echo debug || echo "${CARGO_PROFILE}") \
    && mkdir -p /out \
    && cp "target/${dir}/scion" "target/${dir}/scn" /out/

FROM ${RUNTIME_IMAGE}
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data scion \
    && mkdir -p /data \
    && chown scion /data
COPY --from=build /out/scion /out/scn /usr/local/bin/
USER scion
# 镜像里的 scn 直连服务端，不拉起本机 daemon
ENV SCION_NO_DAEMON=1
# 7080 gRPC（scion server）、7090 gRPC（scion cache）、9464 Prometheus 指标（--metrics-listen）
EXPOSE 7080 7090 9464
VOLUME ["/data"]
ENTRYPOINT ["scion"]
CMD ["--log-format", "json", "server", "--listen", "0.0.0.0:7080", "--lite", "--data-dir", "/data"]
