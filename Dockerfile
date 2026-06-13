# Multi-stage build for the nDB server.
#
#   docker build -t ndb .
#   docker run -p 8742:8742 -v ndb-data:/data ndb
#
# The server binds 0.0.0.0:8742 inside the container and stores the database
# under /data (mount a volume to persist it). Health: GET /v1/health.

# ---- build stage ----------------------------------------------------------
FROM rust:1-bookworm AS build
WORKDIR /src
# Copy the whole workspace (Cargo.toml + crates). A .dockerignore keeps
# target/ and node_modules/ out so the context stays small.
COPY . .
# Build only the server binary in release mode.
RUN cargo build --release -p ndb-server && \
    cp target/release/ndb-server /usr/local/bin/ndb-server

# ---- runtime stage --------------------------------------------------------
FROM debian:bookworm-slim AS runtime
# ca-certificates for outbound TLS (replication/backup to TLS endpoints);
# tini for correct PID-1 signal handling (graceful shutdown).
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates tini && \
    rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/ndb-server /usr/local/bin/ndb-server

# Non-root runtime user owning the data dir.
RUN useradd --system --uid 10001 --home /data ndb && \
    mkdir -p /data && chown ndb:ndb /data
USER ndb
VOLUME ["/data"]
EXPOSE 8742

ENTRYPOINT ["/usr/bin/tini", "--", "ndb-server"]
CMD ["--path", "/data", "--bind", "0.0.0.0:8742"]
