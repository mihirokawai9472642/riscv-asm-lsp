# RISC-V Assembly Language Server

riscv-asm-lsp is a language server for RISC-V assembly in the GNU assembler
syntax, with an extension for the Zed editor. It works with any editor that
supports the Language Server Protocol.

It provides hover documentation for instructions, CSRs, registers and
directives, completion filtered by `-march`, go to definition, references and
rename across files and headers, diagnostics on unsaved text, and semantic
highlighting. It reads `compile_commands.json` and runs a built-in C
preprocessor for `.S` files.

Like clangd, the server's reference is data generated from one pinned LLVM
release (currently `llvmorg-23.1.6`): what that LLVM accepts is what the server
offers. Everything works without external tools; `clang` and `llvm-mc` are
optional.

## Quick Start

- **Zed**: install `zed/` as a dev extension (registry publication is
  pending) and open a `.S` or `.s` file. The extension downloads the server.
  See [docs/editors.md](docs/editors.md#zed).
- **Other editors**: install the server from a release (below) and see
  [docs/editors.md](docs/editors.md) for Neovim and Helix.

## Installing a release

Prebuilt binaries are published on
[GitHub Releases](https://github.com/mihirokawai9472642/riscv-asm-lsp/releases).
Download the archive for your platform and its `.sha256` checksum:

| Target | Archive |
| --- | --- |
| Linux `x86_64` (static musl) | `riscv-asm-lsp-x86_64-unknown-linux-musl.tar.gz` |
| Linux `aarch64` (static musl) | `riscv-asm-lsp-aarch64-unknown-linux-musl.tar.gz` |
| macOS Apple Silicon | `riscv-asm-lsp-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `riscv-asm-lsp-x86_64-apple-darwin.tar.gz` |
| Windows `x86_64` | `riscv-asm-lsp-x86_64-pc-windows-msvc.zip` |

For example, on Linux:

```bash
ARCHIVE=riscv-asm-lsp-x86_64-unknown-linux-musl.tar.gz
curl -fsSLO "https://github.com/mihirokawai9472642/riscv-asm-lsp/releases/download/v0.3.0/${ARCHIVE}"
curl -fsSLO "https://github.com/mihirokawai9472642/riscv-asm-lsp/releases/download/v0.3.0/${ARCHIVE}.sha256"
sha256sum -c "${ARCHIVE}.sha256"
tar -xzf "${ARCHIVE}" riscv-asm-lsp
install -m 0755 riscv-asm-lsp ~/.local/bin/riscv-asm-lsp
```

On macOS, check the archive with `shasum -a 256 -c`; on Windows, compare
`Get-FileHash` with the `.sha256` file and put `riscv-asm-lsp.exe` on `PATH`.
To build from source instead, run `cargo build --release` (Rust 1.90 or newer).

## Optional: LLVM for clang-based diagnostics and expansions

With `diagnostics.external` set to `"auto"` or `"on"`, the server uses `clang`
for diagnostics and `llvm-mc` for hover expansions instead of its built-in
engine. Any LLVM version with the RISC-V target works:

- Debian / Ubuntu: `sudo apt install clang llvm`
- macOS: `brew install llvm` (Apple's `clang` has no RISC-V target)
- Windows: the official LLVM installer

How the server finds the tools is described in
[docs/configuration.md](docs/configuration.md#external-tools).

## Documentation

For **using** riscv-asm-lsp:

- [docs/editors.md](docs/editors.md): setting up Zed, Neovim and Helix, and
  troubleshooting.
- [docs/configuration.md](docs/configuration.md): settings,
  `compile_commands.json` and `.riscv-asm-lsp.yaml`.
- [docs/features.md](docs/features.md): what each feature does, and the
  diagnostic codes.

For **contributing**: [CONTRIBUTING.md](CONTRIBUTING.md),
[docs/architecture.md](docs/architecture.md) and [docs/data.md](docs/data.md).

## Security

See [SECURITY.md](SECURITY.md) for what the server runs and reads, and how to
report a vulnerability.

## License

The code is licensed under the [MIT License](LICENSE.md). The instruction data
derived from LLVM, riscv-unified-db and riscv-asm-manual keeps its own licenses;
see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md). Release archives also
include the licenses of third-party crates and the Rust standard library.
