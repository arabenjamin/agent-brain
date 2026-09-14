# Rust compile/test sandbox for `execute_code(language: "rust")`.
#
# Same isolation contract as ../sandbox (see docker-compose.yml): internal-only
# network, no credentials, no bind mounts, read-only root. Two differences, both
# measured rather than assumed:
#
#   - Crates are fetched HERE, at build time, into the image's CARGO_HOME. At
#     run time the network is gone and cargo runs --offline against this cache,
#     which is read-only and works that way (verified 2026-09-12).
#   - The run scratch must allow exec: Rust compiles test binaries and build
#     scripts and has to run them. That lives in compose, not here.
FROM rust:1.98.1-slim-trixie

RUN rustup component add clippy rustfmt \
 && apt-get update \
 && apt-get install -y --no-install-recommends python3 \
 && rm -rf /var/lib/apt/lists/*

# Populate the offline registry cache from the declarative crate list.
COPY crates/Cargo.toml /opt/seed/Cargo.toml
RUN mkdir -p /opt/seed/src \
 && echo 'fn main() {}' > /opt/seed/src/main.rs \
 && cd /opt/seed \
 && cargo fetch \
 && rm -rf /opt/seed

# Not UID 1000: RLIMIT_NPROC is counted per real UID across the whole kernel,
# so sharing the host user's UID means sharing its process count.
RUN useradd -m -u 10001 sandbox

COPY server.py /usr/local/bin/rust-sandbox-server.py

USER sandbox
WORKDIR /work

EXPOSE 8000

HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
    CMD python3 -c "import urllib.request;urllib.request.urlopen('http://localhost:8000/health').read()"

CMD ["python3", "/usr/local/bin/rust-sandbox-server.py"]
