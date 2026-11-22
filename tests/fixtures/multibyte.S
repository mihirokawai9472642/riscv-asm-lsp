# RISC-V テスト用ファイル (日本語コメントを含む)
# Emoji test: 🚀🔥✨ (UTF-16 surrogate pairs)

    .section .text
    .globl multibyte_test
multibyte_test:
    # 日本語コメントの行にある命令

    addi a0, zero, 1    # インライン日本語コメント: 値 1 を設定
    mv a1, a0           # 絵文字テスト 🎯 a1 <- a0

    ret
