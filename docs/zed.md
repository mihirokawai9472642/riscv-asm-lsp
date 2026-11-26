# Zed Extension

Guide to installing, configuring and developing the `zed/` extension for Zed
(tested against Zed 1.29.3).

## Installing from Zed Extensions

1. Open Zed and run `zed: extensions` (or open the Extensions panel).
2. Search for **RISC-V Assembly** and click **Install**.
3. Open any `.S` or `.s` file. The extension downloads the matching
   `riscv-asm-lsp` release archive automatically from GitHub Releases into its
   extension working directory (for example `riscv-asm-lsp-v0.2.0/riscv-asm-lsp`).

### Server binary precedence and overrides

When starting the language server, the extension resolves the executable in the
following order:

1. Explicit `lsp.riscv-asm-lsp.binary` settings configured in Zed (`path` and
   optional `arguments`).
2. `riscv-asm-lsp` found on `PATH`.
3. Auto-downloaded release binary stored inside the extension's working
   directory. If GitHub Releases cannot be reached offline, it falls back to the
   newest already-downloaded binary in that directory.

To force a specific `riscv-asm-lsp` binary or wrapper command, configure
`lsp.riscv-asm-lsp.binary` in user or workspace `.zed/settings.json`:

```json
{
  "lsp": {
    "riscv-asm-lsp": {
      "binary": {
        "path": "/path/to/riscv-asm-lsp",
        "arguments": []
      }
    }
  }
}
```

## Recommended Settings

Zed extensions cannot enable `document_symbols` or `semantic_tokens` by default
from `config.toml`, so add these in user or workspace `.zed/settings.json`:

```json
{
  "languages": {
    "RISC-V Assembly": {
      "document_symbols": "on",
      "semantic_tokens": "combined",
      "inlay_hints": {
        "enabled": true
      }
    }
  }
}
```

- `document_symbols: "on"` populates outline and breadcrumbs from the server's
  label and macro hierarchy.
- `semantic_tokens: "combined"` overlays server semantic classification
  (`register`, `csr`, `label`, `constant`, `macro`, `parameter`, `preprocessor`,
  `comment`) on top of tree-sitter syntax highlighting.
- `inlay_hints.enabled: true` (optional, off by default in Zed) displays CSR hex
  addresses/names and resolved `.equ`/`#define` values inline.
- Language server options under `lsp.riscv-asm-lsp.settings` apply live without
  restarting the server; options under
  `lsp.riscv-asm-lsp.initialization_options` act as the startup base layer and
  are preserved when `workspace/didChangeConfiguration` sends partial, `null`,
  or `{}` settings updates (nested objects such as `diagnostics` are merged
  key-by-key; see [README.md](../README.md#configuration)).

### Theme Overrides

Themes that omit `variable.special` can colour registers explicitly via
`theme_overrides`:

```json
{
  "theme_overrides": {
    "Ayu Dark": {
      "syntax": {
        "variable.special": { "color": "#95e6cb" }
      }
    }
  }
}
```

## Avoiding File-Type Conflicts

### Other assembly extensions

The Zed extension registry includes generic assembly extensions (such as
`assembly`) that also claim `.S` or `.s` suffixes. If you have another
extension installed that claims `.S`, `.s`, or `.sx`, uninstalling the
conflicting extension is the most reliable fix. You can also map those suffixes
explicitly to `RISC-V Assembly` via `file_types` (note the interaction with
`*.lds.S` described below):

```json
{
  "file_types": {
    "RISC-V Assembly": ["S", "s", "sx"]
  }
}
```

### Linker script templates (`*.lds.S`, `*.ld.S`)

Kernel and firmware trees often preprocess linker scripts via `.S` templates
such as `vmlinux.lds.S` or `sections.ld.S`. The `riscv-asm-lsp` server skips
those files automatically; in Zed, you can map them away from `RISC-V Assembly`
with globs in `file_types`:

```json
{
  "file_types": {
    "Plain Text": ["*.lds.S", "*.ld.S"]
  }
}
```

Zed tests each `file_types` entry as a glob without comparing match lengths,
searching languages from the most recently registered one. Because `"*.lds.S"`
and `"S"` both match `vmlinux.lds.S` when both appear in `file_types`,
`RISC-V Assembly` (registered after built-in languages such as `Plain Text`)
wins if you also map `"S"` to `RISC-V Assembly` in `file_types`. Use the
`*.lds.S` / `*.ld.S` rule only when `RISC-V Assembly` relies on its own
`path_suffixes` (without `"S"` in `file_types`); if another assembly extension
conflicts on `.S`, uninstall that conflicting extension instead. Because
`riscv-asm-lsp` ignores linker script templates either way, the only remaining
effect if they open as `RISC-V Assembly` is incorrect syntax highlighting.

## Developing the Extension

To test local changes to `zed/`:

1. Open Zed and run the command `zed: install dev extension`.
2. Select the `zed/` directory of this repository.
3. Because the extension includes Rust code (`zed/src/lib.rs`), the machine
   running Zed needs `git` and Rust (`rustup`) on `PATH`. Zed installs the
   `wasm32-wasip2` target automatically and compiles the extension.
4. Put your locally built `riscv-asm-lsp` on `PATH` or point
   `lsp.riscv-asm-lsp.binary` to it; otherwise the extension downloads the
   latest published release archive.

## Optional Setups

### Running the Server in a Development Container

When your toolchain lives inside a Docker Compose container mounting the
project at the same path inside the container as on the host, install a pinned
`riscv-asm-lsp-<target>.tar.gz` release into `/usr/local/bin` (verifying its
`.sha256` checksum) in the container image (the static musl binary does not
depend on the container's glibc version) and configure
`lsp.riscv-asm-lsp.binary`:

```json
{
  "lsp": {
    "riscv-asm-lsp": {
      "binary": {
        "path": "/usr/bin/docker",
        "arguments": ["compose", "exec", "-T", "dev", "riscv-asm-lsp"]
      }
    }
  }
}
```

Settings placed in a workspace's `.zed/settings.json` only apply when that
directory is the opened project root. If you open a parent directory, either
place `riscv-asm-lsp` on `PATH` or put `lsp.riscv-asm-lsp.binary` in your user
settings.

### Zed on Windows with WSL Projects

These notes apply when developing the `zed/` extension locally on Windows + WSL:

- Zed links dev extensions with a Windows directory junction, which cannot point
  into `\\wsl.localhost\...`. Copy `zed/` (`extension.toml`, `LICENSE`,
  `Cargo.toml`, `Cargo.lock`, `src/`, `languages/`) to a local Windows folder
  (outside OneDrive) and install the dev extension from there. After editing
  the extension, copy again and click **Rebuild**.
- Install Git for Windows and `rustup` on Windows. With `rustup`'s default MSVC
  toolchain, building needs Visual Studio Build Tools; the GNU toolchain
  (`stable-x86_64-pc-windows-gnu`) provides its own linker.
- The Rust/WebAssembly part (`zed/Cargo.toml`, `zed/src/lib.rs`) is required even
  when `lsp.riscv-asm-lsp.binary` is configured, because remote WSL/SSH
  projects load the extension's WebAssembly module on the remote side to launch
  language servers.

## Extension Internals

| Item | Details |
| --- | --- |
| Language & ID | Language `RISC-V Assembly`, extension ID `riscv-assembly` (`zed/extension.toml`) |
| Path suffixes | `S`, `s`, `sx`, and `riscv` (so ```` ```riscv ```` hover code fences highlight consistently) |
| Grammar | `tree-sitter-riscv` (MIT, pinned at `c89b7c525d90c7375129f888e4a37b4485927c75`) |
| Queries (`zed/languages/riscv_assembly/`) | `highlights.scm` (with `@preproc` support), `injections.scm` (C preprocessor injection), `overrides.scm`, `brackets.scm`, `indents.scm`, `outline.scm`, `semantic_token_rules.json` |
| Editor config (`config.toml`) | `line_comments = ["// ", "# "]` (`// ` first so toggling comments in `.S` avoids creating `#` preprocessor directives), table-form block/doc comments, `/* */` auto-close, bracket/quote pairs, label and `.macro`/`.if`/`.rept`/`.irp`/`.irpc` indent rules |
| WebAssembly entry (`zed/src/lib.rs`) | Resolves `lsp.riscv-asm-lsp.binary` -> `PATH` -> GitHub Release auto-download across Linux (`x86_64`/`aarch64`), macOS (`aarch64`/`x86_64`), and Windows (`x86_64`) with offline cached fallback; forwards `initialization_options` and `workspace/configuration` (`settings`); implements `label_for_completion` to keep syntax-bearing completion labels highlighted while filtering on mnemonics |
| License | MIT copy at `zed/LICENSE` |
