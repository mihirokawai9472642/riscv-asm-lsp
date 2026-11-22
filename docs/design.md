# Design

This document describes what riscv-asm-lsp is, how it is built, and the
decisions behind it. It is the reference for implementation; when the code and
this document disagree, fix one of them.

## 1. Purpose

riscv-asm-lsp (RISC-V Assembly Language Server) is a language server for
RISC-V assembly written in the GNU assembler syntax, together with a Zed
extension that uses it.

### 1.1 Background

It replaces asm-lsp and its Zed extension (zed-asm) for RISC-V work. Testing
them on a RISC-V kernel project showed problems that configuration cannot fix:

- Hover and completion know only base instructions and numbered registers:
  nothing for ABI register names (`sp`, `a0`), pseudo-instructions (`la`,
  `li`, `call`, `ret`), CSR instructions (`csrw`) or CSR names (`stvec`).
- Go to definition finds only named labels in the current file: not numeric
  labels (`1f`, `1b`), `.equ` symbols, macros, `#define`s or labels in other
  files.
- The server panics on positions inside multibyte characters (for example a
  Japanese comment) or past the end of a line, because it treats UTF-16
  columns as byte offsets.
- Diagnostics run only on save, are one column off, and pull requests for
  diagnostics are never answered.
- The Zed extension has only a highlight query: no outline, bracket matching,
  indentation rules or block comments.

Both projects have had little maintenance activity recently, so fixes are not
expected to arrive upstream soon.

### 1.2 Goals

- Accurate hover and completion for everything an assembler accepts: base and
  extension instructions, aliases, pseudo-instructions, CSRs, registers by
  number and ABI name, and directives.
- Navigation for every kind of symbol an assembly program defines, across
  files.
- Diagnostics on unsaved text, at the right position.
- Never crash, whatever the input or request.
- A Zed extension with a RISC-V grammar and the usual editor features.

### 1.3 Non-goals

- Architectures other than RISC-V.
- Directives that only the GNU assembler supports (`.eqv`, `.func`,
  `.float16`, listing directives). They are tolerated, not supported.
- Full macro expansion or full C preprocessing (for example evaluating
  `#ifdef`). Where real expansion matters, the server asks LLVM tools.
- Protection against untrusted projects. The server runs commands built from
  the project's compilation database, so it is meant for projects the user
  trusts.

### 1.4 Priorities

1. Solve the problems above in the author's environment (Section 10).
2. Later: run anywhere without that environment (Section 13.2), and possibly
   publish the Zed extension to the Zed extension registry.

Decisions that would make (2) hard are avoided from the start: external tool
paths and the compilation database location are configurable, and neither the
server nor the extension assumes Docker.

## 2. Key decisions

| Decision | Choice | Reason |
| --- | --- | --- |
| Scope | RISC-V only (RV32 and RV64) | A general assembly server needs data and grammars per architecture and is hard to maintain |
| Syntax | GNU assembler syntax (`#` comments, `;` separators, `.` directives) | The syntax RISC-V code is written in; LLVM accepts the same syntax |
| Reference implementation | The LLVM integrated assembler (clang, llvm-mc) | Results match the tools that build the code; diagnostics have columns; expansions and encodings are available; permissive license |
| LLVM version | 19 (`llvmorg-19.1.7`) | Matches the author's toolchain (Debian trixie). Supporting several versions is left for later |
| Language | Rust | Same language as the Zed extension; tree-sitter and LSP crates exist; ships as one binary |
| Data generation | Rust (`xtask`) | One language in the repository |
| Repository | Server, data and Zed extension in one repository | Versions stay in step; the Zed registry accepts extensions in a subdirectory |
| License | MIT for the project's code; imported data keeps its own license | All dependencies and data are compatible with MIT distribution (Section 11) |

Names:

| Use | Name |
| --- | --- |
| Repository, Cargo package, binary, server name in Zed settings | `riscv-asm-lsp` |
| Zed language name | `RISC-V Assembly` |
| Zed extension ID | `riscv-assembly` |

## 3. Architecture

```text
Zed ──LSP──> riscv-asm-lsp ──> embedded instruction data (data/riscv.json)
                 │
                 ├──> tree-sitter-riscv (parsing, symbol index)
                 ├──> clang     (diagnostics, unsaved text via stdin)
                 └──> llvm-mc   (expansion and encoding for hover)
```

The instruction data is generated ahead of time and embedded in the binary,
so the server needs no network access and no data files at run time. clang
and llvm-mc are optional: without them, diagnostics and expansions are off
and everything else works.

Repository layout:

| Path | Contents |
| --- | --- |
| `server/` | The language server crate (binary `riscv-asm-lsp`) |
| `xtask/` | Data generation (`cargo xtask gen-data`) |
| `data/` | Generated data, the difference report, hand-written directive descriptions |
| `zed/` | The Zed extension |
| `tests/` | Integration tests and assembly fixtures |
| `docs/` | This document |
| `Dockerfile`, `compose.yaml` | The development container |
| `THIRD_PARTY_NOTICES.md` | Attribution for imported data |

## 4. Instruction data

### 4.1 Sources

The rule: **LLVM decides what exists; documentation supplies descriptions.**

| Information | Source | License |
| --- | --- | --- |
| Instructions, aliases and pseudo-instructions the assembler accepts; their syntax, operand classes and required extensions | LLVM TableGen (`llvm-tblgen --dump-json`) | Apache-2.0 WITH LLVM-exception |
| CSR names, numbers, required extensions, deprecated names | LLVM TableGen (`SysReg`) | same |
| Registers and ABI names | LLVM TableGen (`Register`, `AltNames`) | same |
| Extensions and their names in `-march` | LLVM TableGen (`SubtargetFeature`), checked against `clang --print-supported-extensions` | same |
| Instruction names, descriptions and assembly formats | riscv-unified-db (`spec/std/isa/inst/`) | BSD-3-Clause-Clear |
| Single-instruction aliases (`mv`, `csrw`, `ret`, ...) | riscv-unified-db (`pseudoinstructions`) | same |
| CSR descriptions and fields | riscv-unified-db (`spec/std/isa/csr/`) | same |
| Register descriptions and calling convention | riscv-unified-db (`register_file/`) | same |
| Expanding pseudo-instructions (`la`, `li`, `call`, `tail`, ...) | riscv-asm-manual (pseudoinstruction table) | CC-BY-4.0 |
| RISC-V directives (about 45) | riscv-asm-manual (directive table) | CC-BY-4.0 |
| Common GNU assembler directives (about 40: `.rept`, `.if`, `.set`, `.include`, `.cfi_*`, ...) | Written for this project; the GNU as manual (GFDL) is not used | MIT |

### 4.2 Pinned inputs

| Input | Version |
| --- | --- |
| LLVM | Tag `llvmorg-19.1.7`; only the `.td` files under `llvm/include/llvm` and `llvm/lib/Target/RISCV` (about 4 MB, sparse checkout) |
| riscv-unified-db | Commit `5d1ded1f93ab` (the project is under active development and its data format may change) |
| riscv-asm-manual | Commit `27d6e9e67e6d` |

Moving a pin means regenerating the data and reviewing the difference report.

### 4.3 Generation

`cargo xtask gen-data` runs in the development container (it needs
`llvm-tblgen`) and writes `data/riscv.json` and `data/report.md`. The output
is committed, so building the server needs neither LLVM sources nor network
access.

1. Dump LLVM records to JSON (about 180 MB) and keep only what is needed:
   - assembler-visible instructions: records with an `AsmString` that are not
     `isCodeGenOnly` (pseudo records such as `la`, `li` and `call` count);
   - `InstAlias`, `SysReg`, `Register` and `SubtargetFeature` records;
   - drop records whose predicates mention a vendor extension
     (`HasVendor...`).
2. Read the riscv-unified-db YAML with `yaml_serde`. The files use no anchors,
   merge keys or tags.
   - Strip template markup (`<%- ... -%>`, in 34 instruction and 125 CSR
     files) and AsciiDoc markup from descriptions.
   - Decode HTML entities (`&ne;`, `&#8800;`, `&infin;`, `&pm;`, `&gt;`; 13
     files).
   - Ignore `$ref` (12 files, part of Zimop) until that extension matters.
3. Parse the riscv-asm-manual tables (pseudo-instructions, directives).
4. Read the hand-written directive descriptions from `data/`.
5. Join by name, apply the rules in 4.5, and write the outputs.

### 4.4 Data format

`data/riscv.json` (about 1.8 MB compact JSON) holds:

| Key | Entries |
| --- | --- |
| `meta` | Versions of the pinned inputs |
| `extensions` | Name as used in `-march`, LLVM feature name, description |
| `instructions` | Mnemonic; kind (`instruction`, `alias`, `pseudo`); syntax with operand names and classes (for example `simm12`, `GPR`, `csr_sysreg`); required extensions; XLEN; long name; description; source of the description; for aliases, the instruction they stand for |
| `csrs` | Name, number, privilege level, required extensions, fields, description, deprecated names |
| `registers` | Name, ABI names, class (integer, floating point, vector), description, who saves it in the calling convention |
| `directives` | Name, arguments, description, source |

The exact schema is defined by the server's data types and may change freely
while the project is young.

### 4.5 Differences between LLVM and the documentation

Measured on 2026-11-22 (LLVM 19.1.7 against riscv-unified-db `5d1ded1f93ab`,
vendor extensions excluded):

| Comparison | Count | Main contents |
| --- | --- | --- |
| Mnemonics in both (including aliases and pseudo-instructions) | 1381 | |
| Only in LLVM | 69 | `.insn` formats, hints such as `c.slli64`, aliases such as `fge`, `fgt`, `fabs.h`, and `call`, `frsr`, `fsflagsi` |
| Only in riscv-unified-db | 99 | The Q extension (34; LLVM 19 does not support it), newer extensions (Zilx, Zvabd, ...), older names (`fmv.s.x`) |
| rv64gc instructions in riscv-unified-db missing from LLVM | 0 | |
| CSRs in both | 401 | |
| CSRs only in LLVM | 26 | AIA (`stopi`, `stopei`, `mvien`, ...) |
| CSRs only in riscv-unified-db | 16 | Smctr, Sdtrig (`tcontrol`), Smcntrpmf (`mcyclecfg`), ... |

In everyday code (rv64gc, privileged instructions, common CSRs) the two
agree. Mismatches are handled as follows:

| Case | Behavior |
| --- | --- |
| In LLVM, with a description | Shown normally |
| In LLVM, no description | Show syntax and required extensions, and say there is no description |
| Only in the documentation | Not offered in completion; hover says this LLVM version does not accept it |
| Different names (deprecated, renamed) | Joined through LLVM's deprecated names; old names are marked |

`data/report.md` lists every entry in each case, so that a pin update shows
what changed.

## 5. Server internals

| Component | Responsibility |
| --- | --- |
| LSP layer | `lsp-server` (synchronous, from rust-analyzer) and `lsp-types`. Only implemented features are advertised |
| Documents | Text and version of open documents (incremental sync), with a per-line map between UTF-16 and byte offsets |
| Parser | tree-sitter-riscv; incremental reparse on edits |
| Index | Project-wide symbol table (Section 5.2), built at startup and updated on change and save |
| Context | Per-file flags from the compilation database (`--target`, `-march`, `-mabi`, `-I`, `-D`) and the `.option` state |
| External tools | clang and llvm-mc processes, with delays, cancellation and time limits |

### 5.1 Positions

Zed accepts only UTF-16 positions (checked in Zed 1.29.3), and clang reports
columns in bytes, counting from 1. The server converts every position at the
boundary: UTF-16 from the client and bytes from tools are turned into byte
offsets inside the server, and back when replying. Positions outside the text
are clamped, never trusted.

### 5.2 Symbols and scope

| Kind | Scope |
| --- | --- |
| Labels and `.globl` symbols | The whole project; several definitions with one name are all returned |
| Local symbols (`.L...`) | The file |
| Numeric labels (`1:`) | `1b` is the nearest preceding `1:`, `1f` the nearest following one |
| `.equ`, `.set`, `=` | The file and the files it includes with `.include` |
| `.macro` | The file and the files it includes |
| Macro parameters (`\reg`) | The macro body |
| `#define` in headers | Files that `#include` the header; headers are scanned for `#define` lines only |
| C functions (`call kernel_main`) | Out of scope (clangd's job); may be revisited |

The range of a label runs from the label to just before the next ordinary
label. Numeric labels are children of the enclosing label.

### 5.3 Compilation context

For each file the server looks up its entry in `compile_commands.json`
(default locations: the project root and `build/`) and keeps the flags that
matter. Without an entry, it uses the configured defaults. `-march` controls
which instructions and CSRs are offered; `.option arch` and `.option rvc` may
refine this later (open question).

## 6. Features

### 6.1 Hover

| Target | Content |
| --- | --- |
| Instruction | LLVM syntax in a `riscv` fence, then compact metadata (long name, extensions, XLEN), first-paragraph description (~300 chars max), and `**This line:**` live expansion/encoding |
| Alias or pseudo-instruction (`mv`, `csrw`, `li`, `la`, `call`, ...) | Same compact layout with `alias of ...` / `pseudo-instruction` (excluding self-aliases such as `ld` -> `ld`), plus the current line's expansion |
| CSR | Header line (`stvec · 0x105 · S-mode`), one-sentence description, and a bullet list of fields (`- NAME [bits] — summary`, no wide table) |
| Register (`x2` or `sp`) | Single line with ABI name, architectural name, role, and saver convention |
| Directive | Syntax in a `riscv` fence and description |
| Project symbol (label, `.equ`, `.macro`, `#define`) | Kind, `file:line`, value when known, and the defining line in a `riscv` fence |
| Numeric label reference (`1f`, `1b`) | Defining location and line |
| Include directive (`#include`, `.include`) | Resolved filesystem path |

Hover omits attribution trailers (`Source: ...`) and duplicate operand-naming
sections so the popover stays compact and fits without horizontal scrolling.

The expansion comes from `llvm-mc`, given the current line (and any `.option`
lines that affect it, plus `.equ` / `#define` constant substitution and `clang
-E -P -x assembler-with-cpp` preprocessing when needed), cached off the main loop:

```text
llvm-mc -triple=riscv64 -mattr=<from -march> -show-encoding -M no-aliases
```

Directives, generated local labels (`.Lpcrel_hi0:`), and `# fixup` comment lines
are stripped from the output. Instruction encodings are rendered as little-endian
hex instruction words (`0x1141`, `0x10529073`) or `(relocated)` when relocations
are present:

```text
li a0, 0x12345678   ->  lui a0, 74565  # 0x12345537 / addiw a0, a0, 1656  # 0x6785051b
csrw stvec, t0      ->  `csrrw zero, stvec, t0` · `0x10529073`
mv a0, a1           ->  `c.mv a0, a1` · `0x852e`   (when C is enabled)
la t0, sym          ->  auipc t0, %pcrel_hi(sym)  # (relocated) / addi t0, t0, %pcrel_lo(.Lpcrel_hi0)  # (relocated)
```

### 6.2 Completion

Trigger characters include `.`, `%`, `\`, `,`, and ` `.

| Position | Candidates |
| --- | --- |
| Start of a statement | Instructions, aliases, pseudo-instructions, directives and macros, filtered by `-march` |
| Operand | Depending on the operand class: integer or floating-point registers, CSRs, symbols, relocation functions (`%hi(`, `%lo(`, `%pcrel_hi(`, ...) |
| After `.option` | `arch`, `rvc`, `norvc`, `push`, `pop`, `relax`, `norelax`, `pic`, `nopic`, `exact` |
| Inside a macro body | The macro's parameters |

When the client supports snippets (`snippetSupport`) and `labelDetails`, each
variant of an instruction, alias, or pseudo-instruction (register forms sorted
first, memory forms as `ld ${1:rd}, ${2:0}(${3:rs1})`) inserts operand tab stops,
and structured directives (`.macro`, `.rept`, `.irp`, `.if`, `.equ`, `.section`,
`.option`) provide block snippets; zero-operand mnemonics insert plain text.
When snippet support is absent, plain completions append the required separator
(` ` after mnemonics/directives taking arguments, `, ` after non-last operands
and `.option arch`, and nothing after zero-operand mnemonics, final operands,
or other `.option` subcommands).

### 6.3 Navigation

Go to definition, find references, document highlight and rename, following
the scope rules in 5.2. Rename refuses numeric labels and names defined
outside the project.

### 6.4 Symbols

Document symbols (outline and breadcrumbs, using the label ranges from 5.2;
macros are listed too) and workspace symbols.

### 6.5 Diagnostics

1. Build the flags from the compilation context (5.3).
2. Pass the unsaved text to clang on standard input:

   ```text
   clang <flags> -iquote <directory of the file> -x assembler-with-cpp -c -o /dev/null -
   ```

   (`-x assembler` for `.s` files). `-iquote` makes `#include "..."` resolve
   as it would for the file on disk.
3. Parse `<stdin>:LINE:COLUMN: error|warning: MESSAGE` lines. Convert the
   byte column to UTF-16 and extend the range to the word at that position.
   clang's SARIF output does not include assembler errors, so text parsing is
   required.
4. Run after typing pauses (about 300 ms); drop results that an edit has made
   stale. One run takes 0.1 to 0.3 seconds, measured through
   `docker compose exec`.
5. Publish with `textDocument/publishDiagnostics`. Do not advertise pull
   diagnostics unless they are answered.

### 6.6 Code actions

Quick fixes from clang's suggestions, for example `did you mean: mv?`.

### 6.7 Semantic tokens

`textDocument/semanticTokens/full` and `textDocument/semanticTokens/range` emit
delta-encoded UTF-16 tokens classifying architectural names vs. project symbols:

| Token type | Modifier | Examples |
| --- | --- | --- |
| `register` | — | `t0`, `x5`, `fa0`, `sp` |
| `csr` | — | `stvec`, `mstatus`, `scause` |
| `label` | `declaration` (at def) | `_start:`, `1:`, `1f`, `1b`, `.Lloop` |
| `constant` | `declaration` (at def) | `.equ`, `.set`, `=`, `#define` |
| `macro` | `declaration` (at def) | `.macro SAVE`, `SAVE` invocations |
| `parameter` | `declaration` (at def) | macro parameter names and `\reg` references |

### 6.8 Later

Inlay hints (`.equ` values, CSR numbers, expansions), folding ranges (label
ranges, macros, `.if` blocks), signature help (from the instruction syntax) and
call hierarchy (`call`, `jal`, `tail`). Zed supports all of them; LSP folding
needs a Zed setting.

## 7. Robustness

Rules that come from the bugs found in asm-lsp:

- No request or input makes the server panic. Out-of-range positions are
  clamped.
- Positions are converted at the boundary (5.1); nothing inside the server
  indexes text with a client column.
- When an external tool is missing or fails, only the feature that needs it
  stops, and the problem is logged once.
- Every advertised capability is answered.

## 8. Configuration

Options are read from `initializationOptions` (in Zed:
`lsp.riscv-asm-lsp.initialization_options`). All are optional.

| Option | Default | Meaning |
| --- | --- | --- |
| `clang` | `"clang"` | Path to clang |
| `llvmMc` | `"llvm-mc"` | Path to llvm-mc |
| `compileCommandsDirs` | `["", "build"]` | Directories, relative to the project root, searched for `compile_commands.json` |
| `defaultTarget` | `"riscv64-unknown-elf"` | Target when a file has no compilation database entry |
| `defaultMarch` | `"rv64gc"` | `-march` when a file has no entry |
| `diagnostics.enable` | `true` | Run clang for diagnostics |
| `diagnostics.delayMs` | `300` | Pause after typing before diagnostics run |

Logs go to standard error.

## 9. Zed extension

| Item | Choice |
| --- | --- |
| Language | `RISC-V Assembly`, separate from zed-asm's `Assembly`. Both claim `.S` and `.s`: remove zed-asm, or map the files with `"file_types": { "RISC-V Assembly": ["S", "s"] }` in the project settings |
| File suffixes | `S`, `s`, and `riscv` (so ```` ```riscv ```` hover code fences highlight without colliding with zed-asm's `S`/`s`; `asm` is shared with other architectures) |
| Grammar | tree-sitter-riscv (MIT), pinned at `c89b7c525d90`. It distinguishes numeric labels, macro parameters, registers and expressions |
| Queries | `highlights.scm` (with `@preproc` rule for `#include`/`#define`/`#if...` comments), `injections.scm` (injecting `c` on preprocessor lines), `brackets.scm`, `indents.scm`, `outline.scm`, `semantic_token_rules.json` |
| `config.toml` | `line_comments = ["# ", "// "]`, table-form `block_comment` and `documentation_comment`, `brackets`, `word_characters`/`completion_query_characters`, indentation rules for labels and `.macro` |
| Starting the server | If `lsp.riscv-asm-lsp.binary` is set, Zed itself starts that command without calling `language_server_command` (checked in Zed 1.29.3). Otherwise `zed/src/lib.rs` looks up `riscv-asm-lsp` on `PATH` and returns an error if missing; downloading a release comes later |
| License | `zed/LICENSE` (MIT): the registry checks the extension directory, not the repository root |
| Recommended settings | `document_symbols` (`"on"`) and `semantic_tokens` (`"combined"`) documented in the README; `document_folding_ranges` comes later |

Building a Zed development extension that contains Rust code requires Rust on
the machine running Zed (Zed adds `wasm32-wasip2` automatically). Although local
projects with `lsp.riscv-asm-lsp.binary` do not invoke
`language_server_command`, remote projects (WSL, SSH) require `extension.wasm`
on the remote host whenever `extension.toml` declares `[language_servers.*]`, so
`zed/Cargo.toml` and `zed/src/lib.rs` are included.

## 10. The author's environment

Zed runs on Windows, projects live in WSL (Debian), and each project's tools
run in its own container with clang and llvm 19. The server runs in the
project's container, like clangd:

```json
"lsp": {
  "riscv-asm-lsp": {
    "binary": {
      "path": "/usr/bin/docker",
      "arguments": ["compose", "exec", "-T", "dev", "riscv-asm-lsp"]
    }
  }
}
```

The container mounts the project at the same path as in WSL, so paths match
on both sides, and clang and llvm-mc are called directly. Project Dockerfiles
download a pinned release binary into `/usr/local/bin`.

Before the Zed extension exists, the server can be tried in Zed through
zed-asm: setting `lsp.asm-lsp.binary` to the command above makes Zed start
riscv-asm-lsp in place of asm-lsp.

## 11. Licensing

The project's code is MIT. Checked on 2026-11-22, nothing below prevents
distribution under MIT.

| Material | License | Obligations |
| --- | --- | --- |
| Data generated from LLVM TableGen | Apache-2.0 WITH LLVM-exception | Include the license; state that the material was changed |
| riscv-unified-db data (all files used carry this license in their headers) | BSD-3-Clause-Clear | Keep the copyright notice and license in source and binary distributions; no endorsement; no patent grant |
| riscv-asm-manual tables | CC-BY-4.0 | Credit the source, authors and license; state changes |
| lsp-server, serde, serde_json, yaml_serde | MIT OR Apache-2.0 | Include the license |
| lsp-types, tree-sitter, tree-sitter-riscv | MIT | Include the license |
| zed_extension_api | Apache-2.0 | Include the license |
| clang, llvm-mc | Only run as separate programs | None |

- Imported data stays under its own license; `THIRD_PARTY_NOTICES.md` and the
  files in `data/` say where it comes from.
- The data is embedded in the binary, so release archives include the license
  texts and notices.
- The README states: code is MIT; `data/` is under the licenses of its
  sources.
- `cargo-deny` checks the licenses of all dependencies, including transitive
  ones.
- Code copied from asm-lsp (BSD-2-Clause) or zed-asm (MIT) keeps its
  copyright notice; using them only as a reference needs nothing.

## 12. Testing

| Kind | What it covers |
| --- | --- |
| Unit | Position conversion, symbol scope, `-march` parsing, data joining |
| Integration | A test client starts the binary and talks JSON-RPC: initialization, every feature, shutdown |
| Regression | Inputs that crashed asm-lsp: multibyte comments, positions past the end of a line, empty lines |
| Data | A sample line built from each instruction's syntax assembles with llvm-mc |
| Editor | A checklist of hover, completion, navigation, outline and diagnostics cases, tried in Zed |

Fixtures include a small RISC-V kernel entry and trap vector (labels, numeric
labels, `.equ`, `.macro`, `#include`, CSR instructions, pseudo-instructions),
a file with deliberate errors, and a file with Japanese comments.

## 13. Roadmap

### 13.1 Milestones

| Milestone | Contents | Done when | Status |
| --- | --- | --- | --- |
| M0 Foundation | Development container, data generation, server skeleton (document sync, positions, configuration, logging), integration test harness | Data counts match 4.5; initialization, shutdown and out-of-range positions pass the tests | Done |
| M1 Hover and completion | 6.1 without expansions, 6.2 | Hover works for ABI register names, pseudo-instructions, CSR instructions and CSR names; completion follows `-march` | Done |
| M2 Navigation and symbols | 5.2, 6.3, 6.4 | Definitions resolve for `1f`/`1b`, `.equ`, macros, labels in other files and `#define`; outline and breadcrumbs follow label ranges | Done |
| M3 Diagnostics and expansions | 6.5, 6.6, expansions in 6.1 | Errors in unsaved text appear at the right column; files with Japanese comments work; stopping clang disables only diagnostics | Done |
| M4 Zed extension | Section 9 | With zed-asm removed, highlighting, outline, brackets, indentation, comment toggling and the server all work | Done; final checks in Zed pending |
| M5 Adoption | Release binary, project templates use it (Section 10) | A new project from the template needs no extra setup | Not started |

### 13.2 Later

- The features in 6.8.
- Running anywhere: release binaries for Linux, macOS and Windows; the
  extension downloads them; CI on all three; tests pass without clang.
- Several LLVM versions, for example by recording the first version that
  accepts each entry and filtering by the user's `clang --version`.
- Talking to the asm-lsp and zed-asm maintainers, then deciding whether to
  publish to the Zed registry. The registry asks extension authors to try
  improving an existing extension first, and accepts a replacement only after
  documented contact attempts.

## 14. Open questions

| Question | Notes |
| --- | --- |
| GNU toolchain | Whether to run gcc for diagnostics when the compilation database names gcc (not planned) |
| `.asm` suffix | Whether to claim it despite other architectures using it |
| Release notices | Release archives must carry the license texts of the bundled data and of the Rust dependencies compiled into the binary; decide how to collect them (for example `cargo about`) before the first release |

## Appendix A. Directive support in LLVM 19

Checked with llvm-mc 19.1.7.

| Status | Directives |
| --- | --- |
| Not supported | `.eqv`, `.func`/`.endfunc`, `.float16`, `.bfloat16`, `.base64`, `.dtprelword`, `.linkonce`, `.stabs`, listing directives (`.list`, `.nolist`, `.title`, `.sbttl`, `.psize`, `.mri`) |
| Supported (main ones) | `.macro` (including `\@`), `.altmacro`, `.irp`, `.rept`, `.equ`, `.set`, `=`, `.equiv`, `.option` (`arch`, `push`, `pop`, `exact`), `.insn`, `.attribute`, `.variant_cc`, `.cfi_*`, `.pushsection`, `.incbin`, `.ifc`, `.ifb`, `.reloc`, `.print`, `.warning` |

The riscv-asm-manual table lists `.dtprelword`, which LLVM 19 rejects: an
example of why the documentation is checked against LLVM.

## Appendix B. References

- LLVM RISC-V target: https://github.com/llvm/llvm-project/tree/llvmorg-19.1.7/llvm/lib/Target/RISCV
- riscv-unified-db: https://github.com/riscv/riscv-unified-db
- riscv-asm-manual: https://github.com/riscv-non-isa/riscv-asm-manual
- tree-sitter-riscv: https://github.com/omeyenburg/tree-sitter-riscv
- lsp-server: https://crates.io/crates/lsp-server
- yaml_serde: https://crates.io/crates/yaml_serde
- asm-lsp: https://github.com/bergercookie/asm-lsp
- zed-asm: https://github.com/DevBlocky/zed-asm
- Zed extension publishing: https://zed.dev/docs/extensions/publishing/prerequisites
