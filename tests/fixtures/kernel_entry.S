#include "regs.h"

.equ KERNEL_BASE, 0x80200000
.equ TRAP_FLAG,   1

.macro SAVE_REG reg, offset
    sd \reg, (\offset * 8)(sp)
.endm

    .section .text.init
    .globl _start
_start:
    csrr a0, mhartid
    bnez a0, 1f
    la sp, _stack_top
    li t0, MSTATUS_MIE
    csrw stvec, t0
    mv a1, zero
    SAVE_REG ra, 0
    call kernel_main
1:
    wfi
    j 1b

    .globl trap_vector
    .balign 4
trap_vector:
    csrr t0, scause
    addi sp, sp, -16
    sd ra, 8(sp)
    ld ra, 8(sp)
    addi sp, sp, 16
    sret

.Llocal_helper:
    ret

    .section .bss
    .balign 16
_stack_bottom:
    .space STACK_SIZE
_stack_top:
