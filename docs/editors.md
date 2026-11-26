# Editor Setup

riscv-asm-lsp talks LSP over stdio. Settings are the same in every editor; see
[configuration.md](configuration.md).

## Zed

The `zed/` extension adds the `RISC-V Assembly` language (`.S`, `.s`, `.sx`)
with a tree-sitter grammar and starts the server. Until it is published in the
Zed extension registry, install it as a dev extension: run
`zed: install dev extension` and select the `zed/` directory. Building it needs
`git` and Rust (`rustup`) on the machine running Zed.

The extension uses the first server it finds:

1. `lsp.riscv-asm-lsp.binary` in Zed's settings;
2. `riscv-asm-lsp` on `PATH`;
3. a release it downloads from GitHub (or one downloaded earlier, when
   offline).

Zed extensions cannot turn on document symbols or semantic tokens themselves,
so add them in your settings:

```json
{
  "languages": {
    "RISC-V Assembly": {
      "document_symbols": "on",
      "semantic_tokens": "combined",
      "inlay_hints": { "enabled": true }
    }
  }
}
```

### Running the server in a container

If your toolchain lives in a Docker Compose container that mounts the project
at the same path, install a release binary in the container and start it
through `docker compose`:

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

A workspace `.zed/settings.json` applies only when that directory is the
opened project root.

### File type conflicts

Other extensions (such as `assembly`) also claim `.S` and `.s`. Uninstalling
them is the simplest fix; otherwise map the suffixes in `file_types`:

```json
{
  "file_types": { "RISC-V Assembly": ["S", "s", "sx"] }
}
```

Linker script templates (`*.lds.S`, `*.ld.S`) are ignored by the server. To
open them as plain text, map `"Plain Text": ["*.lds.S", "*.ld.S"]`; this only
works when `"S"` is not also mapped to `RISC-V Assembly` in `file_types`.

Themes without a `variable.special` colour can set one for registers in
`theme_overrides`.

## Neovim

Install the server on `PATH` (see the [README](../README.md#installing-a-release),
or `cargo binstall riscv-asm-lsp --git https://github.com/mihirokawai9472642/riscv-asm-lsp`).
With Neovim 0.11 or newer, add to `init.lua`:

```lua
vim.lsp.config('riscv_asm_lsp', {
  cmd = { 'riscv-asm-lsp' },
  filetypes = { 'asm', 's', 'S' },
  root_markers = { 'compile_commands.json', 'compile_flags.txt', '.riscv-asm-lsp.yaml', '.git' },
  init_options = { defaultMarch = 'rv64gc' },
})
vim.lsp.enable('riscv_asm_lsp')
```

## Helix

Add to `~/.config/helix/languages.toml` (Helix calls GNU assembly `gas`):

```toml
[language-server.riscv-asm-lsp]
command = "riscv-asm-lsp"

[[language]]
name = "gas"
file-types = ["s", "S", "sx"]
language-servers = ["riscv-asm-lsp"]
```

## Troubleshooting

- Check the version with `riscv-asm-lsp --version`.
- The server logs to stderr; in Zed, open it with
  `dev: open language server logs`.
- If diagnostics are missing in a `.S` file, check that its `#include` files
  are found. Lines that depend on a missing header are not checked.
- When reporting a problem, include the version and the log.
