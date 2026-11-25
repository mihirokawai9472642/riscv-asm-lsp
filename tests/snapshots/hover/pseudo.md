Pseudo-instruction · `I` · RV32/RV64

```riscv
la dst, src
la rd, imm
```

---

Operands: `dst`, `rd` integer reg (x0–x31); `src` symbol or relocation; `imm` XLEN immediate or symbol

---

Load address — With `.option nopic` (Default) (expands to `auipc rd, symbol[31:12] addi rd, rd, symbol[11:0]`).
