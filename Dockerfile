# =============================================================================
# Stage 1: Build
# =============================================================================
FROM rust:latest AS builder

WORKDIR /app

# Install build dependencies (OpenSSL for neo4rs)
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy workspace manifests first for dependency caching
COPY Cargo.toml Cargo.lock ./
COPY crates/protocol/Cargo.toml ./crates/protocol/
COPY crates/models/Cargo.toml    ./crates/models/
COPY crates/repository/Cargo.toml ./crates/repository/
COPY crates/app/Cargo.toml       ./crates/app/

# Create dummy source files matching the workspace structure so that
# cargo can resolve and compile all dependencies without the real source.
RUN mkdir -p crates/protocol/src \
             crates/models/src \
             crates/repository/src \
             crates/app/src && \
    echo "" > crates/protocol/src/lib.rs && \
    echo "" > crates/models/src/lib.rs && \
    echo "" > crates/repository/src/lib.rs && \
    echo "" > crates/app/src/lib.rs && \
    echo "fn main() {}" > crates/app/src/main.rs && \
    cargo build --release 2>/dev/null || true && \
    rm -rf crates/*/src \
           target/release/.fingerprint/agent* \
           target/release/deps/agent* \
           target/release/deps/libagent* \
           target/release/agent*

# Copy actual source code
COPY crates ./crates

# Build the application
RUN cargo build --release

# =============================================================================
# Stage 2: Runtime
# =============================================================================
FROM debian:trixie-slim AS runtime

# Install runtime dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    git \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -m -u 1000 agent

# Copy binary from builder
COPY --from=builder /app/target/release/agent-brain /usr/local/bin/

# Copy context profiles (YAML files for ContextBuilderService)
COPY --chown=agent:agent contexts /home/agent/contexts/

# Pre-create snapshots directory with correct ownership so named volume inherits permissions
RUN mkdir -p /home/agent/snapshots && chown agent:agent /home/agent/snapshots

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

ENTRYPOINT ["agent-brain"]
CMD ["serve"]

# Labels
LABEL org.opencontainers.image.title="agent-brain" \
      org.opencontainers.image.description="General Intelligence Agent Core with Graph RAG and MCP"
