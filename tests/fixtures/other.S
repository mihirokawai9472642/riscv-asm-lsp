    .section .text
    .globl kernel_main
kernel_main:
    addi sp, sp, -16
    sd ra, 8(sp)
    la a0, .Llocal_helper
    li a1, 42
    ld ra, 8(sp)
    addi sp, sp, 16
    ret

.Llocal_helper:
    nop
    ret
