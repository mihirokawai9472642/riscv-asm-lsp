# Development container for riscv-asm-lsp (Rust edition 2024 + LLVM/Clang 19).
# Pin the Rust minor version on Debian Trixie so builds stay reproducible.
FROM rust:1.100-trixie

# clang-19 provides clang; llvm-19 provides llvm-mc and llvm-tblgen.
# musl-tools and file support static musl release builds and verification.
RUN apt-get update && apt-get install -y --no-install-recommends \
    clang-19 \
    llvm-19 \
    musl-tools \
    file \
    git \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Install formatting/linting tools, targets for the Zed extension crate and
# static Linux release builds, and license auditing/bundling tools.
RUN rustup component add rustfmt clippy \
    && rustup target add wasm32-wasip2 x86_64-unknown-linux-musl \
    && cargo install --locked --version 0.22.0 cargo-deny \
    && cargo install --locked --version 0.9.3 --features cli cargo-about

# Match host WSL UID/GID 1000 so files written to the bind-mounted repo are
# owned by the host user, and prepare user-writable Cargo cache directories.
RUN groupadd -g 1000 dev \
    && useradd -m -u 1000 -g 1000 -s /bin/bash dev \
    && mkdir -p /home/dev/.cargo/registry /home/dev/.cargo/git \
    && chown -R dev:dev /home/dev/.cargo

# Put LLVM 19 unversioned binaries first on PATH and keep /usr/local/cargo/bin
# on PATH so preinstalled tools (cargo-deny) remain accessible under CARGO_HOME.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/home/dev/.cargo \
    PATH="/usr/lib/llvm-19/bin:/home/dev/.cargo/bin:/usr/local/cargo/bin:${PATH}"

USER dev
