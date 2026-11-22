# File with deliberate assembly errors for diagnostic testing
    .section .text
    .globl bad_func
bad_func:
    mvi a0, 1
    addi a0, zero, 99999999
    # 日本語コメント後のエラーテスト
    add x99, a0, a1
    ret
