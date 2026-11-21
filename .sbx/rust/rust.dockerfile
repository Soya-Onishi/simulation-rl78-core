# syntax=docker/dockerfile:1
# Install rustup into a relocatable prefix and copy it onto scratch so the
# overlay does not depend on the workload's package database. CARGO_HOME
# must stay agent-writable for registry/git caches at runtime.
FROM dhi.io/debian-base:trixie-dev AS build
ARG RUST_TOOLCHAIN=stable
ARG TARGETARCH
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/out/usr/local/rustup \
    CARGO_HOME=/out/usr/local/cargo \
    PATH=/out/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

RUN set -eux; \
    mkdir -p "$RUSTUP_HOME" "$CARGO_HOME"; \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --default-toolchain "${RUST_TOOLCHAIN}" --profile minimal --no-modify-path; \
    rustup component add rustfmt clippy rust-analyzer; \
    rustc --version; \
    cargo --version; \
    rust-analyzer --version; \
    # Agent uid must own cargo/rustup trees so registry caches and
    # occasional rustup updates work without root.
    chown -R 1000:1000 /out/usr/local/rustup /out/usr/local/cargo; \
    # Intermediate dirs stay root-owned like a normal /usr/local tree.
    chown 0:0 /out /out/usr /out/usr/local

FROM scratch
COPY --from=build /out /
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin
