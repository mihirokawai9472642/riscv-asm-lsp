# Instruction Data

Reference for the embedded datasets built by `xtask/` and stored under `server/data/`.

## Sources

The guiding rule: **LLVM decides what exists; documentation supplies
descriptions.**

| Information | Source | License |
| --- | --- | --- |
| Instructions, aliases & pseudo-instructions, syntax, operand classes, predicates | LLVM TableGen (`llvm-tblgen --dump-json`) | `Apache-2.0 WITH LLVM-exception` |
| CSR names, numbers, required extensions, deprecated names | LLVM TableGen (`SysReg`) | `Apache-2.0 WITH LLVM-exception` |
| Registers and ABI names | LLVM TableGen (`Register`, `AltNames`) | `Apache-2.0 WITH LLVM-exception` |
| Extensions and `-march` names | LLVM TableGen (`SubtargetFeature`) | `Apache-2.0 WITH LLVM-exception` |
| Instruction names, descriptions and assembly formats | riscv-unified-db (`spec/std/isa/inst/`) | `BSD-3-Clause-Clear` |
| Single-instruction aliases | riscv-unified-db (`pseudoinstructions`) | `BSD-3-Clause-Clear` |
| CSR descriptions and fields | riscv-unified-db (`spec/std/isa/csr/`) | `BSD-3-Clause-Clear` |
| Register descriptions and calling convention | riscv-unified-db (`register_file/`) | `BSD-3-Clause-Clear` |
| Expanding pseudo-instructions (`la`, `li`, `call`, ...) & RISC-V directives | riscv-asm-manual tables | `CC-BY-4.0` |
| Common GNU/LLVM assembler directives (`server/data/directives_gnu.json`) | Hand-written for `riscv-asm-lsp` (GNU `as` GFDL manual not used) | `MIT` |
| Supplemental descriptions, deprecations & text fixes (`server/data/supplements.json`) | Hand-written for `riscv-asm-lsp` | `MIT` |
| Relocation descriptions, `#` preprocessor texts, `\@` explanation, 65 operand classes (`server/data/texts.json`) | Hand-written for `riscv-asm-lsp` | `MIT` |

## Pinned inputs

All pins live in one place in `xtask/src/pins.rs`:

| Input | Pinned Version | Notes |
| --- | --- | --- |
| LLVM | Tag `llvmorg-23.1.6` | Sparse checkout of `.td` files under `llvm/include/llvm` and `llvm/lib/Target/RISCV` |
| riscv-unified-db | Commit `5d1ded1f93ab` | Upstream format may evolve across commits |
| riscv-asm-manual | Commit `27d6e9e67e6d` | Pseudo-instruction and directive tables |

Updating a pin requires regenerating `server/data/` and reviewing the diff of
`server/data/report.md`.

### Updating the reference LLVM version

The server uses one pinned stable LLVM release updated periodically:

1. Bump `LLVM_TAG` in `xtask/src/pins.rs`, the `apt.llvm.org` / package version in `Dockerfile`, and `.github/workflows/ci.yml`, then rebuild the `dev` container.
2. Run `cargo xtask gen-data` and inspect the `server/data/report.md` diff carefully (add new non-vendor CSR/instruction descriptions to `server/data/supplements.json` and extend vendor filters in `xtask/src/llvm.rs` if new vendor `.td` files or predicates were added upstream).
3. Run `cargo test --lib` and decode any newly added operand classes in `server/data/texts.json` (`operand_classes`) and `server/src/hover/operands.rs`.
4. Re-check the [directive table](#directive-support-in-llvm-23) against `llvm-mc` (updating `xtask/src/asm_manual.rs` if support changed) and verify `march_implications_match_clang_print_enabled_extensions` and `data_validation_rv64gc_instructions_assemble_with_llvm_mc` with `--include-ignored`.
5. Run `UPDATE_SNAPSHOTS=1` for snapshot tests if upstream wording or predicates changed intentionally, review every changed snapshot, and update version references in `README.md` and `docs/`.

## Generation

Run inside the development container (needs `llvm-tblgen`):

```bash
docker compose exec dev cargo xtask gen-data
```

Generation is deterministic (`server/data/riscv.json` and `server/data/report.md` use sorted
maps and stable ordering; re-running yields byte-identical output). Because the
outputs are committed, building `riscv-asm-lsp` requires neither LLVM `.td`
sources nor network access.

Pipeline stages:

1. **LLVM TableGen (`xtask/src/llvm.rs`)**: dumps JSON records, keeps
   assembler-visible `Instruction` records (non-`isCodeGenOnly` with `AsmString`),
   `InstAlias`, `SysReg`, `Register`, and `SubtargetFeature`, and drops
   vendor-specific predicates (`HasVendor...`).
2. **riscv-unified-db (`xtask/src/udb.rs`)**: parses YAML files, resolves
   `<%- ... -%>` / `{hpm_num}` placeholders, strips AsciiDoc passthroughs
   (`++`, preserving `C++`), tables/xrefs and stray attribute refs (keeping
   instruction notation such as `{u}`), decodes HTML entities, and removes
   upstream `"TODO"` placeholder sentences while recording them for
   `server/data/report.md`.
3. **riscv-asm-manual (`xtask/src/asm_manual.rs`)**: parses Markdown/AsciiDoc
   pseudo-instruction and directive tables cleanly without duplicating
   `expands to` clauses.
4. **Join & Clean (`xtask/src/join.rs`)**: merges by canonical/deprecated name,
   normalizes UDB `xd`/`xs1`/`xs2`/`xs3` register placeholders to match LLVM's
   `rd`/`rs1`/`rs2`/`rs3` operands, derives privileged/counter `isa_label`s
   (`Sm`, `S`, `Sdext`, `Zicntr`), applies `server/data/supplements.json` descriptions,
   CSR extension requirements, deprecation markers, and typo corrections, and
   generates concise punctuation-terminated `summary` strings (capped at 260
   characters for entries and 140 characters for CSR fields and registers).
5. **Report (`xtask/src/report.rs`)**: emits `server/data/report.md` summarizing
   matches, upstream gaps, cleaned `TODO` items, and applied corrections.

## Data format

- **`server/data/riscv.json`** (compact JSON embedded via `include_str!`):
  - `meta`: pinned input versions.
  - `extensions`: `-march` name, LLVM feature name, `version` (`major.minor`),
    `implies` (implied canonical extension names from TableGen `Implies`),
    `experimental` flag, and description.
  - `instructions`: `mnemonic`, `kind` (`instruction` | `alias` | `pseudo`),
    `variants` (`syntax`, `operands` with LLVM operand class, `required_extensions`,
    `xlen`, `alias_of`, optional TableGen bit `encoding` template `{size, fixed,
    fields}` for real instructions, and optional `alias_expansion` for aliases),
    `udb_assembly`, `isa_label`, `long_name`, `summary`,
    `description`, `description_source`, `llvm_supported`, `deprecated`,
    `preferred_name`.
  - `csrs`: `name`, `number`, privilege/mode, `required_extensions`, `summary`,
    `description`, `fields` (each with `bits`, optional `bits_rv32`, and
    optional `summary`), `deprecated_names`.
  - `registers`: `name`, ABI names, `class`, `summary`, `description`, saver
    convention.
  - `directives`: `name`, arguments, `description`, source.
- **`server/data/texts.json`** (embedded via `include_str!`):
  - `relocations`: `%hi(`, `%lo(`, `%pcrel_hi(`, `%pcrel_lo(`, `%tprel_hi(`,
    `%tprel_lo(`, `%tprel_add(`, `%got_pcrel_hi(`.
  - `preprocessor_directives`: 22 `#` preprocessor directives (`define`,
    `undef`, `include`, `if`, ...).
  - `macro_counter`: description for `\@`.
  - `operand_classes`: human-readable labels for the 65 register, special, and
    non-formulaic LLVM operand classes (combined with the mechanical immediate
    decoders in `server/src/hover/operands.rs`).

## LLVM vs. documentation

[`server/data/report.md`](../server/data/report.md) lists, for the pinned
inputs, which entries LLVM and riscv-unified-db agree on, which come from
riscv-asm-manual or `supplements.json`, which have no description, and which
`TODO`s were cleaned. It is regenerated with the data.

Mismatch handling:

| Case | Runtime Behaviour |
| --- | --- |
| LLVM-accepted with description | Normal hover & `-march`-filtered completion |
| Deprecated aliases (`frsr`, `vpopc.m`, ...) | Marked deprecated in hover (showing `preferred_name`) and completion (`DEPRECATED` tag, lower sort rank) |
| Documentation-only (`llvm_supported: false`) | Excluded from completion; hover warns `"Not accepted by LLVM 23 (spec only)."` |
| Deprecated/renamed entries | Joined via LLVM deprecated names with deprecation metadata |

## Directive support in LLVM 23

Checked against `llvm-mc` 23.1.6. LLVM decides: for example, riscv-asm-manual
lists `.dtprelword`, but LLVM 23 rejects it.

| Status | Directives |
| --- | --- |
| Not supported | `.eqv`, `.func`/`.endfunc`, `.float16`, `.bfloat16`, `.dtprelword`, `.dtpreldword`, `.linkonce`, `.stabs`, listing directives (`.list`, `.nolist`, `.title`, `.sbttl`, `.psize`, `.mri`) |
| Supported (representative) | `.macro` (including `\@`), `.altmacro`, `.irp`, `.rept`, `.equ`, `.set`, `=`, `.equiv`, `.option` (`arch`, `push`, `pop`, `exact`), `.insn`, `.attribute`, `.variant_cc`, `.cfi_*`, `.pushsection`, `.incbin`, `.ifc`, `.ifb`, `.reloc`, `.print`, `.warning`, `.int`, `.weakref`, `.line`, `.version`, `.dc`, `.dcb`, `.ds`, `.ifnotdef`, `.bundle_*`, `.base64` |

## Data licenses

Imported data keeps its upstream license (see the table above); the
hand-written JSON files are MIT. Full notices are in
[THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md).
