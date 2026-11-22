# Instruction Data

Reference for the embedded datasets built by `xtask/` and stored under `data/`.

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
| Common GNU/LLVM assembler directives (`data/directives_gnu.json`) | Hand-written for `riscv-asm-lsp` (GNU `as` GFDL manual not used) | `MIT` |
| Supplemental descriptions, deprecations & text fixes (`data/supplements.json`) | Hand-written for `riscv-asm-lsp` | `MIT` |
| Relocation descriptions, `#` preprocessor texts, `\@` explanation, 51 operand classes (`data/texts.json`) | Hand-written for `riscv-asm-lsp` | `MIT` |

## Pinned inputs

All pins live in one place in `xtask/src/pins.rs`:

| Input | Pinned Version | Notes |
| --- | --- | --- |
| LLVM | Tag `llvmorg-19.1.7` | Sparse checkout of `.td` files under `llvm/include/llvm` and `llvm/lib/Target/RISCV` |
| riscv-unified-db | Commit `5d1ded1f93ab` | Upstream format may evolve across commits |
| riscv-asm-manual | Commit `27d6e9e67e6d` | Pseudo-instruction and directive tables |

Updating a pin requires regenerating `data/` and reviewing the diff of
`data/report.md`.

## Generation

Run inside the development container (needs `llvm-tblgen`):

```bash
docker compose exec dev cargo xtask gen-data
```

Generation is deterministic (`data/riscv.json` and `data/report.md` use sorted
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
   `data/report.md`.
3. **riscv-asm-manual (`xtask/src/asm_manual.rs`)**: parses Markdown/AsciiDoc
   pseudo-instruction and directive tables cleanly without duplicating
   `expands to` clauses.
4. **Join & Clean (`xtask/src/join.rs`)**: merges by canonical/deprecated name,
   applies `data/supplements.json` descriptions, deprecation markers, and typo
   corrections, and generates concise `summary` strings (stripping leading
   markdown title headings or syntax prefixes, capped at 220 characters for
   entries and 100 characters for CSR fields and registers).
5. **Report (`xtask/src/report.rs`)**: emits `data/report.md` summarizing
   matches, upstream gaps, cleaned `TODO` items, and applied corrections.

## Data format

- **`data/riscv.json`** (compact JSON embedded via `include_str!`):
  - `meta`: pinned input versions.
  - `extensions`: `-march` name, LLVM feature name, description.
  - `instructions`: `mnemonic`, `kind` (`instruction` | `alias` | `pseudo`),
    `variants` (`syntax`, `operands` with LLVM operand class, `required_extensions`,
    `xlen`, `alias_of`), `udb_assembly`, `long_name`, `summary`, `description`,
    `description_source`, `llvm_supported`, `deprecated`, `preferred_name`.
  - `csrs`: `name`, `number`, privilege/mode, extensions, `summary`,
    `description`, `fields` (each with optional `summary`), `deprecated_names`.
  - `registers`: `name`, ABI names, `class`, `summary`, `description`, saver
    convention.
  - `directives`: `name`, arguments, `description`, source.
- **`data/texts.json`** (embedded via `include_str!`):
  - `relocations`: `%hi(`, `%lo(`, `%pcrel_hi(`, `%pcrel_lo(`, `%tprel_hi(`,
    `%tprel_lo(`, `%tprel_add(`, `%got_pcrel_hi(`.
  - `preprocessor_directives`: 14 `#` preprocessor directives (`define`,
    `undef`, `include`, `if`, ...).
  - `macro_counter`: description for `\@`.
  - `operand_classes`: human-readable labels for the 51 non-immediate LLVM
    operand classes (combined with 26 immediate decoders in
    `server/src/hover/operands.rs`, covering all 77 classes in `data/riscv.json`).

## LLVM 19.1.7 vs. Documentation

Summary from `data/report.md` (LLVM `llvmorg-19.1.7` vs. riscv-unified-db
`5d1ded1f93ab`, excluding vendor extensions and `.insn_*` internal formats):

| Metric | Count | Notes |
| --- | ---: | --- |
| Extensions | 154 | |
| Instructions & aliases (LLVM supported) | 1432 | 1381 in UDB + LLVM, 21 `riscv-asm-manual`, 30 `supplements.json`, 0 undescribed |
| Only in riscv-unified-db | 99 | e.g. Q extension (34), newer extensions (`Zilx`, `Zvabd`, ...), older names (`fmv.s.x`) |
| `rv64gc` instructions in UDB missing from LLVM | 0 | |
| CSRs (LLVM supported) | 427 | 401 in UDB + LLVM, 26 `supplements.json`, 0 undescribed |
| CSRs only in riscv-unified-db | 16 | e.g. `Smctr`, `Sdtrig` (`tcontrol`), `Smcntrpmf` (`mcyclecfg`) |
| Registers | 96 | |
| Directives | 137 | Combined `riscv-asm-manual` and `data/directives_gnu.json` |
| Cleaned `TODO`s from riscv-unified-db | 6 | `htinst`, `hvip.VSEIP`, `hvip.VSTIP`, `hvip.VSSIP`, `mncause.CODE`, `mtinst` |

Mismatch handling:

| Case | Runtime Behaviour |
| --- | --- |
| LLVM-accepted with description | Normal hover & `-march`-filtered completion |
| Deprecated aliases (`frsr`, `vpopc.m`, ...) | Marked deprecated in hover (showing `preferred_name`) and completion (`DEPRECATED` tag, lower sort rank) |
| Documentation-only (`llvm_supported: false`) | Excluded from completion; hover warns `"Not accepted by LLVM 19 (spec only)."` |
| Deprecated/renamed entries | Joined via LLVM deprecated names with deprecation metadata |

## Data licenses

All imported data keeps its upstream license (`Apache-2.0 WITH LLVM-exception`,
`BSD-3-Clause-Clear`, `CC-BY-4.0`), and project-authored JSON files
(`directives_gnu.json`, `supplements.json`, `texts.json`) are MIT. Complete
notices appear in [THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md).
