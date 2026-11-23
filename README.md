# RISC-V Assembly Language Server

riscv-asm-lsp is a language server for RISC-V assembly in the GNU assembler
syntax, with an extension for the Zed editor.

**Status: early development.** Prebuilt static `x86_64-unknown-linux-musl` binaries
are published on [GitHub Releases](https://github.com/mihirokawai9472642/riscv-asm-lsp/releases),
or you can build from source. It targets LLVM 19 (`llvmorg-19.1.7`).

The LLVM integrated assembler is the reference: instruction data is generated
from LLVM's own definitions with descriptions from the RISC-V specifications,
and diagnostics and expansions come from running `clang` and `llvm-mc`.

## Features

- **Hover** for instructions, aliases and pseudo-instructions (`mv`, `li`,
  `la`, `csrw`, ...), CSRs with their fields, registers by number and ABI
  name, directives, preprocessor directives (`#define`, `#include`, ...), and
  project symbols (`_start`, `.equ`/`.set`/`=`, macros, `#define`, `1f`/`1b`,
  `\@`). Instruction hovers show decoded operand constraints and the current
  line's `llvm-mc` expansion and encoding.
- **Completion** filtered by `-march` and `.option arch`: instructions with
  variant-specific operand placeholders (Tab moves between operands), directive
  snippets (`.macro` ... `.endm`, `.if`, `.irp`, ...), registers, CSRs,
  symbols, `%` relocation functions and macro parameters.
- **Navigation and symbols**: go to definition, references, highlight and
  rename for labels, numeric labels (`1f`, `1b`), `.equ`/`.set`/`=`, macros
  and `#define`s across files and included headers; document outline and
  workspace symbols.
- **Diagnostics and code actions** from `clang` on unsaved text, with quick
  fixes for compiler suggestions and `#` preprocessor-directive comment hints.
- **Semantic tokens** classifying registers, CSRs, labels, constants, macros,
  parameters, preprocessor lines and `#` comments.
- **Zed extension** (`zed/`): the `RISC-V Assembly` language with a
  tree-sitter grammar, highlighting, outline, brackets, indentation and
  block/line comment support.

`clang` and `llvm-mc` are optional. Without them, diagnostics and expansions
are off and everything else works.

See [docs/features.md](docs/features.md) for detailed feature behaviour.

## Installing a release

Download the release archive and its checksum from
[GitHub Releases](https://github.com/mihirokawai9472642/riscv-asm-lsp/releases)
(for example `v0.1.0`):

```bash
curl -fsSLO https://github.com/mihirokawai9472642/riscv-asm-lsp/releases/download/v0.1.0/riscv-asm-lsp-x86_64-unknown-linux-musl.tar.gz
curl -fsSLO https://github.com/mihirokawai9472642/riscv-asm-lsp/releases/download/v0.1.0/riscv-asm-lsp-x86_64-unknown-linux-musl.tar.gz.sha256
sha256sum -c riscv-asm-lsp-x86_64-unknown-linux-musl.tar.gz.sha256
tar -xzf riscv-asm-lsp-x86_64-unknown-linux-musl.tar.gz riscv-asm-lsp
install -m 0755 riscv-asm-lsp ~/.local/bin/riscv-asm-lsp
```

The archive contains `riscv-asm-lsp` together with `LICENSE.md`,
`THIRD_PARTY_NOTICES.md`, `THIRD_PARTY_LICENSES.md`, and
`COPYRIGHT-library.html`. Make sure the destination directory is on `PATH`,
alongside `clang` and `llvm-mc` if you want diagnostics and expansions.

## Building

With Docker (recommended when using the repository's Rust + LLVM 19 container):

```bash
docker compose up -d --build
```

```bash
docker compose exec dev cargo build --release
```

The server binary is `target/release/riscv-asm-lsp`. Place it on the `PATH`
of the machine or container where the editor runs language servers, alongside
`clang` and `llvm-mc` if you want diagnostics and expansions.

Building natively requires Rust 1.90 or newer (`edition = "2024"`).

## Using it with Zed

1. Install `zed/` as a dev extension (`zed: install dev extension`). Zed
   compiles the WebAssembly extension, so the machine running Zed needs `git`
   and Rust (`rustup`) on its `PATH`; Zed adds `wasm32-wasip2` itself.
2. If another installed extension also claims `.S`, `.s`, or `.sx`, uninstall
   the conflicting extension (recommended) or map those suffixes in
   `file_types`. To keep linker script templates out of assembly highlighting,
   map `"*.lds.S"` and `"*.ld.S"` to `Plain Text` (or a linker-script language)
   via `file_types`; this only takes effect when `.S` is not also mapped in
   `file_types` (see [docs/zed.md](docs/zed.md#avoiding-file-type-conflicts)).
3. Make `riscv-asm-lsp` available on `PATH` or via `lsp.riscv-asm-lsp.binary`.

See [docs/zed.md](docs/zed.md) for settings, container execution, Windows with
WSL, and theme overrides.

## Configuration

Options go in `initializationOptions` (in Zed:
`lsp.riscv-asm-lsp.initialization_options`). All are optional and are read once
at startup, so changing them requires restarting the language server.

| Option | Default | Meaning |
| --- | --- | --- |
| `clang` | `"clang"` | Path to clang |
| `llvmMc` | `"llvm-mc"` | Path to llvm-mc |
| `compileCommandsDirs` | `["", "build"]` | Directories, relative to the project root, searched for `compile_commands.json` |
| `defaultTarget` | `"riscv64-unknown-elf"` | Target for files without a compilation database entry |
| `defaultMarch` | `"rv64gc"` | `-march` for files without an entry |
| `diagnostics.enable` | `true` | Run clang for diagnostics |
| `diagnostics.delayMs` | `300` | Pause after typing before diagnostics run |

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
third-party crates, musl, and the Rust standard library.
