//===-- matint.rs - Immediate materialization ported from LLVM RISCVMatInt --===//
//
// Ported from LLVM Project (`llvmorg-23.1.6`):
//   Source: `llvm/lib/Target/RISCV/MCTargetDesc/RISCVMatInt.cpp`
//   License: Apache License v2.0 with LLVM Exceptions (`Apache-2.0 WITH LLVM-exception`)
//   See `THIRD_PARTY_NOTICES.md` for full license text.
//
//===-------------------------------------------------------------------------===//

use crate::context::MarchSpec;
use crate::data::Xlen;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatIntOpcode {
    Lui,
    Addi,
    Addiw,
    Slli,
    Srli,
    SlliUw,
    AddUw,
    Sh1Add,
    Sh2Add,
    Sh3Add,
    Xori,
    Bseti,
    Bclri,
    Rori,
    Pack,
    PliB,
    PliH,
    PliW,
    PluiH,
    PluiW,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpndKind {
    Imm,
    RegX0,
    RegReg,
    RegImm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatIntInst {
    pub opcode: MatIntOpcode,
    pub imm: i64,
}

impl MatIntInst {
    pub fn mnemonic(self) -> &'static str {
        match self.opcode {
            MatIntOpcode::Lui => "lui",
            MatIntOpcode::Addi => "addi",
            MatIntOpcode::Addiw => "addiw",
            MatIntOpcode::Slli => "slli",
            MatIntOpcode::Srli => "srli",
            MatIntOpcode::SlliUw => "slli.uw",
            MatIntOpcode::AddUw => "add.uw",
            MatIntOpcode::Sh1Add => "sh1add",
            MatIntOpcode::Sh2Add => "sh2add",
            MatIntOpcode::Sh3Add => "sh3add",
            MatIntOpcode::Xori => "xori",
            MatIntOpcode::Bseti => "bseti",
            MatIntOpcode::Bclri => "bclri",
            MatIntOpcode::Rori => "rori",
            MatIntOpcode::Pack => "pack",
            MatIntOpcode::PliB => "pli.b",
            MatIntOpcode::PliH => "pli.h",
            MatIntOpcode::PliW => "pli.w",
            MatIntOpcode::PluiH => "plui.h",
            MatIntOpcode::PluiW => "plui.w",
        }
    }

    pub fn opnd_kind(self) -> OpndKind {
        match self.opcode {
            MatIntOpcode::Lui
            | MatIntOpcode::PliB
            | MatIntOpcode::PliH
            | MatIntOpcode::PliW
            | MatIntOpcode::PluiH
            | MatIntOpcode::PluiW => OpndKind::Imm,
            MatIntOpcode::AddUw => OpndKind::RegX0,
            MatIntOpcode::Sh1Add
            | MatIntOpcode::Sh2Add
            | MatIntOpcode::Sh3Add
            | MatIntOpcode::Pack => OpndKind::RegReg,
            MatIntOpcode::Addi
            | MatIntOpcode::Addiw
            | MatIntOpcode::Xori
            | MatIntOpcode::Slli
            | MatIntOpcode::Srli
            | MatIntOpcode::SlliUw
            | MatIntOpcode::Bseti
            | MatIntOpcode::Bclri
            | MatIntOpcode::Rori => OpndKind::RegImm,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedInst {
    pub mnemonic: &'static str,
    pub operands: Vec<String>,
}

/// Materialize an integer constant for `li rd, imm` using `RISCVMatInt::generateInstSeq`.
pub fn materialize_li(rd: &str, raw_val: i64, march: &MarchSpec) -> Vec<MaterializedInst> {
    let is_rv64 = march.xlen != Xlen::Rv32;
    let val = if is_rv64 {
        raw_val
    } else {
        i64::from(raw_val as i32)
    };
    let seq = generate_inst_seq(val, march);
    let mut src_reg = "zero";
    let mut out = Vec::with_capacity(seq.len());
    for inst in seq {
        let mnem = inst.mnemonic();
        let operands = match inst.opnd_kind() {
            OpndKind::Imm => vec![rd.to_owned(), inst.imm.to_string()],
            OpndKind::RegX0 => vec![rd.to_owned(), src_reg.to_owned(), "zero".to_owned()],
            OpndKind::RegReg => vec![rd.to_owned(), src_reg.to_owned(), src_reg.to_owned()],
            OpndKind::RegImm => vec![rd.to_owned(), src_reg.to_owned(), inst.imm.to_string()],
        };
        out.push(MaterializedInst {
            mnemonic: mnem,
            operands,
        });
        src_reg = rd;
    }
    out
}

#[inline]
fn is_int<const N: u32>(x: i64) -> bool {
    if N >= 64 {
        true
    } else {
        let min = -(1i64 << (N - 1));
        let max = (1i64 << (N - 1)) - 1;
        x >= min && x <= max
    }
}

#[inline]
fn is_uint<const N: u32>(x: i64) -> bool {
    if N >= 64 {
        x >= 0
    } else {
        (x as u64) < (1u64 << N)
    }
}

#[inline]
fn sign_extend32(x: u64) -> i64 {
    i64::from((x as u32) as i32)
}

#[inline]
fn sign_extend12(x: i64) -> i64 {
    let bits = (x as u64) & 0xfff;
    if (bits & 0x800) != 0 {
        (bits | !0xfffu64) as i64
    } else {
        bits as i64
    }
}

#[inline]
fn is_shifted_int<const N: u32, const S: u32>(x: i64) -> bool {
    let lo_mask = (1i64 << S) - 1;
    (x & lo_mask) == 0 && is_int::<N>(x >> S)
}

#[inline]
fn mask_trailing_ones(n: u32) -> u64 {
    if n == 0 {
        0
    } else if n >= 64 {
        u64::MAX
    } else {
        (1u64 << n) - 1
    }
}

#[inline]
fn mask_trailing_zeros(n: u32) -> u64 {
    !mask_trailing_ones(n)
}

/// Port of LLVM `generateInstSeqImpl`.
fn generate_inst_seq_impl(val: i64, march: &MarchSpec, res: &mut Vec<MatIntInst>) {
    let is_rv64 = march.xlen != Xlen::Rv32;
    let has = |ext: &str| march.extensions.contains(ext);

    if has("zbs") && (val as u64).is_power_of_two() && (!is_int::<32>(val) || val == 0x800) {
        res.push(MatIntInst {
            opcode: MatIntOpcode::Bseti,
            imm: i64::from((val as u64).trailing_zeros()),
        });
        return;
    }

    if has("p") && !is_int::<12>(val) && !is_shifted_int::<20, 12>(val) {
        let bit31_to_0 = val as i32;
        let width = if !is_rv64 || i32::try_from(val >> 32).is_ok_and(|hi| hi == bit31_to_0) {
            let bit15_to_0 = bit31_to_0 as i16;
            if i16::try_from(bit31_to_0 >> 16).is_ok_and(|hi| hi == bit15_to_0) {
                16
            } else {
                32
            }
        } else {
            64
        };

        let bit15_to_0 = bit31_to_0 as i16;
        let bit7_to_0 = bit15_to_0 as i8;
        if width == 16 && i16::from(bit7_to_0) == bit15_to_0 && (bit15_to_0 >> 8) as i8 == bit7_to_0
        {
            res.push(MatIntInst {
                opcode: MatIntOpcode::PliB,
                imm: i64::from(bit7_to_0),
            });
            return;
        }
        if width == 16 && is_int::<10>(i64::from(bit15_to_0)) {
            res.push(MatIntInst {
                opcode: MatIntOpcode::PliH,
                imm: i64::from(bit15_to_0),
            });
            return;
        }
        if width == 16 && is_shifted_int::<10, 6>(i64::from(bit15_to_0)) {
            res.push(MatIntInst {
                opcode: MatIntOpcode::PluiH,
                imm: i64::from(bit15_to_0 >> 6),
            });
            return;
        }
        if width == 32 && is_int::<10>(i64::from(bit31_to_0)) {
            res.push(MatIntInst {
                opcode: MatIntOpcode::PliW,
                imm: i64::from(bit31_to_0),
            });
            return;
        }
        if width == 32 && is_shifted_int::<10, 22>(i64::from(bit31_to_0)) {
            res.push(MatIntInst {
                opcode: MatIntOpcode::PluiW,
                imm: i64::from(bit31_to_0 >> 22),
            });
            return;
        }
    }

    if is_int::<32>(val) {
        let hi20 = (val.wrapping_add(0x800) >> 12) & 0xFFFFF;
        let lo12 = sign_extend12(val);

        if hi20 != 0 {
            res.push(MatIntInst {
                opcode: MatIntOpcode::Lui,
                imm: hi20,
            });
        }

        if lo12 != 0 || hi20 == 0 {
            let mut addi_opc = MatIntOpcode::Addi;
            if is_rv64 && hi20 != 0 {
                let lui_res = sign_extend32((hi20 as u64) << 12);
                if !is_int::<32>(lui_res.wrapping_add(lo12)) {
                    addi_opc = MatIntOpcode::Addiw;
                }
            }
            res.push(MatIntInst {
                opcode: addi_opc,
                imm: lo12,
            });
        }
        return;
    }

    if !is_rv64 {
        return;
    }

    let lo12 = sign_extend12(val);
    let mut rem = ((val as u64).wrapping_sub(lo12 as u64)) as i64;

    let mut shift_amount = 0u32;
    let mut shift_opc = MatIntOpcode::Slli;

    if !is_int::<32>(rem) {
        shift_amount = (rem as u64).trailing_zeros();
        rem >>= shift_amount;

        if shift_amount > 12 && !is_int::<12>(rem) {
            let shifted12 = ((rem as u64) << 12) as i64;
            if is_int::<32>(shifted12) {
                shift_amount -= 12;
                rem = shifted12;
            } else if is_uint::<32>(shifted12) && has("zba") {
                shift_amount -= 12;
                rem = sign_extend32(shifted12 as u64);
                shift_opc = MatIntOpcode::SlliUw;
            }
        }

        if is_uint::<32>(rem) && !is_int::<32>(rem) && has("zba") {
            rem = sign_extend32(rem as u64);
            shift_opc = MatIntOpcode::SlliUw;
        }
    }

    generate_inst_seq_impl(rem, march, res);

    if shift_amount != 0 {
        res.push(MatIntInst {
            opcode: shift_opc,
            imm: i64::from(shift_amount),
        });
    }

    if lo12 != 0 {
        res.push(MatIntInst {
            opcode: MatIntOpcode::Addi,
            imm: lo12,
        });
    }
}

fn extract_rotate_info(val: i64) -> u32 {
    let uval = val as u64;
    let leading_ones = uval.leading_ones();
    let trailing_ones = uval.trailing_ones();
    if trailing_ones > 0 && trailing_ones < 64 && (leading_ones + trailing_ones) > (64 - 12) {
        return 64 - trailing_ones;
    }

    let hi32 = (uval >> 32) as u32;
    let lo32 = uval as u32;
    let upper_trailing_ones = hi32.trailing_ones();
    let lower_leading_ones = lo32.leading_ones();
    if upper_trailing_ones < 32 && (upper_trailing_ones + lower_leading_ones) > (64 - 12) {
        return 32 - upper_trailing_ones;
    }

    0
}

fn generate_inst_seq_leading_zeros(val: i64, march: &MarchSpec, res: &mut Vec<MatIntInst>) {
    let uval = val as u64;
    let leading_zeros = uval.leading_zeros();
    if leading_zeros >= 64 {
        return;
    }

    let shifted_ones = ((uval << leading_zeros) | mask_trailing_ones(leading_zeros)) as i64;
    let mut tmp = Vec::new();
    generate_inst_seq_impl(shifted_ones, march, &mut tmp);
    if (tmp.len() + 1) < res.len() || (res.is_empty() && tmp.len() < 8) {
        tmp.push(MatIntInst {
            opcode: MatIntOpcode::Srli,
            imm: i64::from(leading_zeros),
        });
        *res = tmp;
    }

    let shifted_zeros = ((uval << leading_zeros) & mask_trailing_zeros(leading_zeros)) as i64;
    let mut tmp2 = Vec::new();
    generate_inst_seq_impl(shifted_zeros, march, &mut tmp2);
    if (tmp2.len() + 1) < res.len() || (res.is_empty() && tmp2.len() < 8) {
        tmp2.push(MatIntInst {
            opcode: MatIntOpcode::Srli,
            imm: i64::from(leading_zeros),
        });
        *res = tmp2;
    }

    if leading_zeros == 32 && march.extensions.contains("zba") {
        let leading_ones_val = sign_extend32(uval);
        let mut tmp3 = Vec::new();
        generate_inst_seq_impl(leading_ones_val, march, &mut tmp3);
        if (tmp3.len() + 1) < res.len() || (res.is_empty() && tmp3.len() < 8) {
            tmp3.push(MatIntInst {
                opcode: MatIntOpcode::AddUw,
                imm: 0,
            });
            *res = tmp3;
        }
    }
}

pub fn generate_inst_seq(val: i64, march: &MarchSpec) -> Vec<MatIntInst> {
    let has = |ext: &str| march.extensions.contains(ext);
    let mut res = Vec::new();
    generate_inst_seq_impl(val, march, &mut res);

    if (val & 0xfff) != 0 && (val & 1) == 0 && res.len() >= 2 {
        let trailing_zeros = (val as u64).trailing_zeros();
        let shifted_val = val >> trailing_zeros;
        let is_shifted_compressible = is_int::<6>(shifted_val);
        let mut tmp = Vec::new();
        generate_inst_seq_impl(shifted_val, march, &mut tmp);
        if (tmp.len() + 1) < res.len() || is_shifted_compressible {
            tmp.push(MatIntInst {
                opcode: MatIntOpcode::Slli,
                imm: i64::from(trailing_zeros),
            });
            res = tmp;
        }
    }

    if res.len() <= 2 {
        return res;
    }

    if march.xlen == Xlen::Rv32 {
        return res;
    }

    if (val & 0xfff) != 0 && (val & 0x1800) == 0x1000 {
        let imm12 = -(0x800 - (val & 0xfff));
        let adjusted_val = val.wrapping_sub(imm12);
        let mut tmp = Vec::new();
        generate_inst_seq_impl(adjusted_val, march, &mut tmp);
        if (tmp.len() + 1) < res.len() {
            tmp.push(MatIntInst {
                opcode: MatIntOpcode::Addi,
                imm: imm12,
            });
            res = tmp;
        }
    }

    if val > 0 && res.len() > 2 {
        generate_inst_seq_leading_zeros(val, march, &mut res);
    }

    if val < 0 && res.len() > 3 {
        let inverted = (!(val as u64)) as i64;
        if inverted > 0 {
            let mut tmp = Vec::new();
            generate_inst_seq_leading_zeros(inverted, march, &mut tmp);
            if !tmp.is_empty() && (tmp.len() + 1) < res.len() {
                tmp.push(MatIntInst {
                    opcode: MatIntOpcode::Xori,
                    imm: -1,
                });
                res = tmp;
            }
        }
    }

    if res.len() > 2 && (has("zbkb") || has("p")) {
        let lo_val = sign_extend32(val as u64);
        let hi_val = sign_extend32((val as u64) >> 32);
        if lo_val == hi_val {
            let mut tmp = Vec::new();
            generate_inst_seq_impl(lo_val, march, &mut tmp);
            if (tmp.len() + 1) < res.len() {
                tmp.push(MatIntInst {
                    opcode: MatIntOpcode::Pack,
                    imm: 0,
                });
                res = tmp;
            }
        }
    }

    if res.len() > 2 && has("zbs") {
        let lo = (val as u64) & 0x7fff_ffff;
        let hi = (val as u64) ^ lo;
        if hi != 0 {
            let mut tmp = Vec::new();
            if lo != 0 {
                generate_inst_seq_impl(lo as i64, march, &mut tmp);
            }
            if tmp.len() + (hi.count_ones() as usize) < res.len() {
                let mut rem_hi = hi;
                while rem_hi != 0 {
                    tmp.push(MatIntInst {
                        opcode: MatIntOpcode::Bseti,
                        imm: i64::from(rem_hi.trailing_zeros()),
                    });
                    rem_hi &= rem_hi - 1;
                }
                res = tmp;
            }
        }
        if res.len() >= 2
            && res[0].opcode == MatIntOpcode::Addi
            && res[0].imm == 1
            && res[1].opcode == MatIntOpcode::Slli
        {
            res.remove(0);
            res[0].opcode = MatIntOpcode::Bseti;
        }
    }

    if res.len() > 2 && has("zbs") {
        let lo = (val as u64) | 0xffff_ffff_8000_0000;
        let hi = (val as u64) ^ lo;
        if hi != 0 {
            let mut tmp = Vec::new();
            generate_inst_seq_impl(lo as i64, march, &mut tmp);
            if tmp.len() + (hi.count_ones() as usize) < res.len() {
                let mut rem_hi = hi;
                while rem_hi != 0 {
                    tmp.push(MatIntInst {
                        opcode: MatIntOpcode::Bclri,
                        imm: i64::from(rem_hi.trailing_zeros()),
                    });
                    rem_hi &= rem_hi - 1;
                }
                res = tmp;
            }
        }
    }

    if res.len() > 2 && has("zba") {
        let (div, opc) = if (val % 3) == 0 && is_int::<32>(val / 3) {
            (3, MatIntOpcode::Sh1Add)
        } else if (val % 5) == 0 && is_int::<32>(val / 5) {
            (5, MatIntOpcode::Sh2Add)
        } else if (val % 9) == 0 && is_int::<32>(val / 9) {
            (9, MatIntOpcode::Sh3Add)
        } else {
            (0, MatIntOpcode::Sh1Add)
        };
        if div > 0 {
            let mut tmp = Vec::new();
            generate_inst_seq_impl(val / div, march, &mut tmp);
            if (tmp.len() + 1) < res.len() {
                tmp.push(MatIntInst {
                    opcode: opc,
                    imm: 0,
                });
                res = tmp;
            }
        } else {
            let hi52 = ((val as u64).wrapping_add(0x800) & !0xfffu64) as i64;
            let lo12 = sign_extend12(val);
            let (hdiv, hopc) = if (hi52 % 3) == 0 && is_int::<32>(hi52 / 3) {
                (3, MatIntOpcode::Sh1Add)
            } else if (hi52 % 5) == 0 && is_int::<32>(hi52 / 5) {
                (5, MatIntOpcode::Sh2Add)
            } else if (hi52 % 9) == 0 && is_int::<32>(hi52 / 9) {
                (9, MatIntOpcode::Sh3Add)
            } else {
                (0, MatIntOpcode::Sh1Add)
            };
            if hdiv > 0 && lo12 != 0 {
                let mut tmp = Vec::new();
                generate_inst_seq_impl(hi52 / hdiv, march, &mut tmp);
                if (tmp.len() + 2) < res.len() {
                    tmp.push(MatIntInst {
                        opcode: hopc,
                        imm: 0,
                    });
                    tmp.push(MatIntInst {
                        opcode: MatIntOpcode::Addi,
                        imm: lo12,
                    });
                    res = tmp;
                }
            }
        }
    }

    if res.len() > 2 && has("zbb") {
        let rotate = extract_rotate_info(val);
        if rotate > 0 {
            let neg_imm12 = ((val as u64).rotate_left(rotate)) as i64;
            if is_int::<12>(neg_imm12) {
                res = vec![
                    MatIntInst {
                        opcode: MatIntOpcode::Addi,
                        imm: neg_imm12,
                    },
                    MatIntInst {
                        opcode: MatIntOpcode::Rori,
                        imm: i64::from(rotate),
                    },
                ];
            }
        }
    }

    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materializes_common_and_extended_constants() {
        let rv64gc = MarchSpec::parse("rv64gc");
        assert_eq!(
            generate_inst_seq(0, &rv64gc),
            vec![MatIntInst {
                opcode: MatIntOpcode::Addi,
                imm: 0
            }]
        );
        assert_eq!(
            generate_inst_seq(0x1234_5678, &rv64gc),
            vec![
                MatIntInst {
                    opcode: MatIntOpcode::Lui,
                    imm: 74565
                },
                MatIntInst {
                    opcode: MatIntOpcode::Addi,
                    imm: 1656
                }
            ]
        );
        assert_eq!(
            generate_inst_seq(0x7fff_ffff, &rv64gc),
            vec![
                MatIntInst {
                    opcode: MatIntOpcode::Lui,
                    imm: 524_288
                },
                MatIntInst {
                    opcode: MatIntOpcode::Addiw,
                    imm: -1
                }
            ]
        );
        let rv64zbs = MarchSpec::parse("rv64gc_zbs");
        assert_eq!(
            generate_inst_seq(1i64 << 40, &rv64zbs),
            vec![MatIntInst {
                opcode: MatIntOpcode::Bseti,
                imm: 40
            }]
        );
        let rv64zbb = MarchSpec::parse("rv64gc_zbb");
        assert_eq!(
            generate_inst_seq(!(1i64 << 40), &rv64zbb),
            vec![
                MatIntInst {
                    opcode: MatIntOpcode::Addi,
                    imm: -2
                },
                MatIntInst {
                    opcode: MatIntOpcode::Rori,
                    imm: 24
                }
            ]
        );
    }
}
