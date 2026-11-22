# RISC-V Assembly Language Server

riscv-asm-lsp is a language server for RISC-V assembly in the GNU assembler
syntax, with an extension for the Zed editor.

**Status: early development.** There are no releases yet; build the server
from source. It is developed and tested with LLVM 19.

The LLVM integrated assembler is the reference: instruction data is generated
from LLVM's own definitions, with descriptions from the RISC-V specifications,
and diagnostics and expansions come from running `clang` and `llvm-mc`.

## Features

- **Hover** for instructions, aliases and pseudo-instructions (`mv`, `li`,
  `la`, `csrw`, ...), CSRs with their fields, registers by number and ABI
  name, directives, and the project's own labels, `.equ` constants, macros and
  `#define`s. For instructions it also shows what the current line expands to
  and its encoding.
- **Completion** filtered by `-march` and `.option arch`: instructions with
  operand placeholders (Tab moves to the next operand), directive blocks such
  as `.macro` ... `.endm`, registers, CSRs, symbols, relocation functions and
  macro parameters.
- **Navigation**: go to definition, references, highlight and rename for
  labels, numeric labels (`1f`, `1b`), `.equ`/`.set`, macros and `#define`s,
  across files and included headers; outline and workspace symbols.
- **Diagnostics** from `clang` on unsaved text, with quick fixes for its
  suggestions.
- **Semantic tokens** that tell registers, CSRs, labels, constants, macros and
  macro parameters apart.
- **Zed extension** (`zed/`): the `RISC-V Assembly` language with a
  tree-sitter grammar, highlighting (including preprocessor lines in `.S`
  files), outline, brackets, indentation and comment continuation.

`clang` and `llvm-mc` are optional. Without them, diagnostics and expansions
are off and everything else works.

## Building

The Rust toolchain and LLVM 19 run in a development container. With Docker,
from the repository root:

```bash
docker compose up -d --build
```

```bash
docker compose exec dev cargo build --release
```

The server is `target/release/riscv-asm-lsp`. Put it on the `PATH` of the
machine or container where the editor starts language servers, next to
`clang` and `llvm-mc` if you want diagnostics and expansions.

## Using it with Zed

1. Install `zed/` as a dev extension (`zed: install dev extension`). Zed
   compiles it, so the machine running Zed needs `git` and Rust (rustup) on
   its `PATH`; Zed adds the `wasm32-wasip2` target itself.
2. Uninstall zed-asm, or map the file types in the project settings
   (`"file_types": { "RISC-V Assembly": ["S", "s", "sx"] }`): both extensions
   claim `.S` and `.s`.
3. Make the server reachable: either on `PATH`, or through
   `lsp.riscv-asm-lsp.binary`. For example, to run it in a project's container
   (the project mounted at the same path inside it):

   ```json
   {
     "lsp": {
       "riscv-asm-lsp": {
         "binary": {
           "path": "/usr/bin/docker",
           "arguments": ["compose", "exec", "-T", "dev", "riscv-asm-lsp"]
         }
       }
     },
     "languages": {
       "RISC-V Assembly": {
         "document_symbols": "on",
         "semantic_tokens": "combined"
       }
     }
   }
   ```

   `document_symbols` builds the outline and breadcrumbs from the server's
   label ranges; `semantic_tokens` adds its highlighting on top of the
   grammar's.

   `lsp.riscv-asm-lsp.binary` in a project's `.zed/settings.json` applies only
   when that directory is the opened project root. If you open a parent
   directory (for example your home directory `~`), either put `riscv-asm-lsp`
   on `PATH` or configure `lsp.riscv-asm-lsp.binary` in your user settings.

   Themes that omit `variable.special` (such as Ayu) can still give registers
   their own colour via `theme_overrides`:

   ```json
   "theme_overrides": {
     "Ayu Dark": {
       "syntax": {
         "variable.special": { "color": "#95e6cb" }
       }
     }
   }
   ```

### Zed on Windows with projects in WSL

- Zed links a dev extension with a directory junction, which cannot point
  into `\\wsl.localhost\...`. Copy the extension's sources (`extension.toml`,
  `LICENSE`, `Cargo.toml`, `Cargo.lock`, `src/`, `languages/`) to a local
  Windows directory outside OneDrive and install that copy. After changing
  them, copy again and press **Rebuild** on the extension.
- Install Git for Windows and rustup on Windows. With rustup's default MSVC
  toolchain, building needs Visual Studio; the GNU toolchain
  (`stable-x86_64-pc-windows-gnu`) brings its own linker.
- The extension must declare Rust code even when `lsp.riscv-asm-lsp.binary`
  is set: in a remote project, Zed loads the extension's WebAssembly on the
  remote side to start its language server.

## Configuration

Options go in `initializationOptions` (in Zed:
`lsp.riscv-asm-lsp.initialization_options`). All are optional. They are read
when the server starts, so changing them requires restarting the language
server (`editor: restart language server` in Zed).

| Option | Default | Meaning |
| --- | --- | --- |
| `clang` | `"clang"` | Path to clang |
| `llvmMc` | `"llvm-mc"` | Path to llvm-mc |
| `compileCommandsDirs` | `["", "build"]` | Directories, relative to the project root, searched for `compile_commands.json` |
| `defaultTarget` | `"riscv64-unknown-elf"` | Target for files without a compilation database entry |
| `defaultMarch` | `"rv64gc"` | `-march` for files without an entry |
| `diagnostics.enable` | `true` | Run clang for diagnostics |
| `diagnostics.delayMs` | `300` | Pause after typing before diagnostics run |

## Development

See [AGENTS.md](AGENTS.md) for the commands, conventions and tests, and
[docs/design.md](docs/design.md) for the design.

## License

The code is licensed under the [MIT License](LICENSE.md); the Zed extension
carries its own copy in [zed/LICENSE](zed/LICENSE). The instruction data in
`data/riscv.json` is derived from LLVM (Apache-2.0 WITH LLVM-exception),
riscv-unified-db (BSD-3-Clause-Clear) and riscv-asm-manual (CC-BY-4.0) and
stays under their licenses; see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
