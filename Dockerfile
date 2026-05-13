FROM rust:1-bookworm

# System packages: fonts for headless rendering from phase 4+, pkg-config + openssl for any
# crate that wants them. Kept minimal on purpose.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        pkg-config \
        libssl-dev \
        fonts-dejavu-core \
        fonts-noto-core \
    && rm -rf /var/lib/apt/lists/*

# Run as a non-root user. UID 1000 lines up with Docker Desktop's host bind-mount mapping
# on macOS, so files we create in /workspace are readable from the host without chown.
RUN useradd -m -u 1000 -s /bin/bash daboss

# Pre-create the workspace and the target/ subdir as daboss-owned so the docker-compose
# named volumes (target-cache, cargo-cache) inherit the right ownership on first use.
# Without this, /workspace/target gets created as root:root when the volume initializes.
RUN mkdir -p /workspace/target && chown -R daboss:daboss /workspace

USER daboss

# Cargo + rustup go in the user's home, which we mount as a named volume so deps cache
# between runs.
ENV CARGO_HOME=/home/daboss/.cargo
ENV PATH="/home/daboss/.cargo/bin:${PATH}"

# Supply-chain tooling. Installed as the daboss user so the binaries land in
# /home/daboss/.cargo/bin, inside the cached volume.
RUN cargo install --locked cargo-deny cargo-audit

WORKDIR /workspace
CMD ["bash"]
