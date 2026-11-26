# Configuration

## Settings

The server reads `initializationOptions` at startup. Settings sent later
through `workspace/didChangeConfiguration` are merged on top of them, key by
key, and apply without a restart. In Zed these are
`lsp.riscv-asm-lsp.initialization_options` and `lsp.riscv-asm-lsp.settings`.

| Option | Default | Meaning |
| --- | --- | --- |
| `defaultTarget` | `"riscv64-unknown-elf"` | Target for files without a compilation database entry |
| `defaultMarch` | `"rv64gc"` | `-march` for files without an entry |
| `compileCommandsDirs` | `["", "build"]` | Directories, relative to the workspace root, searched for `compile_commands.json` |
| `diagnostics.enable` | `true` | Enable diagnostics |
| `diagnostics.delayMs` | `300` | Pause after typing before diagnostics run |
| `diagnostics.external` | `"off"` | Use `clang` / `llvm-mc`: `"off"`, `"auto"` or `"on"` |
| `diagnostics.suppress` | `[]` | Built-in diagnostic codes to hide, for example `["gnu-only-directive"]` |
| `clang` | found automatically | Path to `clang` |
| `llvmMc` | found automatically | Path to `llvm-mc` |

Example for Zed (`.zed/settings.json`):

```json
{
  "lsp": {
    "riscv-asm-lsp": {
      "settings": {
        "defaultMarch": "rv64gc",
        "diagnostics": { "delayMs": 300 }
      }
    }
  }
}
```

## Compilation database

For each file, the server takes the target, `-march`, `-mabi`, include paths,
macro definitions and the language (`-x`) from `compile_commands.json`, like
clangd. A file without an entry borrows the flags of the nearest file that has
one. Without a database, `compile_flags.txt` or the `defaultTarget` and
`defaultMarch` settings apply.

Only assembler and preprocessor flags are used, and the compiler named in the
database is never run.

`.S`, `.sx` and header files are preprocessed; `.s` and `.inc` files are not.
A `-x assembler-with-cpp` or `-x assembler` flag in the database overrides
this. `.option` and `.attribute arch` directives change the active extensions
from that line on.

## Project file

A `.riscv-asm-lsp.yaml` (or `.yml`) file applies to the files below it, up to
the workspace root:

```yaml
CompileFlags:
  Add: ["-march=rv32imac", "-Iinclude"]
  Remove: ["-march=*"]
Diagnostics:
  Suppress: ["gnu-only-directive"]
```

`Remove` takes exact flags or prefixes ending in `*`. `Add` accepts the same
flags as the compilation database. Tool paths cannot be set here.

## External tools

When `diagnostics.external` is `"auto"` or `"on"`, the server looks for `clang`
and `llvm-mc` unless their paths are set:

1. `clang` / `llvm-mc` on `PATH`;
2. the highest versioned `clang-<N>` / `llvm-mc-<N>` on `PATH`;
3. standard install directories, newest first: `/usr/lib/llvm-*/bin` on Linux,
   Homebrew `llvm` and `llvm@*` on macOS, `C:\Program Files\LLVM\bin` on
   Windows.

Tools without the RISC-V target, such as Apple's `clang`, are skipped. If no
tool is found, the server keeps using its built-in engine and logs the problem
once. A configured path that does not work produces one warning.
