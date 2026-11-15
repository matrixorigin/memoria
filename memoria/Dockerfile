# Multi-stage build for Memoria Rust API
# Note: http_proxy/https_proxy are predefined Docker build args —
# passed via docker-compose build.args, auto-available in all stages.

# ── Stage 1: deps — cache dependency build ──────────────────────────────────
ARG RUST_VERSION=1.85
FROM rust:${RUST_VERSION}-slim-bookworm AS builder

# Rust crates mirror (USTC)
RUN printf '[source.crates-io]\nreplace-with = "mirror"\n[source.mirror]\nregistry = "sparse+https://mirrors.ustc.edu.cn/crates.io-index/"\n' \
    > $CARGO_HOME/config.toml

# Debian mirror (aliyun)
RUN sed -i 's|deb.debian.org|mirrors.aliyun.com|g' /etc/apt/sources.list.d/debian.sources 2>/dev/null; \
    sed -i 's|deb.debian.org|mirrors.aliyun.com|g' /etc/apt/sources.list 2>/dev/null; true
RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy only manifests first → deps layer cached unless Cargo.toml/lock changes
COPY memoria/Cargo.toml memoria/Cargo.lock ./
COPY memoria/crates/memoria-api/Cargo.toml crates/memoria-api/Cargo.toml
COPY memoria/crates/memoria-cli/Cargo.toml crates/memoria-cli/Cargo.toml
COPY memoria/crates/memoria-core/Cargo.toml crates/memoria-core/Cargo.toml
COPY memoria/crates/memoria-embedding/Cargo.toml crates/memoria-embedding/Cargo.toml
COPY memoria/crates/memoria-git/Cargo.toml crates/memoria-git/Cargo.toml
COPY memoria/crates/memoria-mcp/Cargo.toml crates/memoria-mcp/Cargo.toml
COPY memoria/crates/memoria-service/Cargo.toml crates/memoria-service/Cargo.toml
COPY memoria/crates/memoria-storage/Cargo.toml crates/memoria-storage/Cargo.toml

# Create dummy src files so cargo can resolve the workspace
RUN for d in crates/*/; do mkdir -p "$d/src" && echo "" > "$d/src/lib.rs"; done && \
    mkdir -p crates/memoria-api/src && echo "fn main(){}" > crates/memoria-api/src/main.rs && \
    mkdir -p crates/memoria-cli/src && echo "fn main(){}" > crates/memoria-cli/src/main.rs && \
    mkdir -p crates/memoria-mcp/src && echo "fn main(){}" > crates/memoria-mcp/src/main.rs

# Build deps only (cached layer — errors expected from dummy sources)
ENV SQLX_OFFLINE=true
RUN cargo build --release || true

# Now copy real source and build
COPY memoria/ .
RUN cargo build --release --bin memoria

# ── Stage 2: runtime ───────────────────────────────────────────────────────
FROM debian:bookworm-slim
RUN sed -i 's|deb.debian.org|mirrors.aliyun.com|g' /etc/apt/sources.list.d/debian.sources 2>/dev/null; \
    sed -i 's|deb.debian.org|mirrors.aliyun.com|g' /etc/apt/sources.list 2>/dev/null; true
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/* && \
    useradd -m -u 1000 appuser && \
    mkdir -p /var/log/memoria && chown appuser:appuser /var/log/memoria
COPY --from=builder /build/target/release/memoria /usr/local/bin/
USER appuser
EXPOSE 8100
CMD ["sh", "-c", "memoria serve 2>&1 | tee -a /var/log/memoria/api.log"]
