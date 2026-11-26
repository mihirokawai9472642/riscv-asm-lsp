# Using `riscv-asm-lsp` in Other Editors

`riscv-asm-lsp` communicates over standard LSP stdio and works with any editor
that supports the Language Server Protocol.

## Installing the server binary

Prebuilt static binaries are published on
[GitHub Releases](https://github.com/mihirokawai9472642/riscv-asm-lsp/releases)
for Linux (`x86_64` and `aarch64` musl), macOS (`aarch64` and `x86_64`), and
Windows (`x86_64`). See [README.md](../README.md#building) for archive
checksum verification or building from source (`cargo install --path server`).

The server crate (`server/Cargo.toml`) also carries `[package.metadata.binstall]`
so `cargo-binstall` can install the matching release archive once published on
crates.io (or with `--git https://github.com/mihirokawai9472642/riscv-asm-lsp`):

```bash
cargo binstall riscv-asm-lsp --git https://github.com/mihirokawai9472642/riscv-asm-lsp
```

Make sure `riscv-asm-lsp` is on your `PATH`.

## Neovim

### Neovim 0.11+ (`vim.lsp.config`)

Add to your `init.lua`:

```lua
vim.lsp.config('riscv_asm_lsp', {
  cmd = { 'riscv-asm-lsp' },
  filetypes = { 'asm', 's', 'S' },
  root_markers = {
    'compile_commands.json',
    'compile_flags.txt',
    '.riscv-asm-lsp.yaml',
    '.riscv-asm-lsp.yml',
    '.git',
  },
  -- Optional startup defaults (`initializationOptions`)
  init_options = {
    defaultTarget = 'riscv64-unknown-elf',
    defaultMarch = 'rv64gc',
    diagnostics = {
      enable = true,
      delayMs = 300,
      external = 'off',
    },
  },
})

vim.lsp.enable('riscv_asm_lsp')
```

### `nvim-lspconfig`

```lua
local lspconfig = require('lspconfig')
local configs = require('lspconfig.configs')

if not configs.riscv_asm_lsp then
  configs.riscv_asm_lsp = {
    default_config = {
      cmd = { 'riscv-asm-lsp' },
      filetypes = { 'asm', 's', 'S' },
      root_dir = lspconfig.util.root_pattern(
        'compile_commands.json',
        'compile_flags.txt',
        '.riscv-asm-lsp.yaml',
        '.riscv-asm-lsp.yml',
        '.git'
      ),
      init_options = {
        defaultMarch = 'rv64gc',
        diagnostics = { external = 'off' },
      },
    },
  }
end

lspconfig.riscv_asm_lsp.setup({})
```

## Helix

Add to `~/.config/helix/languages.toml` (Helix uses the `gas` language entry
for GNU assembly `.s` / `.S` files; check `hx --health gas` on your version):

```toml
[language-server.riscv-asm-lsp]
command = "riscv-asm-lsp"

# Optional startup defaults (`initializationOptions`):
# [language-server.riscv-asm-lsp.config]
# defaultTarget = "riscv64-unknown-elf"
# defaultMarch = "rv64gc"
# diagnostics = { enable = true, delayMs = 300, external = "off" }

[[language]]
name = "gas"
file-types = ["s", "S", "sx"]
language-servers = ["riscv-asm-lsp"]
```

## Server configuration

All editors accept the same `initializationOptions` / `settings` JSON keys
(`defaultTarget`, `defaultMarch`, `compileCommandsDirs`, `diagnostics.*`,
`clang`, `llvmMc`) documented in [README.md](../README.md#configuration), as well
as per-project `.riscv-asm-lsp.yaml` / `compile_commands.json` /
`compile_flags.txt`.
