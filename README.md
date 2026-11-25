# RISC-V Assembly Language Server

riscv-asm-lsp is a language server for RISC-V assembly in the GNU assembler
syntax, with an extension for the Zed editor.

Prebuilt binaries for Linux (`x86_64` and `aarch64` musl), macOS (`aarch64`
and `x86_64`), and Windows (`x86_64`) are published on
[GitHub Releases](https://github.com/mihirokawai9472642/riscv-asm-lsp/releases),
or you can build from source.

Like `clangd`, the server's reference is its bundled data generated from one
pinned LLVM release (currently `llvmorg-23.1.6`, updated with each server
release). External `clang` and `llvm-mc` tools are optional supplements and are
not required for editing, hover documentation, completion, navigation, or
symbols.

## Quick Start

1. Install `zed/` as a dev extension (`zed: install dev extension`; registry publication is pending — see [Developing the Extension](docs/zed.md#developing-the-extension)).
2. Optionally install LLVM (`clang` and `llvm-mc`) and enable `diagnostics.external` (`"auto"` or `"on"`) if you want compiler-backed diagnostics and external `llvm-mc` expansions alongside the built-in engine (see [Optional: LLVM for clang-based diagnostics and expansions](#optional-llvm-for-clang-based-diagnostics-and-expansions)).
3. Open a `.S` or `.s` file (see [Recommended Settings](docs/zed.md#recommended-settings) to enable `document_symbols` and `semantic_tokens`, which Zed extensions cannot enable by default in `config.toml`).

## Features

- **Hover** for instructions, aliases and pseudo-instructions (`mv`, `li`,
  `la`, `csrw`, ...), CSRs with their fields, registers by number and ABI
  name, directives, preprocessor directives (`#define`, `#include`, ...), and
  project symbols (`_start`, `.equ`/`.set`/`=`, macros, `#define`, `1f`/`1b`,
  `\@`). Instruction hovers show decoded operand constraints and the current
  line's expansion and encoding (built in by default, with optional `llvm-mc`
  when `diagnostics.external` is enabled).
- **Completion** filtered by `-march` and `.option arch`: instructions with
  variant-specific operand placeholders (Tab moves between operands), directive
  snippets (`.macro` ... `.endm`, `.if`, `.irp`, ...), registers, CSRs,
  symbols, `%` relocation functions and macro parameters.
- **Navigation and symbols**: go to definition, references, highlight and
  rename for labels, numeric labels (`1f`, `1b`), `.equ`/`.set`/`=`, macros
  and `#define`s across files and included headers; document outline and
  workspace symbols.
- **Diagnostics and code actions** via the built-in Rust validator on unsaved
  text (plus optional `clang` when `diagnostics.external` is `"auto"` or
  `"on"`), with suppressible diagnostic codes and `#` preprocessor-directive
  comment quick fixes.
- **Signature help, folding ranges, and inlay hints** for instruction operands,
  comment/directive blocks, CSR addresses, and resolved constants.
- **Semantic tokens** classifying registers, CSRs, labels, constants, macros,
  parameters, preprocessor lines and `#` comments.
- **Zed extension** (`zed/`): the `RISC-V Assembly` language with a
  tree-sitter grammar, highlighting, outline, brackets, indentation and
  block/line comment support.

`clang` and `llvm-mc` are optional and disabled by default (`diagnostics.external`
is `"off"`). Everything — built-in C preprocessing, built-in diagnostics, hover
expansions and encodings, completion, navigation, and symbols — works out of
the box without external tools.

See [docs/features.md](docs/features.md) for detailed feature behaviour.

## Optional: LLVM for clang-based diagnostics and expansions

External `clang` and `llvm-mc` tools are completely optional, and any recent
LLVM version with RISC-V (`riscv32`/`riscv64`) target support works. No
specific LLVM version is required.

When `clang` or `llvmMc` are not set explicitly, `riscv-asm-lsp` searches in
order:

1. `clang` / `llvm-mc` on `PATH`
2. Highest versioned `clang-<N>` / `llvm-mc-<N>` found on `PATH`
3. Standard install directories (newest version first):
   - Linux: `/usr/lib/llvm-*/bin`
   - macOS (Homebrew): `/opt/homebrew/opt/llvm/bin`, `/usr/local/opt/llvm/bin`,
     and versioned `llvm@*` formulas
   - Windows: `C:\Program Files\LLVM\bin`

Candidates that lack RISC-V targets (such as Apple's system `clang`) are
skipped automatically. When automatic discovery finds no usable binary, the
dependent feature is disabled quietly in the UI (logged once to stderr); warning
popups are shown only when an explicitly configured path fails or lacks RISC-V
targets.

- **Debian / Ubuntu**:
  ```bash
  sudo apt install clang llvm
  ```
- **macOS (Homebrew)**:
  ```bash
  brew install llvm
  ```
  Apple's system `clang` does not include the RISC-V target and is not enough
  for RISC-V diagnostics or `llvm-mc` expansion.
- **Windows**: install LLVM using the official LLVM installer (which installs
  tools under `C:\Program Files\LLVM\bin` by default).

## Configuration

`initializationOptions` (in Zed: `lsp.riscv-asm-lsp.initialization_options`)
are read once at startup. Settings under `lsp.riscv-asm-lsp.settings` (or sent
via `workspace/didChangeConfiguration`) apply live without restarting the
language server.

Example `.zed/settings.json`:

```json
{
  "lsp": {
    "riscv-asm-lsp": {
      "settings": {
        "defaultTarget": "riscv64-unknown-elf",
        "defaultMarch": "rv64gc",
        "diagnostics": {
          "enable": true,
          "delayMs": 300
        }
      }
    }
  }
}
```

| Option | Default | Meaning |
| --- | --- | --- |
| `clang` | Auto-discover (`PATH` -> versioned -> standard dirs) | Path to `clang` (overrides auto-discovery) |
| `llvmMc` | Auto-discover (`PATH` -> versioned -> standard dirs) | Path to `llvm-mc` (overrides auto-discovery) |
| `compileCommandsDirs` | `["", "build"]` | Directories, relative to the project root, searched for `compile_commands.json` |
| `defaultTarget` | `"riscv64-unknown-elf"` | Target for files without a compilation database entry |
| `defaultMarch` | `"rv64gc"` | `-march` for files without an entry |
| `diagnostics.enable` | `true` | Enable diagnostics |
| `diagnostics.delayMs` | `300` | Pause after typing before diagnostics run |
| `diagnostics.external` | `"off"` | External `clang` / `llvm-mc` mode (`"off"`, `"auto"`, or `"on"`; opt-in) |
| `diagnostics.suppress` | `[]` | Built-in diagnostic codes to suppress (e.g. `["gnu-only-directive"]`) |

## Installing a release

When using Zed, the extension downloads the release archive automatically. For
standalone use (or other editors), download the matching archive and `.sha256`
checksum from
[GitHub Releases](https://github.com/mihirokawai9472642/riscv-asm-lsp/releases):

| Target | Archive |
| --- | --- |
| Linux `x86_64` (static musl) | `riscv-asm-lsp-x86_64-unknown-linux-musl.tar.gz` |
| Linux `aarch64` (static musl) | `riscv-asm-lsp-aarch64-unknown-linux-musl.tar.gz` |
| macOS Apple Silicon (`aarch64`) | `riscv-asm-lsp-aarch64-apple-darwin.tar.gz` |
| macOS Intel (`x86_64`) | `riscv-asm-lsp-x86_64-apple-darwin.tar.gz` |
| Windows `x86_64` | `riscv-asm-lsp-x86_64-pc-windows-msvc.zip` |

For example, on Linux or macOS (`v0.2.0`):

```bash
ARCHIVE=riscv-asm-lsp-x86_64-unknown-linux-musl.tar.gz
curl -fsSLO "https://github.com/mihirokawai9472642/riscv-asm-lsp/releases/download/v0.2.0/${ARCHIVE}"
curl -fsSLO "https://github.com/mihirokawai9472642/riscv-asm-lsp/releases/download/v0.2.0/${ARCHIVE}.sha256"
sha256sum -c "${ARCHIVE}.sha256"   # or: shasum -a 256 -c "${ARCHIVE}.sha256" on macOS
tar -xzf "${ARCHIVE}" riscv-asm-lsp
install -m 0755 riscv-asm-lsp ~/.local/bin/riscv-asm-lsp
```

On Windows PowerShell, download the `.zip` and `.zip.sha256`, compare
`Get-FileHash riscv-asm-lsp-x86_64-pc-windows-msvc.zip -Algorithm SHA256`
against the `.sha256` file, extract `riscv-asm-lsp.exe`, and place it on
`PATH`.

Every release archive includes `LICENSE.md`, `THIRD_PARTY_NOTICES.md`,
`THIRD_PARTY_LICENSES.md`, and `COPYRIGHT-library.html`.

## Building

Building natively requires Rust 1.90 or newer (`edition = "2024"`):

```bash
cargo build --release
```

With Docker (using the repository's Rust + LLVM 23 development container):

```bash
docker compose up -d --build
docker compose exec dev cargo build --release
```

The server binary is `target/release/riscv-asm-lsp` (`riscv-asm-lsp.exe` on
Windows). Place it on `PATH` or point `lsp.riscv-asm-lsp.binary.path` to it.
See [docs/zed.md](docs/zed.md) for Zed settings, extension development,
container execution, Windows with WSL, and theme overrides.

## Documentation

| Document | Contents |
| --- | --- |
| [docs/features.md](docs/features.md) | Detailed behaviour of hover, completion, navigation, diagnostics, semantic tokens and preprocessing |
| [docs/design.md](docs/design.md) | Architecture, server internals, robustness rules, testing and roadmap |
| [docs/data.md](docs/data.md) | Instruction/CSR/directive data sources, pinned versions, generation and licenses |
| [docs/zed.md](docs/zed.md) | Installing and configuring the Zed extension |
| [AGENTS.md](AGENTS.md) | Development commands, code conventions, module layout and test workflow |

## License

The code is licensed under the [MIT License](LICENSE.md); the Zed extension
carries its own copy in [zed/LICENSE](zed/LICENSE). The instruction data in
`data/riscv.json` is derived from LLVM (`Apache-2.0 WITH LLVM-exception`),
riscv-unified-db (`BSD-3-Clause-Clear`) and riscv-asm-manual (`CC-BY-4.0`) and
stays under their licenses; hand-written files in `data/` (`directives_gnu.json`,
`supplements.json`, `texts.json`) are MIT. See
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md); binary release archives
also include `THIRD_PARTY_LICENSES.md` and `COPYRIGHT-library.html` for
third-party crates, musl/CRT, and the Rust standard library.
