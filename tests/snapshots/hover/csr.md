CSR · Supervisor Trap Vector · 0x105 · S-mode

```riscv
stvec
```

---

Fields:
- `BASE` [63:2] — Bits [SXLEN-1:2] of the virtual address of the exception vector for any trap taken into S-mode.
- `MODE` [1:0] — Vectoring mode for asynchronous interrupts.

---

Controls where traps jump.
