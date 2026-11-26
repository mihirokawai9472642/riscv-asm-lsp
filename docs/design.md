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
- Matching each workspace's exact GNU `as` or local LLVM version dynamically.
  Following `clangd`, behaviour is defined by the server's own bundled reference
  generated from one pinned LLVM release, updated with each server release;
  multiple per-version datasets or binaries (`zls`/`HLS` style) are not
  maintained.
- Executing compiler drivers discovered inside `compile_commands.json` (never run
  by default, matching `clangd`'s security model).
- Directives that only GNU `as` accepts and LLVM rejects (for example `.eqv`,
  `.func`, `.float16`, listing directives). They are tolerated in parsing and
  treated as non-LLVM advice/warnings rather than supported assembler directives.

### 1.4 Priorities

1. Environment independence: installing the extension works out of the box with
   bundled reference data and zero required external tools, while keeping
   explicit `binary` overrides (`docker compose exec ...`) and `PATH` servers
   working seamlessly.
2. Clean, portable `clangd`-style architecture: bundled single-version reference
   engine supplemented optionally by explicit or `PATH`-discovered LLVM tools,
   with safe compile-database handling and zero panics.

## 2. Key decisions

`clangd` is the main architecture and distribution reference (compile-database
handling, configuration layering and safety rules, workspace indexing,
robustness, and single pinned upstream reference updated once per LLVM major
release).

| Decision | Choice | Reason |
| --- | --- | --- |
| Scope | RISC-V only (RV32 and RV64) | Architecture-specific data, CSRs and grammar stay coherent and maintainable |
| Syntax | GNU assembler syntax (`#` / `//` / `/* */` comments, `;` statements, `.` directives) | Standard syntax for RISC-V assembly; accepted by LLVM |
| Reference implementation | Bundled data generated from one pinned LLVM release (`23` / `llvmorg-23.1.6`), supplemented optionally by external `clang`/`llvm-mc` | Matches `clangd`'s single-reference model without per-version data maintenance; reference LLVM acts as CI oracle |
| External tools | Optional & opt-in (`diagnostics.external: "off"` by default), any recent LLVM version with `riscv32`/`riscv64` targets; never run compilers from `compile_commands.json` | Avoids brittle host-version coupling and security risks; built-in engine handles core workflows offline |
| Built-in engine | Pure-Rust preprocessor (`preprocessor/`) + built-in validator (`diagnostics/builtin.rs`) + `RISCVMatInt` / tblgen expansion & encoding (`hover/matint.rs`, `hover/builtin_expand.rs`) | Avoids C++ LLVM MC bindings and 5-target LLVM C++ builds; keeps binary small and static |
| Built-in diagnostics rule | Report only when certain (skip unexpanded macros, `.macro`/`.irp`/`.rept` bodies, unresolved expressions); GNU-only constructs warn with stable codes | Avoids false positives that erode trust while providing stable suppressible diagnostic codes |
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
                     ├──> built-in engine (C preprocessor, validator, RISCVMatInt + Inst encoding)
                     ├──> clang     (optional unsaved diagnostics via stdin when opted in)
                     └──> llvm-mc   (optional hover expansion & encoding when opted in)
```

Instruction and reference text data is generated offline and embedded into the
binary via `include_str!`, so runtime execution needs no network access,
external data directory, or external tools. `clang` and `llvm-mc` are opt-in
supplements (`diagnostics.external: "off"` by default); when disabled, missing,
or failing, the built-in engine provides preprocessing, diagnostics, and hover
expansions/encodings.

### Repository layout

| Path | Contents |
| --- | --- |
| `server/` | Language server crate (`riscv-asm-lsp`) |
| `xtask/` | Offline data generator (`cargo xtask gen-data`) |
| `data/` | Generated `riscv.json` and `report.md`; hand-written `directives_gnu.json`, `supplements.json`, `texts.json` |
| `zed/` | Zed extension (`wasm32-wasip2`, queries, `config.toml`) |
| `tests/` | Integration tests, JSON-RPC client harness, fixtures, and `tests/snapshots/` |
| `docs/` | Contributor and user documentation (`design.md`, `features.md`, `data.md`, `zed.md`) |
| `Dockerfile`, `compose.yaml` | Optional development container with Rust and LLVM 23 |
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
| `preprocessor/` | Built-in pure-Rust C preprocessor (`mod.rs`), RISC-V predefined macros (`predefined.rs`), `#if` expression evaluator (`expr.rs`), and macro expander with UTF-16 source mapping (`expand.rs`), plus per-line directive classification |
| `ast.rs` | Shared `tree-sitter-riscv` AST visitor and classification helpers |
| `document.rs` | Open document text store and incremental `tree-sitter` reparsing |
| `context/` | `march.rs` (`MarchSpec`, `.option` tracking), `compile_db.rs` (`compile_commands.json` / `compile_flags.txt`, multi-root, linker-script filter), `uri.rs` (`url`-based symlink-preserving URIs) |
| `index/` | Background workspace scanner (`mod.rs`), AST symbol extractor (`parse.rs`), lexical/macro/include scope resolver (`scope.rs`) |
| `hover/` | Hover orchestration (`mod.rs`), built-in `RISCVMatInt` (`matint.rs`), offline expansion & tblgen encoding (`builtin_expand.rs`), `llvm-mc` expansion (`expand.rs`), operand-class decoder (`operands.rs`), shared Markdown renderer (`format.rs`) |
| `completion/` | Position classification & text edits (`mod.rs`), candidate collection & lazy resolve (`candidates.rs`), snippet builder (`snippets.rs`) |
| `signature_help.rs` | Active operand signature help with decoded LLVM operand documentation |
| `folding.rs` | Block comments, `.macro`/`.if`/`.rept`/`#if` blocks, and label scope folds |
| `inlay_hint.rs` | CSR number/name and `.equ`/`#define` value inlay hints |
| `navigation.rs` | Go to definition, find references, document highlight, prepare/execute rename |
| `symbols.rs` | Document symbols (`DocumentSymbol`) and workspace symbols (`SymbolInformation`) |
| `semantic_tokens.rs` | Full and range semantic token encoding (including `#` comment/preproc override and inactive preprocessor branch greying) |
| `diagnostics/` | Async debounced `DiagnosticsWorker` (`worker.rs`), pure-Rust built-in validator (`builtin.rs`), `clang` runner (`clang.rs`), stderr/note/include parser (`parse.rs`), `#define` hints (`hints.rs`) |
| `code_action.rs` | `did you mean` and `Use // comment` quick fixes and rewrite actions |
| `tools.rs` | Version-independent external process discovery, RISC-V target verification, configured-tool warnings, timeouts, and one-shot permanent-failure disable |

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
with fallback to nearest-sibling interpolation (without inheriting sibling `-x`
language overrides) or `compile_flags.txt`, and extracts an allow-list of
assembler/preprocessor flags (`--target`, `-march`, `-mabi`, `-mcmodel`, `-I`, `-iquote`,
`-isystem`, `-D`, `-U`, `-include`, `-imacros`, `--sysroot`, `-isysroot`,
`-nostdinc`, `-Wa,-I/-march/-mabi`, `-x`, `-menable-experimental-extensions`,
PIC flags) plus the working directory. When no entry matches, configured
defaults (`defaultTarget`, `defaultMarch`) and workspace root apply. Per-file
project configuration files (`.riscv-asm-lsp.yaml` / `.riscv-asm-lsp.yml`,
located nearest up to workspace root) can layer `CompileFlags.Remove`,
`CompileFlags.Add` (through the same flag allow-list), and
`Diagnostics.Suppress` — never unsafe tool paths (`clang`, `llvmMc`, driver
paths). Per-file preprocessing defaults by suffix (`.S`, `.sx`, headers = on;
`.s`, `.inc` = off) and is overridden by `-x assembler-with-cpp` or
`-x assembler`. `.option push`/`pop`/`rvc`/`norvc`/`arch` and `.attribute arch` directives refine the
effective `MarchSpec` per line.

### 4.4 Configuration

Options are read from `initializationOptions` as the startup base layer and
updated live via `workspace/didChangeConfiguration` (deep-merging nested objects
such as `diagnostics` key-by-key on top of the base layer, ignoring `null` or
empty `{}` payloads) and `workspace/didChangeWorkspaceFolders`:
`clang`, `llvmMc`, `compileCommandsDirs`, `defaultTarget`, `defaultMarch`,
`diagnostics.enable`, `diagnostics.delayMs`, `diagnostics.external` (default `"off"`), and `diagnostics.suppress`. External tool paths (`clang`,
`llvmMc`) come strictly from user/client configuration, `PATH`, or standard
system install directories — never from compilers named inside
`compile_commands.json` (which is inspected solely for allow-listed assembler and
preprocessor flags).

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

- **Parsing and `tree-sitter-riscv` resilience**: the upstream
  `tree-sitter-riscv` grammar produces `ERROR` nodes on five common RISC-V /
  Linux kernel assembly constructs:
  1. C function-like macro invocations with parentheses (e.g. `SET_FAULT_HANDLER(x0, a1)`);
  2. multi-line `\` line-continuation macro calls;
  3. multi-argument `.option arch, +a, +c`;
  4. `.dc.a sym` data-expression directives;
  5. column-0 `sym = expr` symbol assignments.
  The server keeps full IDE behaviour on all five by combining C-macro-call
  skips in the built-in validator (`head.contains('(')` / leading `(` arg) with
  line/token fallback extraction on `ERROR` spans in `index/parse.rs`. In
  addition, inline labels defined inside C macro argument strings or
  `__stringify(...)` (e.g. Linux `ALTERNATIVE(..., __stringify(1: ...), ...)`)
  are extracted with exact source spans for outline and navigation.

## 6. Licensing

The project's code is MIT ([LICENSE.md](../LICENSE.md), [zed/LICENSE](../zed/LICENSE)).
Embedded data stays under its upstream licenses ([THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md)):

| Material | License | Notes |
| --- | --- | --- |
| Generated LLVM instruction/CSR/register/extension data & `RISCVMatInt` port | `Apache-2.0 WITH LLVM-exception` | Generated from pinned `.td` sources; `server/src/hover/matint.rs` ported from `RISCVMatInt.cpp` |
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

Tool-dependent tests are marked `#[ignore]` and run in CI/container with LLVM 23
via `-- --include-ignored`. The non-ignored suite passes with `clang` and
`llvm-mc` absent from `PATH`.

## 8. Roadmap

Only unfinished work is listed here:

- **Distribution verification & publishing**:
  - Test clean Zed GUI installation/settings/features, tag `v0.2.0` once ready, and publish to the Zed extension registry (`zed-industries/extensions`).
  - Additional optional install channels (`crates.io` after moving embedded `data/` into `server/`, `cargo-binstall`, Neovim/Helix configs).
- **clangd-style project ergonomics**:
  - Compile-database command interpolation from nearest known sibling file; `.clangd`-style project configuration file (excluding unsafe tool-path settings).
- **Additional LSP features & corpus items**: none currently open beyond distribution verification and Zed polish.
- **Zed extension polish**:
  - Separate `.s` vs. `.S` language grammars or upstream `tree-sitter-riscv`
    preprocessor improvements if real-world editing warrants it.

## 9. Open questions

| Topic | Notes |
| --- | --- |
| `.asm` suffix | Shared across many architectures; currently left unclaimed |

## Appendix A. Directive support in LLVM 23

Checked against `llvm-mc` 23.1.6:

| Status | Directives |
| --- | --- |
| Not supported | `.eqv`, `.func`/`.endfunc`, `.float16`, `.bfloat16`, `.dtprelword`, `.linkonce`, `.stabs`, listing directives (`.list`, `.nolist`, `.title`, `.sbttl`, `.psize`, `.mri`) |
| Supported (representative) | `.macro` (including `\@`), `.altmacro`, `.irp`, `.rept`, `.equ`, `.set`, `=`, `.equiv`, `.option` (`arch`, `push`, `pop`, `exact`), `.insn`, `.attribute`, `.variant_cc`, `.cfi_*`, `.pushsection`, `.incbin`, `.ifc`, `.ifb`, `.reloc`, `.print`, `.warning`, `.int`, `.weakref`, `.line`, `.version`, `.dc`, `.dcb`, `.ds`, `.ifnotdef`, `.bundle_*`, `.base64` |

For example, `riscv-asm-manual` lists `.dtprelword` while LLVM 23 rejects it,
so LLVM remains authoritative for directive acceptance.

## Appendix B. References

- LLVM RISC-V target: https://github.com/llvm/llvm-project/tree/llvmorg-23.1.6/llvm/lib/Target/RISCV
- riscv-unified-db: https://github.com/riscv/riscv-unified-db
- riscv-asm-manual: https://github.com/riscv-non-isa/riscv-asm-manual
- tree-sitter-riscv: https://github.com/omeyenburg/tree-sitter-riscv
- lsp-server: https://crates.io/crates/lsp-server
- yaml_serde: https://crates.io/crates/yaml_serde
- Zed extension publishing: https://zed.dev/docs/extensions/publishing/prerequisites
