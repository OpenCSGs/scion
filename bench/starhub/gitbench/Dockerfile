# gitbench runs next to Gitaly, in the same Docker VM, on a bare copy of the same repository.
ARG RUST_IMAGE=rust:1.99-slim-bookworm
FROM ${RUST_IMAGE}
ARG DEBIAN_MIRROR=
RUN if [ -n "$DEBIAN_MIRROR" ]; then sed -i "s|deb.debian.org|$DEBIAN_MIRROR|g" /etc/apt/sources.list.d/debian.sources; fi \
    && apt-get update && apt-get install -y --no-install-recommends git && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml ./
COPY src src
RUN --mount=type=cache,target=/usr/local/cargo/registry cargo build --release && cp target/release/gitbench /usr/local/bin/
ENTRYPOINT ["gitbench"]
