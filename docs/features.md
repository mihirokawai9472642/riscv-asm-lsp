# Features

Examples of hover output are in [tests/snapshots/hover/](../tests/snapshots/hover/).

## Hover

| Target | Shows |
| --- | --- |
| Instruction, alias, pseudo-instruction | Long name, extensions, RV32/RV64, syntax variants, operand constraints, a notice when it is disabled by `-march` / `.option`, deprecated, or not accepted by LLVM, and the expansion and encoding of the current line |
| CSR (`stvec`, or a number in a CSR operand) | Name, address, privilege mode and fields |
| Register (`x10`, `a0`) | Class and calling-convention role |
| Directive | Arguments, description, and whether LLVM accepts it |
| `%hi`, rounding modes, `fence` sets, `v0.t` | What the keyword means in that operand |
| Labels, `.equ` / `.set` / `=`, macros, macro parameters, `#define`, `1f` / `1b`, `\@` | Kind, defining file and line, and the value of constants |
| `#include`, `.include` | The resolved path, or how to fix the search path |

The expansion and encoding come from the built-in engine (LLVM's `li`
sequence, pseudo-instruction and alias expansions, compressed forms when `C`
is enabled). With `diagnostics.external` enabled, `llvm-mc` produces them
instead:

```text
li a0, 0x12345678   ->  lui a0, 74565 · 0x12345537 / addi a0, a0, 1656 · 0x67850513
csrw stvec, t0      ->  csrrw zero, stvec, t0 · 0x10529073
mv a0, a1           ->  c.mv a0, a1 · 0x852e
```

## Completion

- At the start of a statement: instructions enabled by the active `-march` and
  `.option`, directives, macros and `#define`s. Instructions insert their
  operands as snippets.
- In operands: registers, CSRs, symbols, `%` relocation functions and macro
  parameters, depending on the operand.
- After `.option` and `.section`: their arguments and extension or section
  names.
- Nothing is offered inside comments.

## Navigation

Go to definition, references, highlight, rename, document outline, workspace
symbols and call hierarchy work for labels, numeric labels (`1f` / `1b`),
`.equ` / `.set` / `=`, macros, macro parameters and `#define`s, across files and
included headers.

- `.L` labels are local to their file; `.set` / `=` resolve to the assignment
  active at the cursor.
- Rename refuses numeric labels, symbols outside the workspace, invalid names,
  names of registers, CSRs, instructions or directives, and names that would
  collide.
- The location counter `.` and linker script templates (`*.lds.S`, `*.ld.S`)
  are ignored.

## Diagnostics

The built-in validator checks unsaved text after preprocessing. Each
diagnostic has a code that `diagnostics.suppress` or `.riscv-asm-lsp.yaml` can
hide:

| Code | Severity | Meaning |
| --- | --- | --- |
| `unknown-mnemonic` | Error | Unknown instruction (with suggestions) |
| `unknown-directive` | Error | Unknown directive |
| `extension-disabled` | Error | The instruction needs an extension that is not enabled |
| `xlen-mismatch` | Error | The instruction is for the other XLEN |
| `operand-count` | Error | Wrong number of operands |
| `invalid-register` | Error | Wrong or missing register |
| `invalid-immediate` | Error | Immediate out of range or misaligned |
| `invalid-csr` | Error | Unknown or disabled CSR |
| `gnu-only-directive` | Warning | GNU `as` accepts the directive, LLVM does not |
| `inactive-branch` | Hint | Inactive `#if` branch, shown greyed out |

To avoid false errors, the validator skips lines it cannot be sure about:
inactive branches, lines that depend on a missing `#include`, unexpanded
macros, `.macro` / `.irp` / `.rept` bodies, `%` relocations and expressions
with unknown symbols.

With `diagnostics.external` enabled and `clang` found, diagnostics come from
`clang` instead, including errors inside macros and included files. The two
are never mixed.

Code actions offer `clang`'s "did you mean" fixes and turn `#` comments that
the preprocessor would misread into `//` comments.

## Other features

- **Signature help** shows the syntax variants of the instruction and the
  current operand.
- **Folding** covers block comments, `.macro`, `.if`, `.rept` / `.irp`, `#if`
  blocks and label scopes.
- **Inlay hints** show CSR addresses and the values of `.equ` and `#define`
  constants.
- **Semantic tokens** classify registers, CSRs, labels, constants, macros,
  parameters, preprocessor lines and comments, and grey out inactive `#if`
  branches.

## Preprocessing

In preprocessed files (see [configuration.md](configuration.md#compilation-database)),
the built-in preprocessor evaluates `#if` / `#ifdef` / `#elif`, expands
object-like and function-like macros, follows `#include`, and defines the
macros clang predefines for RISC-V (`__riscv`, `__riscv_xlen`,
`__riscv_<ext>`, ...) from `-march` and `-mabi`. In files that are not
preprocessed, `#` starts a comment.
