# =============================================================================
# Stage 1: Build
# =============================================================================
# Debian release matches the runtime stage; the compiler version comes from
# rust-toolchain.toml (rustup installs it on the first cargo call).
FROM rust:1-trixie AS builder

WORKDIR /app

# Install build dependencies (OpenSSL for neo4rs)
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy workspace manifests first for dependency caching
# Cargo.lock is committed and every build is --locked: the image gets exactly
# the versions the host tested. Update them with scripts/update_deps.sh.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
# Patched dependency ([patch.crates-io] in Cargo.toml), needed to resolve.
COPY vendor/ ./vendor/
COPY crates/protocol/Cargo.toml ./crates/protocol/
COPY crates/models/Cargo.toml    ./crates/models/
COPY crates/repository/Cargo.toml ./crates/repository/
COPY crates/app/Cargo.toml       ./crates/app/
COPY crates/plugin-api/Cargo.toml ./crates/plugin-api/
COPY crates/llm/Cargo.toml        ./crates/llm/
COPY crates/host/Cargo.toml ./crates/host/
COPY crates/api/Cargo.toml ./crates/api/
COPY crates/skills/Cargo.toml ./crates/skills/
COPY crates/retrieve/Cargo.toml ./crates/retrieve/
COPY crates/scheduler/Cargo.toml ./crates/scheduler/
COPY crates/chat/Cargo.toml ./crates/chat/
COPY crates/work/Cargo.toml ./crates/work/
COPY crates/knowledge/Cargo.toml ./crates/knowledge/
COPY crates/graph/Cargo.toml ./crates/graph/
# In-tree plugin crates (workspace members plugins/*/crate).
COPY plugins/osint-data/crate/Cargo.toml ./plugins/osint-data/crate/
COPY plugins/media/crate/Cargo.toml ./plugins/media/crate/
COPY plugins/alias-judge/crate/Cargo.toml ./plugins/alias-judge/crate/
COPY plugins/reference/crate/Cargo.toml ./plugins/reference/crate/
COPY plugins/developer/crate/Cargo.toml ./plugins/developer/crate/
COPY plugins/epistemics/crate/Cargo.toml ./plugins/epistemics/crate/
COPY plugins/sandbox/crate/Cargo.toml ./plugins/sandbox/crate/
COPY plugins/research/crate/Cargo.toml ./plugins/research/crate/

# Create dummy source files matching the workspace structure so that
# cargo can resolve and compile all dependencies without the real source.
RUN mkdir -p crates/protocol/src \
             crates/models/src \
             crates/repository/src \
             crates/app/src \
             crates/plugin-api/src \
             crates/llm/src \
             crates/host/src \
             crates/api/src \
             crates/skills/src \
             crates/retrieve/src \
             crates/scheduler/src \
             crates/chat/src \
             crates/work/src \
             crates/knowledge/src \
             crates/graph/src \
             plugins/osint-data/crate/src \
             plugins/media/crate/src \
             plugins/alias-judge/crate/src \
             plugins/reference/crate/src \
             plugins/developer/crate/src \
             plugins/epistemics/crate/src \
             plugins/sandbox/crate/src \
             plugins/research/crate/src && \
    echo "" > crates/protocol/src/lib.rs && \
    echo "" > crates/models/src/lib.rs && \
    echo "" > crates/repository/src/lib.rs && \
    echo "" > crates/app/src/lib.rs && \
    echo "" > crates/plugin-api/src/lib.rs && \
    echo "" > crates/llm/src/lib.rs && \
    echo "" > crates/host/src/lib.rs && \
    echo "" > crates/api/src/lib.rs && \
    echo "" > crates/skills/src/lib.rs && \
    echo "" > crates/retrieve/src/lib.rs && \
    echo "" > crates/scheduler/src/lib.rs && \
    echo "" > crates/chat/src/lib.rs && \
    echo "" > crates/work/src/lib.rs && \
    echo "" > crates/knowledge/src/lib.rs && \
    echo "" > crates/graph/src/lib.rs && \
    echo "" > plugins/osint-data/crate/src/lib.rs && \
    echo "" > plugins/media/crate/src/lib.rs && \
    echo "" > plugins/alias-judge/crate/src/lib.rs && \
    echo "" > plugins/reference/crate/src/lib.rs && \
    echo "" > plugins/developer/crate/src/lib.rs && \
    echo "" > plugins/epistemics/crate/src/lib.rs && \
    echo "" > plugins/sandbox/crate/src/lib.rs && \
    echo "" > plugins/research/crate/src/lib.rs && \
    echo "fn main() {}" > crates/app/src/main.rs && \
    cargo build --release --locked 2>/dev/null || true && \
    rm -rf crates/*/src plugins/*/crate/src \
           target/release/.fingerprint/agent* \
           target/release/deps/agent* \
           target/release/deps/libagent* \
           target/release/agent*

# Copy actual source code
COPY crates ./crates
COPY plugins ./plugins

# Build the application
RUN cargo build --release --locked

# =============================================================================
# Stage 2: Runtime
# =============================================================================
FROM debian:trixie-slim AS runtime

# Install runtime dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    git \
    openssh-client \
    ffmpeg \
    && rm -rf /var/lib/apt/lists/*

# yt-dlp for the Media Learning skill (video metadata + caption extraction).
# Standalone Linux build — self-contained, no python runtime needed. Pinned by
# rebuild: `docker compose build` fetches the current release.
#
# ffmpeg is REQUIRED, not optional: the Whisper fallback has shipped
# (WHISPER_PROVIDER=http + the whisper sidecar), so a caption-less video now
# reaches yt-dlp's audio download, and YouTube serves that as m3u8 — which
# yt-dlp can only mux with ffmpeg. Without it the job fails "m3u8 download
# detected but ffmpeg could not be found", retries 3×, and dead-letters: 3 such
# jobs on 2026-09-11 alone. It was previously omitted on the grounds that the
# Whisper path had not shipped yet; that comment outlived the condition.
ADD https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux /usr/local/bin/yt-dlp
RUN chmod 0755 /usr/local/bin/yt-dlp

# deno: yt-dlp solves YouTube's JavaScript challenges with it (yt-dlp's EJS;
# only deno is enabled by default). Without it yt-dlp warns that YouTube
# extraction "has been deprecated" and its requests look less like a real
# client, one reason for the 2026-10-16 "Sign in to confirm you're not a bot"
# blocks. The official binary-only image, pinned.
COPY --from=denoland/deno:bin-2.9.7 /deno /usr/local/bin/deno

# Create non-root user
RUN useradd -m -u 1000 agent

# Copy binary from builder
COPY --from=builder /app/target/release/agent-brain /usr/local/bin/

# Seed bundles (services/plugins.rs). Compose points PLUGINS_DIR at the
# bind-mounted repo instead; this copy is the default (./plugins from WORKDIR).
COPY --chown=agent:agent plugins /home/agent/plugins/

# Copy entrypoint script
COPY --chown=agent:agent entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# Pre-create runtime directories with correct ownership so named volumes inherit permissions
RUN mkdir -p /home/agent/snapshots /home/agent/telemetry /home/agent/.ssh \
    && chown -R agent:agent /home/agent/snapshots /home/agent/telemetry /home/agent/.ssh \
    && chmod 700 /home/agent/.ssh

# Switch to non-root user
USER agent
WORKDIR /home/agent

# Default environment variables
ENV NEO4J_URI=bolt://neo4j:7687 \
    NEO4J_USER=neo4j \
    OLLAMA_URL=http://ollama:11434 \
    OLLAMA_MODEL=qwen3.5:4b \
    LOG_LEVEL=info \
    LOG_FORMAT=json \
    RUST_BACKTRACE=1 \
    MCP_TRANSPORT=http \
    MCP_HTTP_BIND=0.0.0.0:3000

# Expose HTTP port for MCP server
EXPOSE 3002

ENTRYPOINT ["entrypoint.sh"]
CMD ["serve"]

# Labels
LABEL org.opencontainers.image.title="agent-brain" \
      org.opencontainers.image.description="General Intelligence Agent Core with Graph RAG and MCP"
