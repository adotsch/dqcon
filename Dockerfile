FROM rust:latest

# Install zig and other dependencies
RUN apt-get update && apt-get install -y \
    wget \
    xz-utils \
    && rm -rf /var/lib/apt/lists/*

# Install Zig (using a pre-built binary for simplicity)
RUN wget https://ziglang.org/download/0.13.0/zig-linux-x86_64-0.13.0.tar.xz \
    && tar -xvf zig-linux-x86_64-0.13.0.tar.xz \
    && mv zig-linux-x86_64-0.13.0 /usr/local/zig \
    && ln -s /usr/local/zig/zig /usr/local/bin/zig \
    && rm zig-linux-x86_64-0.13.0.tar.xz

# Install cargo-zigbuild
RUN cargo install cargo-zigbuild

# Install formatting tools used by local development checks
RUN rustup component add rustfmt

# Add Rust targets for all platforms
RUN rustup target add \
    aarch64-unknown-linux-gnu \
    x86_64-unknown-linux-gnu \
    aarch64-unknown-linux-musl \
    x86_64-unknown-linux-musl \
    aarch64-pc-windows-gnullvm \
    x86_64-pc-windows-gnullvm \
    aarch64-apple-darwin \
    x86_64-apple-darwin

WORKDIR /src
