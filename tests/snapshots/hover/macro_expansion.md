Pseudo-instruction · `I` · RV32/RV64

```riscv
la dst, src
la rd, imm
```

---

Operands: `dst`, `rd` integer reg (x0–x31); `src` symbol or relocation; `imm` XLEN immediate or symbol

This line:
```riscv
auipc sp, %pcrel_hi(_stack_top)  # (relocated)
addi sp, sp, %pcrel_lo(.Lpcrel_hi0)  # (relocated)
```

---

Load address — With `.option nopic` (Default) (expands to `auipc rd, symbol[31:12] addi rd, rd, symbol[11:0]`)
