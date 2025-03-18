# Build stage
FROM rust:1.75-slim as builder

# Create a new empty shell project
WORKDIR /usr/src/chess-server

# Set environment variables to help with memory usage
ENV CARGO_NET_GIT_FETCH_WITH_CLI=true
ENV RUST_BACKTRACE=1
ENV RUSTFLAGS="-C target-cpu=native -C opt-level=2"
# Limit parallel jobs
ENV CARGO_BUILD_JOBS=1

# Create .cargo/config.toml with codegen-units=1
RUN mkdir -p .cargo && \
    echo '[profile.release]' > .cargo/config.toml && \
    echo 'codegen-units = 1' >> .cargo/config.toml && \
    echo 'opt-level = "z"' >> .cargo/config.toml

# Copy only Cargo.toml first
COPY Cargo.toml Cargo.lock ./

# Create dummy src layout
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs

# Build dependencies only
RUN cargo build --release && rm -rf src

# Now copy the actual source code
COPY . .

# Verify the files are present and build
RUN ls -la src/ && \
    RUSTFLAGS="-C target-cpu=native" cargo build --release --verbose

# Runtime stage
FROM debian:bookworm-slim

# Install OpenSSL and DNS dependencies for MongoDB Atlas
RUN apt-get update && \
    apt-get install -y libssl-dev ca-certificates libc6 dnsutils && \
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
CMD ["chess_server"]