# Build stage
FROM rust:1.86 AS builder

# Install system dependencies
RUN apt-get update && apt-get install -y \
    build-essential \
    clang \
    llvm-dev \
    libclang-dev \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Create app directory
WORKDIR /usr/src/app

# Set build-time args to control how solana-ledger is sourced
ARG LEDGER_GIT_URL=https://github.com/jito-foundation/jito-solana.git
ARG LEDGER_GIT_REF=eric/v2.2-merkle-recovery

# Copy project files
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# For Docker builds, rewrite the local path dependency on solana-ledger to a pinned git ref
# This change is only inside the build container and does not modify your local files.
RUN sed -ri \
    's#^\s*solana-ledger\s*=.*#solana-ledger = { git = "'"${LEDGER_GIT_URL}"'", branch = "'"${LEDGER_GIT_REF}"'", package = "solana-ledger" }#' \
    Cargo.toml \
 && echo "Using solana-ledger from ${LEDGER_GIT_URL}@${LEDGER_GIT_REF}" \
 && cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -m -u 1000 appuser

# Copy the binary from builder
COPY --from=builder /usr/src/app/target/release/shreds-subscribe /usr/local/bin/shreds-subscribe

# Change ownership
RUN chown appuser:appuser /usr/local/bin/shreds-subscribe

# Switch to non-root user
USER appuser

# Expose ports
EXPOSE 18999/udp 28899/tcp

# Set default environment variables (can be overridden)
ENV UDP_PORT=18999 \
    RPC_PORT=28899 \
    TRACE_LOG_PATH=

# Run the application; flags are optional since ports can be set via env
ENTRYPOINT ["/usr/local/bin/shreds-subscribe"]
