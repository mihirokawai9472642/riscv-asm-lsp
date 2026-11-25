# Design

This document describes what riscv-asm-lsp is, how it is structured, and the
decisions behind it. It is the contributor reference for architecture and
invariants; when code and documentation disagree, update one of them.

See also:
- [docs/features.md](features.md) for per-feature behaviour.
- [docs/data.md](data.md) for instruction data sources and generation.
- [docs/zed.md](zed.md) for the Zed extension.

## 1. Purpose

riscv-asm-lsp (RISC-V Assembly Language Server) is a language server for
RISC-V assembly written in the GNU assembler syntax, paired with a Zed editor
extension.

### 1.1 Motivation

Generic assembly language servers lack the RISC-V-specific knowledge needed in
kernel, firmware and embedded projects:

- Instruction sets vary by `-march` and `.option arch`; ABI register names
  (`sp`, `a0`, ...), pseudo-instructions (`la`, `li`, `call`, `ret`), CSR
  instructions (`csrw`, ...) and CSR names/fields (`stvec`, ...) need accurate,
  architecture-aware lookup.
- Assembly programs define labels, numeric labels (`1f`/`1b`), `.equ`/`.set`/`=`
  constants, `.macro`/`.irp`/`.irpc` blocks and `#define`s across files and
  included headers.
- Clients send UTF-16 columns while assemblers report byte columns; handling
  both cleanly without panicking on multibyte text or out-of-range positions is
  essential.
- Diagnostics belong on unsaved editor text as you type, aligned with what the
  project's build toolchain actually accepts.

### 1.2 Goals

- Accurate hover and completion for everything the LLVM RISC-V assembler
  accepts: base and extension instructions, aliases, pseudo-instructions, CSRs,
  registers by number and ABI name, and directives.
- Cross-file navigation, rename and symbols for every kind of assembly symbol.
- Unsaved-text diagnostics at the right line and UTF-16 range.
- No panics on any input or LSP request.
- A Zed extension with a RISC-V tree-sitter grammar and full editor support.

### 1.3 Non-goals

- Architectures other than RISC-V.
- Directives that only GNU `as` accepts and LLVM rejects (for example `.eqv`,
  `.func`, `.float16`, listing directives). They are tolerated in parsing but
  are not treated as supported assembler directives.
- Re-implementing full C preprocessing or conditional `#ifdef` evaluation; when
  exact expansion or validation is needed, the server invokes LLVM tools.
- Sandboxing against untrusted projects: the server builds `clang` and
  `llvm-mc` invocations from `compile_commands.json` and is meant for trusted
  workspaces.

### 1.4 Priorities

1. Complete, reliable RISC-V assembly support matching LLVM 19.
2. Clean, portable architecture: configurable tool paths and compilation
   database lookup, without tying the server or extension to a particular container
   or editor host setup.

## 2. Key decisions

| Decision | Choice | Reason |
| --- | --- | --- |
| Scope | RISC-V only (RV32 and RV64) | Architecture-specific data, CSRs and grammar stay coherent and maintainable |
| Syntax | GNU assembler syntax (`#` / `//` / `/* */` comments, `;` statements, `.` directives) | Standard syntax for RISC-V assembly; accepted by LLVM |
| Reference implementation | LLVM integrated assembler (`clang`, `llvm-mc`) | Matches build diagnostics with column numbers, provides live expansions/encodings, permissive license |
| Target LLVM version | 19 (`llvmorg-19.1.7`) | Pinned reference version; supporting multiple versions is future work |
| Implementation language | Rust (edition 2024, MSRV 1.90) | Required by `tree-sitter` 0.27 (`msrv` CI check); shares ecosystem with `zed_extension_api` and ships one binary |
| Data generation | Rust (`xtask`) | Single language in the repository |
| Repository layout | Server, data, generator and Zed extension together | Versions and grammar queries stay in sync |
| Licensing | MIT for project code; source licenses kept for embedded data | All dependencies and data sources permit distribution under `THIRD_PARTY_NOTICES.md` |

Canonical names:

| Role | Name |
| --- | --- |
| Repository, server crate, binary, Zed `lsp` key | `riscv-asm-lsp` |
| Zed language name | `RISC-V Assembly` |
| Zed extension ID | `riscv-assembly` |

## 3. Architecture

```text
Editor ──LSP──> riscv-asm-lsp ──> embedded data (data/riscv.json, data/texts.json)
                     │
                     ├──> tree-sitter-riscv (parsing, AST traversal, symbol index)
                     ├──> clang     (unsaved diagnostics via stdin)
                     └──> llvm-mc   (hover expansion & encoding)
```

Instruction and reference text data is generated offline and embedded into the
binary via `include_str!`, so runtime execution needs no network access or
external data directory. `clang` and `llvm-mc` are optional: when either is
missing or fails, only diagnostics or expansions degrade.

### Repository layout

| Path | Contents |
| --- | --- |
| `server/` | Language server crate (`riscv-asm-lsp`) |
| `xtask/` | Offline data generator (`cargo xtask gen-data`) |
| `data/` | Generated `riscv.json` and `report.md`; hand-written `directives_gnu.json`, `supplements.json`, `texts.json` |
| `zed/` | Zed extension (`wasm32-wasip2`, queries, `config.toml`) |
| `tests/` | Integration tests, JSON-RPC client harness, fixtures, and `tests/snapshots/` |
| `docs/` | Contributor and user documentation (`design.md`, `features.md`, `data.md`, `zed.md`) |
| `Dockerfile`, `compose.yaml` | Optional development container with Rust and LLVM 19 |
| `THIRD_PARTY_NOTICES.md` | Attribution and license notices |

### Crate module map

`server/src/`:

| Module | Responsibility |
| --- | --- |
| `main.rs`, `lib.rs` | Binary entrypoint and library exports |
| `server.rs` | `ServerState`, LSP stdio lifecycle, typed request/notification dispatch and panic guard |
| `config.rs` | `initializationOptions` parsing |
| `data.rs` | Embedded `riscv.json` / `texts.json` structs and lookup indices |
| `position.rs` | Incremental `LineIndex`, UTF-16 <-> UTF-8 byte offsets, `clang` column conversion |
| `line.rs` | Shared line scanner for comments (`//`, `#`, `/* */`), strings, `;` statements and statement heads |
| `preprocessor.rs` | Per-line C preprocessor directive classification and `#define`/`#include` parsing |
| `ast.rs` | Shared `tree-sitter-riscv` AST visitor and classification helpers |
| `document.rs` | Open document text store and incremental `tree-sitter` reparsing |
| `context/` | `march.rs` (`MarchSpec`, `.option` tracking), `compile_db.rs` (`compile_commands.json` / `compile_flags.txt`, multi-root, linker-script filter), `uri.rs` (`url`-based symlink-preserving URIs) |
| `index/` | Background workspace scanner (`mod.rs`), AST symbol extractor (`parse.rs`), lexical/macro/include scope resolver (`scope.rs`) |
| `hover/` | Hover orchestration (`mod.rs`), `llvm-mc` expansion (`expand.rs`), operand-class decoder (`operands.rs`), shared Markdown renderer (`format.rs`) |
| `completion/` | Position classification & text edits (`mod.rs`), candidate collection & lazy resolve (`candidates.rs`), snippet builder (`snippets.rs`) |
| `signature_help.rs` | Active operand signature help with decoded LLVM operand documentation |
| `folding.rs` | Block comments, `.macro`/`.if`/`.rept`/`#if` blocks, and label scope folds |
| `inlay_hint.rs` | CSR number/name and `.equ`/`#define` value inlay hints |
| `navigation.rs` | Go to definition, find references, document highlight, prepare/execute rename |
| `symbols.rs` | Document symbols (`DocumentSymbol`) and workspace symbols (`SymbolInformation`) |
| `semantic_tokens.rs` | Full and range semantic token encoding (including `#` comment/preproc override) |
| `diagnostics/` | Async debounced `DiagnosticsWorker` (`worker.rs`), `clang` runner (`clang.rs`), stderr/note/include parser (`parse.rs`), `#define` hints (`hints.rs`) |
| `code_action.rs` | `did you mean` and `Use // comment` quick fixes and rewrite actions |
| `tools.rs` | External process execution with timeouts, Clang major-version warning, and one-shot permanent-failure disable |

`xtask/src/`: `main.rs`, `pins.rs`, `fetch.rs`, `llvm.rs`, `udb.rs`, `asm_manual.rs`, `join.rs`, `report.rs`.

## 4. Server internals

| Component | Key behaviour |
| --- | --- |
| LSP layer | Synchronous `lsp-server` + `lsp-types`; advertises only implemented capabilities; typed dispatch via `ServerState::dispatch_typed` |
| Documents | Incremental text sync updates text, `LineIndex`, and `tree-sitter` parse tree |
| AST & Line Scanner | `line.rs` handles comments/statements uniformly across hover, completion, `.option` and code actions; `ast.rs` drives indexing and semantic tokens from one grammar walk |
| Index | Background worker walks workspace via `ignore`, skipping hidden/ignored dirs, non-RISC-V assembly (e.g. Go Plan 9 prefix detection), and `*.lds.S`/`*.ld.S` linker script templates; prioritizes active project subtrees |
| Tools | `ToolRunner` executes `clang` and `llvm-mc` via argument vectors (never through a shell) with timeouts |

### 4.1 Positions

Clients use UTF-16 code-unit columns while `clang` reports 1-based byte columns
and internal Rust strings use UTF-8 byte offsets. `position.rs` converts at every
boundary and clamps out-of-range lines/columns to valid UTF-8 character
boundaries. `file://` URIs (including `file://localhost`, encoded characters,
spaces/Unicode and symlinked workspaces) are converted using `url::Url`; paths are
canonicalized only for internal map keys while LSP locations map back under the
client's workspace root.

### 4.2 Symbols and scope

| Kind | Scope |
| --- | --- |
| Labels and `.globl` symbols | Whole workspace; multiple definitions are all returned |
| Local labels (`.L...`) | Current file |
| Numeric labels (`1:`) | `1b` binds to nearest preceding `1:`; `1f` binds to nearest following `1:` |
| `.equ`, `.equiv`, `.set`, `=` | Current file and `.include`-linked files; `.set`/`=` resolves to the latest assignment active at the cursor line |
| `.macro` | Current file and `.include`-linked files |
| Macro parameters (`\param`) and `.irp`/`.irpc` parameters | Enclosing block |
| `#define` constants/macros | Preprocessed files (`.S`, `.sx`, headers, `-x assembler-with-cpp`) and `#include`/`.include` graphs |

The location counter `.` (`. = ...`) is excluded from symbol definitions,
references, workspace symbols and semantic tokens. Linker script templates
(`*.lds.S`, `*.ld.S`) are skipped by workspace scanning and ignored in LSP
document features. Open documents always respond directly from memory without
waiting for background workspace indexing.

### 4.3 Compilation context

`CompileContext` supports multi-root workspace folders, searches
`compileCommandsDirs` (`["", "build"]` by default) for `compile_commands.json`
with fallback to `compile_flags.txt`, and extracts an allow-list of
assembler/preprocessor flags (`--target`, `-march`, `-mabi`, `-I`, `-iquote`,
`-isystem`, `-D`, `-U`, `-include`, `-imacros`, `--sysroot`, `-isysroot`,
`-nostdinc`, `-Wa,-I/-march/-mabi`, `-x`, PIC flags) plus the working directory.
When no entry matches, configured defaults (`defaultTarget`, `defaultMarch`) and
workspace root apply. Per-file preprocessing defaults by suffix (`.S`, `.sx`,
headers = on; `.s`, `.inc` = off) and is overridden by `-x assembler-with-cpp` or
`-x assembler`. `.option push`/`pop`/`rvc`/`norvc`/`arch` directives refine the
effective `MarchSpec` per line.

### 4.4 Configuration

Options are read from `initializationOptions` at startup and updated live via
`workspace/didChangeConfiguration` and `workspace/didChangeWorkspaceFolders`:
`clang`, `llvmMc`, `compileCommandsDirs`, `defaultTarget`, `defaultMarch`,
`diagnostics.enable`, and `diagnostics.delayMs`.

## 5. Robustness rules

- **Never panic on client input or tool output**: no `.unwrap()`, `.expect()`,
  or unchecked indexing outside tests in `server/src/`. Clamp positions, offsets
  and slices; guard requests, notifications and worker threads with
  `catch_unwind` (returning `InternalError` on requests and logging once per
  method/worker).
- **Convert positions at the boundary**: never index source strings directly
  with a client UTF-16 column or a `clang` byte column.
- **Degrade gracefully**: when `clang` or `llvm-mc` is missing or fails, disable
  only the dependent feature and log once.
- **Honour capabilities and lifecycle**: advertise only implemented features;
  handle `shutdown` (draining pending client responses) and `exit` (status 0
  after `shutdown`, status 1 otherwise).
- **No `clippy::mutable_key_type` / `too_many_arguments` shortcuts**: keep keys
  non-mutable (or pass iterators directly) and bundle cohesive parameters into
  typed structs; any required `#[allow(deprecated)]` carries an explanatory
  comment.

## 6. Licensing

The project's code is MIT ([LICENSE.md](../LICENSE.md), [zed/LICENSE](../zed/LICENSE)).
Embedded data stays under its upstream licenses ([THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md)):

| Material | License | Notes |
| --- | --- | --- |
| Generated LLVM instruction/CSR/register/extension data | `Apache-2.0 WITH LLVM-exception` | Generated from pinned `.td` sources |
| riscv-unified-db descriptions & CSR metadata | `BSD-3-Clause-Clear` | Keep copyright and notice |
| riscv-asm-manual tables | `CC-BY-4.0` | Credit source, authors and license |
| `data/directives_gnu.json`, `data/supplements.json`, `data/texts.json` | `MIT` | Original project texts (no GPL/GFDL sources used) |
| Rust dependencies (`lsp-server`, `lsp-types`, `serde*`, `tree-sitter*`, `ignore`, `url`, `yaml_serde`, `zed_extension_api`) | `MIT` and `Unicode-3.0` (`riscv-asm-lsp` binary after MIT choice; full `deny.toml` allow-list verified) | Verified via `cargo deny check licenses` against `deny.toml` |

External `clang` and `llvm-mc` executables run strictly as child processes.
Prebuilt binary release archives (`x86_64`/`aarch64-unknown-linux-musl`,
`x86_64`/`aarch64-apple-darwin`, and `x86_64-pc-windows-msvc`) bundle
`THIRD_PARTY_LICENSES.md` (generated with `cargo-about` using `about.toml` and
`about.hbs`, plus `/usr/share/doc/musl/copyright` on Linux musl builds and a
static MSVC CRT notice on Windows builds) and `COPYRIGHT-library.html` (Rust
standard library notices) alongside `LICENSE.md` and `THIRD_PARTY_NOTICES.md`.

CI and release workflows (`.github/workflows/ci.yml`,
`.github/workflows/release.yml`) pin `actions/checkout`,
`dtolnay/rust-toolchain`, `Swatinem/rust-cache`, `taiki-e/install-action`,
`actions/upload-artifact`, and `actions/download-artifact` and keep them updated
via Dependabot.

## 7. Testing

| Suite | What it covers |
| --- | --- |
| Unit tests | Position/UTF-16 math, line scanner, `-march`/`.option` state, preprocessor classification, scope resolution, operand class decoding (all classes in `data/riscv.json`), xtask cleaning/joining |
| Integration tests | In-process JSON-RPC harness driving the real binary over stdio across navigation, completion, hover, symbols, semantic tokens, diagnostics, code actions and lifecycle |
| Snapshot tests | Golden files under `tests/snapshots/` (`hover/*.md`, `completion/*.json`, etc.); rewritten intentionally via `UPDATE_SNAPSHOTS=1` and reviewed in diff |
| Regression inputs | Multibyte UTF-8/UTF-16 characters, positions past line ends, empty lines, closed/open block comments |
| Data validation | Generated RV64GC instructions assemble with `llvm-mc` (`#[ignore]` tool test) |

Tool-dependent tests are marked `#[ignore]` and run in CI/container with LLVM 19
via `-- --include-ignored`. The non-ignored suite passes with `clang` and
`llvm-mc` absent from `PATH`.

## 8. Roadmap

Only unfinished work is listed here:

- **Distribution & ecosystem**:
  - Publish to the Zed extension registry (`zed-industries/extensions`).
  - Optional versioned LLVM data sets and additional install channels (`crates.io` / `cargo-binstall` / editor configurations for Neovim and Helix).
- **Additional LSP features**:
  - Call hierarchy (`textDocument/prepareCallHierarchy`, incoming/outgoing calls).
- **Corpus-driven improvements**:
  - Inline labels defined inside C macro argument strings (e.g. Linux
    `ALTERNATIVE(..., __stringify(... label:), ...)`).
  - Upstream reports / selective workarounds for `tree-sitter-riscv` grammar
    edge cases (`SET_FAULT_HANDLER(x0, a1)`, multiline `\` macro calls, multi-arg
    `.option arch`, `.dc.a`, start-of-line `sym = expr`).
- **Zed extension polish**:
  - Separate `.s` vs. `.S` language grammars or upstream `tree-sitter-riscv`
    preprocessor improvements if real-world editing warrants it.

## 9. Open questions

| Topic | Notes |
| --- | --- |
| GNU toolchain | Whether to invoke `gcc` when `compile_commands.json` specifies GCC (not currently planned) |
| `.asm` suffix | Shared across many architectures; currently left unclaimed |

## Appendix A. Directive support in LLVM 19

Checked against `llvm-mc` 19.1.7:

| Status | Directives |
| --- | --- |
| Not supported | `.eqv`, `.func`/`.endfunc`, `.float16`, `.bfloat16`, `.base64`, `.dtprelword`, `.linkonce`, `.stabs`, listing directives (`.list`, `.nolist`, `.title`, `.sbttl`, `.psize`, `.mri`) |
| Supported (representative) | `.macro` (including `\@`), `.altmacro`, `.irp`, `.rept`, `.equ`, `.set`, `=`, `.equiv`, `.option` (`arch`, `push`, `pop`, `exact`), `.insn`, `.attribute`, `.variant_cc`, `.cfi_*`, `.pushsection`, `.incbin`, `.ifc`, `.ifb`, `.reloc`, `.print`, `.warning`, `.int`, `.weakref`, `.line`, `.version`, `.dc`, `.dcb`, `.ds`, `.ifnotdef`, `.bundle_*` |

For example, `riscv-asm-manual` lists `.dtprelword` while LLVM 19 rejects it,
so LLVM remains authoritative for directive acceptance.

## Appendix B. References

- LLVM RISC-V target: https://github.com/llvm/llvm-project/tree/llvmorg-19.1.7/llvm/lib/Target/RISCV
- riscv-unified-db: https://github.com/riscv/riscv-unified-db
- riscv-asm-manual: https://github.com/riscv-non-isa/riscv-asm-manual
- tree-sitter-riscv: https://github.com/omeyenburg/tree-sitter-riscv
- lsp-server: https://crates.io/crates/lsp-server
- yaml_serde: https://crates.io/crates/yaml_serde
- Zed extension publishing: https://zed.dev/docs/extensions/publishing/prerequisites
