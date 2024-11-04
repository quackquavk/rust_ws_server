# Build stage
FROM rust:1.75-slim as builder

# Create a new empty shell project
WORKDIR /usr/src/chess-server
COPY . .

# Build with release profile for better performance
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install OpenSSL - required for many Rust applications
RUN apt-get update && \
    apt-get install -y libssl-dev ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# Copy the binary from builder
COPY --from=builder /usr/src/chess-server/target/release/chess_server /usr/local/bin/
RUN chmod +x /usr/local/bin/chess_server

# Create a non-root user for security
RUN useradd -m -U -s /bin/bash chess_server

# Create necessary directories and set permissions
RUN mkdir -p /var/log/chess_server && \
    chown -R chess_server:chess_server /var/log/chess_server

# Switch to non-root user
USER chess_server

# Add debugging environment variables
ENV RUST_BACKTRACE=1
ENV RUST_LOG=debug

# Change the CMD to help with debugging
CMD ["sh", "-c", "echo 'Starting server...' && whoami && pwd && ls -la /usr/local/bin/chess_server && chess_server"]
