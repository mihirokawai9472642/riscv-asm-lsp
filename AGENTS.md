# AGENTS.md

Instructions for coding agents working in this repository. `README.md` is the
user overview; `docs/design.md` describes the architecture and design decisions;
`docs/features.md`, `docs/data.md`, and `docs/zed.md` document feature
behaviour, generated data, and the Zed extension.

## Project overview

`riscv-asm-lsp` is a language server for RISC-V assembly in the GNU assembler
syntax, written in Rust, with a Zed extension that uses it. The LLVM
integrated assembler (`clang`, `llvm-mc`, version 23) is the reference: what it
accepts is what the server offers.

| Path | Contents |
| --- | --- |
| `server/` | The language server crate (binary `riscv-asm-lsp`) |
| `server/data/` | Generated instruction data (`riscv.json`, `report.md`) and hand-written files (`directives_gnu.json`, `supplements.json`, `texts.json`) |
| `xtask/` | Data generation (`cargo xtask gen-data`) |
| `zed/` | The Zed extension (its own crate, built for `wasm32-wasip2`; not a workspace member) |
| `tests/` | Integration tests, fixtures, and golden snapshots (`tests/snapshots/`) |
| `docs/` | Design and reference documentation (`design.md`, `features.md`, `data.md`, `zed.md`) |
| `THIRD_PARTY_NOTICES.md` | Attribution and licenses of imported data and dependencies |

### Crate module layout

`server/src/`:
- Core & LSP: `main.rs`, `lib.rs`, `server.rs` (`ServerState` + typed dispatch), `config.rs`, `tools.rs`
- Data & text primitives: `data.rs`, `position.rs`, `line.rs` (shared comment/statement scanner), `preprocessor.rs`, `ast.rs` (shared tree-sitter AST visitor), `document.rs`
- Workspace & compile DB: `context/{mod.rs, march.rs, compile_db.rs, uri.rs}`, `index/{mod.rs, parse.rs, scope.rs}`
- Features: `hover/{mod.rs, expand.rs, operands.rs, format.rs}`, `completion/{mod.rs, candidates.rs, snippets.rs}`, `navigation.rs`, `symbols.rs`, `semantic_tokens.rs`, `diagnostics/{mod.rs, worker.rs, clang.rs, parse.rs, hints.rs}`, `code_action.rs`

`xtask/src/`: `main.rs`, `pins.rs`, `fetch.rs`, `llvm.rs`, `udb.rs`, `asm_manual.rs`, `join.rs`, `report.rs`.

Read the relevant sections of `docs/design.md` (and `docs/features.md` /
`docs/data.md`) before changing behaviour. When a change departs from the
design, update the docs in the same change, or ask first if the departure is a
decision rather than a detail.

## Commands

Rust, LLVM and the other tools run in a container, not in WSL. Run every
command from the repository root; `compose.yaml` mounts the repository at the
same path inside the container.

- `docker compose up -d --build`: builds and starts the container. The other
  commands need it running.
- `docker compose exec dev cargo build`: builds the workspace.
- `docker compose exec dev cargo test`: runs unit tests and non-tool integration
  tests; `docker compose exec dev cargo test -- --include-ignored` also runs
  the `clang` / `llvm-mc` integration tests (marked `#[ignore]`).
- `docker compose exec -e UPDATE_SNAPSHOTS=1 dev cargo test --test integration snapshots -- --include-ignored`:
  regenerates golden snapshot files under `tests/snapshots/` after intentional
  output changes; review every changed snapshot in `git diff`.
- `docker compose exec dev cargo clippy --all-targets -- -D warnings`: lints.
- `docker compose exec dev cargo fmt`: formats; `cargo fmt --check` checks.
- `docker compose exec dev cargo deny check licenses`: checks dependency
  licenses against `deny.toml`.
- The Zed extension crate is outside the workspace; pass its manifest:
  `docker compose exec dev cargo clippy --manifest-path zed/Cargo.toml --target wasm32-wasip2 -- -D warnings`
  and `docker compose exec dev cargo fmt --manifest-path zed/Cargo.toml --check`.
- `docker compose exec dev cargo xtask gen-data`: regenerates `server/data/` from the
  pinned inputs. It downloads LLVM `.td` files, `riscv-unified-db` and
  `riscv-asm-manual`, so run it only when changing the data or its generator.

Do not install Rust, LLVM or other project tools in WSL. Add them to the
`Dockerfile` and rebuild the container.

Run `git` in WSL, not in the container: commit signing and SSH are not
available there.

## Code style

- Rust stable, edition 2024 (minimum Rust version 1.90, required by
  `tree-sitter` 0.27; checked by the `msrv` CI job). Formatting follows
  `rustfmt`; clippy reports no warnings (`too_many_arguments` and
  `mutable_key_type` suppressions are avoided; any `#[allow(deprecated)]`
  requires an explanatory comment).
- Keep dependencies few. Before adding one, check that its license is
  permissive (MIT, Apache-2.0, BSD, zlib or similar) and that it is
  maintained. Do not copy code or text from GPL or GFDL sources (for example
  binutils or the GNU as manual).
- The server must never panic on any input or request:
  - no `unwrap`, `expect` or unchecked indexing on anything derived from
    client input, file contents or tool output;
  - positions from the client are UTF-16 and positions from `clang` are byte
    columns; convert them at the boundary (`docs/design.md` section 4.1) and
    never index text with an unconverted column;
  - when an external tool is missing or fails, disable only the feature that
    needs it and log the problem once.
- Advertise only capabilities that are implemented, and answer every request
  for them.
- Names are descriptive; abbreviations only where they are standard (LSP,
  CSR, ABI, XLEN).
- Files, comments, commit messages and user-facing text are in English.
  Comments explain why, in plain sentences.

## Data

- Files in `server/data/` marked as generated (`riscv.json`, `report.md`) are written
  by `cargo xtask gen-data`; never edit them by hand. Hand-written files say so
  (`directives_gnu.json`, `supplements.json`, `texts.json` in their `notice`
  field).
- The pinned input versions live in one place in `xtask/src/pins.rs`. Changing
  a pin means regenerating and reviewing the diff of `server/data/report.md`.
- Imported data keeps its source license. When adding a source, record it in
  `THIRD_PARTY_NOTICES.md` and in `docs/data.md`.

## Testing

- `cargo test`, `cargo clippy --all-targets -- -D warnings`,
  `cargo fmt --check` and `cargo deny check licenses` pass before every
  commit, and so do clippy and fmt for `zed/` when it changed.
- New behaviour comes with an integration test that drives the server over
  JSON-RPC, using the fixtures in `tests/`.
- A fixed bug comes with a regression test. Keep tests covering regression
  inputs: multibyte characters, positions past the end of a line, empty lines,
  and block/line comment boundaries.
- Behaviour-preserving refactors keep golden snapshots under `tests/snapshots/`
  passing without setting `UPDATE_SNAPSHOTS=1`. Intentional output changes
  update the snapshots via `UPDATE_SNAPSHOTS=1` and inspect the diff.
- Tests that need `clang` or `llvm-mc` are marked `#[ignore]` and run in the
  container with `-- --include-ignored` (or `--test integration -- --ignored`);
  the rest of the suite must pass with those tools absent from `PATH`.

## Security

- No secrets in the repository.
- The server runs `clang` and `llvm-mc` with flags taken from the project's
  compilation database. Build those commands as argument lists, never through
  a shell.

## Git workflow

The project follows GitHub Flow: `main` always builds and passes the tests,
and every change reaches it through a pull request.

- Make each change on its own branch created from an up-to-date `main`,
  named after the change with the commit type as a prefix (for example
  `feat/hover-csr`). Do not commit to `main` directly.
- Push the branch and open a pull request only when the user asks. The pull
  request says what changed and why, and how it was tested; when a coding
  agent contributed, it says so there as well.
- Merging happens through the pull request after review; delete the branch
  afterwards.
- Dependabot pull requests run the CI workflow and are reviewed and merged
  through the pull request like other changes; `tree-sitter-riscv` and
  `zed_extension_api` are ignored in `.github/dependabot.yml` and updated by
  hand together with `zed/extension.toml` / the supported Zed version.

## Commits

- Message format: `type(scope): summary` following Conventional Commits
  (feat, fix, refactor, docs, test, build, ...), then a short body when
  needed. Scopes are the top-level directories (`server`, `xtask`, `data`,
  `zed`, `tests`, `docs`).
- When a coding agent contributed to a commit, add a trailer naming it,
  with the model when known:
  - `Assisted-By: <tool> [model]` when a person materially revised the work.
  - `Generated-By: <tool> [model]` when it was mostly written by the agent.

  Do not use `Co-Authored-By` for AI tools; it is meant for human co-authors.
