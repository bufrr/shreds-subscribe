# Build stage
FROM rust:1.80 AS builder

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

# Copy Cargo files
COPY Cargo.toml Cargo.lock ./

# Copy source code
COPY src ./src

# Build the application
RUN cargo build --release

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
COPY --from=builder /usr/src/app/target/release/shreds-subcribe /usr/local/bin/shreds-subscribe

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
