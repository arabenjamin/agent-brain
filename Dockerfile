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

# Copy manifests first for dependency caching
COPY Cargo.toml Cargo.lock ./

# Create dummy src to build dependencies only
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    echo "" > src/lib.rs && \
    cargo build --release --lib 2>/dev/null || true && \
    rm -rf src && \
    rm -rf target/release/.fingerprint/agent* \
           target/release/deps/agent* \
           target/release/deps/libagent* \
           target/release/agent*

# Copy actual source code
COPY src ./src

# Build the application
RUN cargo build --release

# =============================================================================
# Stage 2: Runtime
# =============================================================================
FROM debian:bookworm-slim AS runtime

# Install runtime dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -m -u 1000 agent

# Copy binary from builder
COPY --from=builder /app/target/release/agent-api /usr/local/bin/

# Switch to non-root user
USER agent
WORKDIR /home/agent

# Default environment variables
ENV NEO4J_URI=bolt://neo4j:7687 \
    NEO4J_USER=neo4j \
    OLLAMA_URL=http://ollama:11434 \
    OLLAMA_MODEL=llama3 \
    LOG_LEVEL=info \
    LOG_FORMAT=json \
    RUST_BACKTRACE=1

ENTRYPOINT ["agent-api"]
CMD ["serve"]

# Labels
LABEL org.opencontainers.image.title="agent-api" \
      org.opencontainers.image.description="Autonomous API Knowledge Graph MCP Server"
