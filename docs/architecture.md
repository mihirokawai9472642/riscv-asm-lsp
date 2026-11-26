# Architecture

An overview for contributors: how the server is structured, where the code
is, and the rules that hold across it.

## Bird's eye view

```text
Editor ──LSP──> riscv-asm-lsp ──> embedded data (server/data/riscv.json, texts.json)
                     │
                     ├──> tree-sitter-riscv (parsing, symbol index)
                     ├──> built-in engine (C preprocessor, validator, li / alias expansion, encoding)
                     ├──> clang     (optional diagnostics)
                     └──> llvm-mc   (optional hover expansion)
```

The instruction data is generated offline from one pinned LLVM release plus
documentation (see [data.md](data.md)) and embedded in the binary, so the
server needs no network, data directory or external tools at run time.

Main decisions, following clangd where it applies:

- **One reference.** Behaviour is defined by the bundled data of one LLVM
  release, updated with each server release. The server does not try to match
  each project's GNU `as` or LLVM version.
- **A built-in engine instead of LLVM MC.** LLVM has no C API for its
  assembler parser, and embedding it would need C++ builds for every release
  target. The server has its own preprocessor, validator and a port of LLVM's
  `RISCVMatInt`.
- **Diagnostics only when certain.** The validator is an approximation, so it
  skips what it cannot resolve rather than risk false errors.
- **External tools are opt-in** (`diagnostics.external`, off by default). The
  server never runs the compiler named in `compile_commands.json`.

## Code map

`server/src/`:

| Module | Responsibility |
| --- | --- |
| `main.rs`, `server.rs` | Entry point, LSP lifecycle, request dispatch and panic guard |
| `config.rs` | Settings and their layering |
| `data.rs` | Embedded data and lookup tables |
| `position.rs` | UTF-16 / UTF-8 / byte-column conversion |
| `line.rs` | Comment and statement scanner shared by the features |
| `ast.rs`, `document.rs` | tree-sitter walk, open documents and incremental parsing |
| `preprocessor/` | C preprocessor: predefined macros, `#if` evaluation, macro expansion with source mapping |
| `context/` | Compilation database, `.riscv-asm-lsp.yaml`, `-march` and `.option` state, URIs |
| `index/` | Workspace scanner, symbol extraction, scope and include resolution |
| `hover/`, `completion/`, `signature_help.rs` | Hover (with `RISCVMatInt` and encoding), completion, signature help |
| `navigation.rs`, `symbols.rs`, `call_hierarchy.rs` | Definition, references, rename, symbols, call hierarchy |
| `diagnostics/` | Built-in validator, `clang` runner and output parser, debounced worker |
| `semantic_tokens.rs`, `folding.rs`, `inlay_hint.rs`, `code_action.rs` | The remaining features |
| `tools.rs` | Finding and running `clang` / `llvm-mc` |

Elsewhere:

- `xtask/`: data generation (`cargo xtask gen-data`), see [data.md](data.md).
- `zed/`: the Zed extension. `src/lib.rs` finds or downloads the server and
  forwards settings; `languages/riscv_assembly/` holds the tree-sitter queries
  and editor configuration.
- `tests/`: integration tests that drive the server over JSON-RPC, fixtures,
  and golden snapshots.

## Cross-cutting concerns

### Robustness

- The server never panics on client input, file contents or tool output: no
  `unwrap`, `expect` or unchecked indexing on such data. Requests and worker
  threads are wrapped in `catch_unwind`.
- When `clang` or `llvm-mc` is missing or fails, only the feature that needs it
  is disabled, and the problem is logged once.
- Only implemented capabilities are advertised, and every request for them is
  answered.
- Clippy's `too_many_arguments` and `mutable_key_type` are not suppressed; pass
  a struct or iterators instead.

### Positions

Clients send UTF-16 columns, `clang` reports byte columns, and Rust strings use
UTF-8 offsets. `position.rs` converts at every boundary and clamps positions
that are out of range; text is never indexed with an unconverted column.

### Symbols and scope

| Kind | Scope |
| --- | --- |
| Labels and `.globl` symbols | Whole workspace |
| `.L` labels | File |
| Numeric labels | `1b` / `1f`: nearest `1:` before / after |
| `.equ`, `.set`, `=` | File and `.include`d files; `.set` / `=` use the assignment active at the line |
| `.macro` | File and `.include`d files |
| Macro and `.irp` parameters | Enclosing block |
| `#define` | Preprocessed files and their `#include` / `.include` graph |

Open documents are answered from memory without waiting for the background
workspace index.

### Parser gaps

`tree-sitter-riscv` produces `ERROR` nodes for C macro calls with parentheses,
`\` continuations, multi-argument `.option arch`, `.dc.a`, and column-0
`sym = expr`. `index/parse.rs` falls back to line-based extraction for these,
and labels inside `ALTERNATIVE(...)` / `__stringify(...)` keep exact spans.

### Testing

Unit tests cover the position math, scanner, `-march` state, preprocessor,
scopes and operand decoding. Integration tests drive the real server over
JSON-RPC; golden snapshots in `tests/snapshots/` pin the output of hover,
completion and other features. Tests that need `clang` or `llvm-mc`, including
a check that the generated RV64GC instructions assemble, are `#[ignore]`d and
run in CI. See [CONTRIBUTING.md](../CONTRIBUTING.md#tests).
