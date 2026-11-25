# Features

Detailed specification of the features implemented in `riscv-asm-lsp`.

## Hover

Every hover uses a structured Markdown layout rendered through one shared
formatter (`hover/format.rs`):

````text
<kind and context header>

```riscv
<syntax / definition line(s)>
```

*(+N more)*

> <status or deprecation notice, if any>

---

<concise structured facts>

---

<concise summary prose>
````

Representative outputs live under `tests/snapshots/hover/`:

| Target | Context Header & Code Block | Notices, Facts & Prose | Example |
| --- | --- | --- | --- |
| Instruction / Alias / Pseudo | `<Instruction\|Alias\|Pseudo-instruction> · [<long name> · ]<extensions/base ISA> · <RV32\|RV64\|RV32/RV64>[ · spec only]`; up to 3 active-XLEN syntax variants in a `riscv` fence (plus `*(+N more)*` if truncated) | `>` notices if spec-only (not in the bundled LLVM reference), disabled by `-march`/`.option`, or deprecated (`prefer \`...\``); `Alias of:`, decoded `Operands:` constraints across all LLVM operand classes, `This line:` live expansion & encoding from `llvm-mc`, and concise `summary` | [instruction.md](../tests/snapshots/hover/instruction.md), [instruction_with_expansion.md](../tests/snapshots/hover/instruction_with_expansion.md), [alias.md](../tests/snapshots/hover/alias.md), [pseudo.md](../tests/snapshots/hover/pseudo.md), [macro_expansion.md](../tests/snapshots/hover/macro_expansion.md) |
| CSR (`stvec` or numeric `0x105` in CSR position) | `CSR · [<long name> · ]0x<hex> · <privilege>-mode[ · spec only]` (e.g. `CSR · Supervisor Trap Vector · 0x105 · S-mode`); CSR name in `riscv` fence | `>` notices if spec-only or disabled by active `-march` XLEN/extensions; `Fields:` bullet list (up to 8 fields using RV32 bit ranges under RV32 `-march`, as ``- `NAME` [bits] — summary``, plus `- *+N more fields*` if more) and concise summary prose | [csr.md](../tests/snapshots/hover/csr.md) |
| Operand keyword (`%hi`, `rne`, `iorw`, `v0.t`) | `Relocation modifier`, `Rounding mode`, `Fence ordering`, or `Vector mask`; token in `riscv` fence | Explanation matching active instruction operand role or `%` modifier | — |
| Register (`x10` / `a0`) | `Register · <Integer\|Floating-point\|Vector>[ · <saver>]` (e.g. `Register · Integer · caller-saved`); `<abi> (<arch>)` in `riscv` fence | Concise register summary prose | [register.md](../tests/snapshots/hover/register.md) |
| Directive (`.globl`, `.macro`, `.cfi_sections`, ...) | `Directive · <GNU / LLVM\|GNU only>`; `<name>[ <arguments>]` in `riscv` fence | `>` notice for GNU-only directives not accepted by the bundled LLVM reference; concise description | [directive.md](../tests/snapshots/hover/directive.md) |
| Preprocessor `#<directive>` (in preprocessed files) | `Preprocessor directive`; `#<name>` in `c` fence | `>` notice that `#<name>` is handled by the C preprocessor rather than treated as a comment; directive description | [preprocessor_directive.md](../tests/snapshots/hover/preprocessor_directive.md) |
| Project symbols (`label`, `.equ`, `.set`, `=`, `.macro`, `.irp`/`.irpc` param, `#define`, `1f`/`1b`) | ``<Kind> · `<file>:<line>` `` (`Label`, `Local label`, `Numeric label`, `Constant (.equ)` / `Constant (.equiv)` / `Symbol (.set)` / `Symbol (=)`, `Macro`, `Macro parameter`, `#define`); defining line in `riscv` fence | `>` notice when reassigned multiple times in scope; ``Value: `<val>` `` fact for constants/defines | [label.md](../tests/snapshots/hover/label.md), [equ.md](../tests/snapshots/hover/equ.md), [macro.md](../tests/snapshots/hover/macro.md), [macro_param.md](../tests/snapshots/hover/macro_param.md), [define.md](../tests/snapshots/hover/define.md) |
| Macro invocation counter (`\@`) | `Macro invocation counter`; `\@` in `riscv` fence | Explanation from `data/texts.json` | — |
| Include directive (`#include`, `.include`) | `Include` (or `Include · unresolved`); `#include "<target>"` in `c` fence | ``Path: `<resolved>` `` when found on disk; search-path configuration hint when unresolved | — |

### Live `llvm-mc` expansion & encoding

For valid instructions under active `-march`/`.option` state, hover invokes
`llvm-mc` asynchronously off the main loop:

```text
llvm-mc -triple=<riscv32|riscv64> -mattr=<attrs> -show-encoding -M no-aliases
```

- When `llvmMc` is not explicitly configured, the server searches `llvm-mc` on
  `PATH`, then the highest versioned `llvm-mc-<N>` on `PATH`, then standard
  LLVM install directories (`/usr/lib/llvm-*/bin` on Linux, Homebrew
  `llvm`/`llvm@*` on macOS, `C:\Program Files\LLVM\bin` on Windows), verifying
  RISC-V (`riscv32`/`riscv64`) target support via `llvm-mc --version` without
  requiring a specific LLVM version.
- Preceding `.option` directives, visible `.equ`/`#define` constants, and
  `clang -E -P -x assembler-with-cpp` preprocessing are applied before invoking
  `llvm-mc`.
- Assembler directives, compiler-generated `.Lpcrel_hi*:` labels, and `# fixup`
  lines are filtered out.
- Little-endian encoding bytes render as hex instruction words (`0x1141`,
  `0x10529073`) or `(relocated)` when relocations apply:

```text
li a0, 0x12345678   ->  lui a0, 74565 · 0x12345537 / addiw a0, a0, 1656 · 0x6785051b
csrw stvec, t0      ->  csrrw zero, stvec, t0 · 0x10529073
mv a0, a1           ->  c.mv a0, a1 · 0x852e          (when C is enabled)
```

## Completion

Trigger characters: `.`, `%`, `\`, `,`, and ` `.

- **Comment suppression**: handled through `line.rs` consistently with hover and
  `.option` tracking — silent inside `//`, `#` comments, and open `/* ...`
  block comments, while completing normally after closed `/* ... */` blocks on
  the same line.
- **Positions**:
  - Statement start: LLVM-accepted instructions, aliases, pseudo-instructions
    filtered by active `-march`/`.option`, directives, `.macro` invocations, and
    `#define` macros. Spec-only (`llvm_supported == false`) entries are excluded.
  - Operand positions: classified by expected operand role (integer/FP/vector
    registers, CSRs including `0xhex · mode` details, symbols, `%hi(`/etc.
    relocations from `data/texts.json`, and enclosing macro/`.irp`/`.irpc`
    parameters).
  - After `.option`: `arch`, `rvc`, `norvc`, `push`, `pop`, `relax`, `norelax`,
    `pic`, `nopic`, `exact`, plus extension names (`+ext`/`-ext`) after `.option arch, `.
  - After `.section` / `.pushsection`: standard ELF section names (`.text`, `.data`, `.rodata`, `.bss`, ...), section flags (`"ax"`, `"aw"`, ...), and ELF section types (`@progbits`, `@nobits`, ...).
- **Snippets and variant labels**:
  - When `snippetSupport` is enabled, overloaded instruction variants sort real
    instructions -> aliases -> special relocation/symbol forms, and register forms
    -> immediate forms; each variant includes its operand syntax in `label`,
    mnemonic in `filterText`, long name in `detail`, and tab-stop placeholders
    such as `ld ${1:rd}, ${2:0}(${3:rs1})$0`.
  - Structured directives (`.macro`, `.rept`, `.irp`, `.if`, `.equ`, `.section`,
    `.option`) emit block/argument snippets; zero-operand instructions insert
    plain text.
  - Without snippet support, plain completions insert required trailing
    separators (` ` or `, `) when more arguments follow.
- **Lazy documentation & caps**:
  - When the client advertises `resolveSupport` for `documentation`,
    `completionItem/resolve` formats the variant's Markdown documentation using
    the shared hover renderer.
  - Deprecated entries carry `CompletionItemTag::DEPRECATED` and sort lower.
  - Large symbol sets filter by prefix, cap at 200 symbol/macro items, and set
    `isIncomplete: true` so typing refreshes candidates.

## Navigation and symbols

Supports `textDocument/definition`, `textDocument/references`,
`textDocument/documentHighlight`, `textDocument/prepareRename`,
`textDocument/rename`, `textDocument/documentSymbol`, and `workspace/symbol`.

- Labels (`.globl` / normal across workspace, `.L...` file-local), numeric
  labels (`1:` <-> `1f`/`1b`), `.equ`/`.equiv`/`.set`/`=` (respecting reassignment
  at cursor line), `.macro`, macro/`\.irp`/`\.irpc` parameters (`\p`), and
  `#define`s across `#include`/`.include` graphs are indexed.
- Document outline nests both numeric labels (`1:`) and file-local labels
  (`.L...`) under enclosing ordinary labels and lists macros; `workspace/symbol`
  ranks matches cleanly and omits file-local/numeric/parameter symbols.
- **Rename guards**: refuses numeric labels, symbols defined outside the
  workspace, invalid identifiers (including leading digits or `.`), architectural
  register/CSR/instruction/directive names, and names that collide within the
  target scope.
- **Exclusions**: the location counter `.` (`. = ...`) is never treated as a
  symbol, and `*.lds.S` / `*.ld.S` linker script templates are excluded from
  indexing and LSP features.

## Diagnostics and code actions

- **Toolchain discovery**: when `clang` or `llvmMc` is omitted, the server
  searches in order:
  1. `clang` / `llvm-mc` on `PATH`;
  2. the highest versioned `clang-<N>` / `llvm-mc-<N>` found on `PATH`;
  3. standard LLVM install paths (`/usr/lib/llvm-*/bin` on Linux, Homebrew
     `llvm` and `llvm@*` on macOS, `C:\Program Files\LLVM\bin` on Windows,
     newest version first).

  Any LLVM version with `riscv32` or `riscv64` target support (`clang
  -print-targets` / `llvm-mc --version`) is accepted without version warnings.
  During auto-discovery, candidates that lack RISC-V targets (such as Apple's
  system `clang`) are skipped; if no usable binary is found, only that
  feature is disabled and logged once to stderr without a `window/showMessage`
  popup. When an explicitly configured `clang` / `llvmMc` path fails to run or
  lacks RISC-V targets, a single `window/showMessage` warning is sent naming the
  setting and linking to
  `README.md#optional-llvm-for-clang-based-diagnostics-and-expansions`.
- `DiagnosticsWorker` debounces unsaved edits (`diagnostics.delayMs`, default
  300 ms), discards stale runs when document version advances, and runs:

  ```text
  clang <allow-listed flags> -iquote <file dir> -x <assembler-with-cpp|assembler> -c -o /dev/null -
  ```

- Parses `<stdin>` errors/warnings, `<instantiation>` macro diagnostics (via
  `note: while in macro instantiation`), and errors in `#include`/`.include`
  headers (mapped back to the include line with `relatedInformation`).
- **Preprocessed `#define` hints**: attaches neutral `relatedInformation`
  pointing to referenced `#define` lines, and flags prose-like `#define`s (where
  an un-commented `# ...` line expanded into invalid syntax) for `Use // comment`
  quick fixes.
- **Code actions** (`textDocument/codeAction`): replacement quick fixes from
  `clang` `"did you mean: ..."` suggestions and `Use // comment` quick fixes on
  diagnosed `#` lines (plus `REFACTOR_REWRITE` on `#` comment lines in
  preprocessed files).

## Signature help, folding ranges, and inlay hints

- **Signature help** (`textDocument/signatureHelp`, triggers ` ` and `,`):
  shows active instruction syntax variants matching `-march`/`.option` and
  highlights the current operand parameter with decoded LLVM operand
  constraints.
- **Folding ranges** (`textDocument/foldingRange`): folds multi-line `/* ... */`
  block comments (`Comment`), `.macro`/`.endm`, `.if*`/`.endif`, `.rept`/`.irp`/`.irpc`/`.endr`,
  `#if*`/`#endif` blocks (in preprocessed files), and multi-line label scopes.
- **Inlay hints** (`textDocument/inlayHint`): displays CSR hex addresses / names
  next to CSR operands and resolved `.equ` / `#define` values next to constant
  references.

## Semantic tokens

`textDocument/semanticTokens/full` and `range` emit delta-encoded UTF-16 tokens:

| Token type | Modifier | Examples |
| --- | --- | --- |
| `register` | — | `t0`, `x5`, `fa0`, `sp` |
| `csr` | — | `stvec`, `mstatus`, `scause` |
| `label` | `declaration` | `_start:`, `1:`, `1f`, `1b`, `.Lloop` |
| `constant` | `declaration` | `.equ`, `.set`, `=`, `#define` |
| `macro` | `declaration` | `.macro SAVE`, `SAVE` invocations |
| `parameter` | `declaration` | macro and `.irp`/`.irpc` parameter definitions and `\param` uses |
| `preprocessor` | — | `#define`, `#include`, `# 100`, `#` in preprocessed files |
| `comment` | — | `# ...` lines in non-preprocessed files |

## Preprocessor handling

Per-file preprocessing mode determines `#` handling, `#define` extraction,
`clang -x` language flag, semantic tokens and `#` code actions:

- Default by suffix: `.S`, `.sx`, `.h`, `.H`, `.hpp` -> preprocessed (`-x assembler-with-cpp`);
  `.s`, `.inc` -> non-preprocessed (`-x assembler`).
- Overridden per file when `compile_commands.json` specifies `-x assembler-with-cpp`
  or `-x assembler`.
