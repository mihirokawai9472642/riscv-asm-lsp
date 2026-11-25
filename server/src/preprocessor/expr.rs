use std::collections::HashMap;

use super::MacroDef;
use super::expand::{Token, TokenKind, expand_string_for_expr, tokenize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CondResult {
    True,
    False,
    Uncertain,
}

/// Evaluate a C preprocessor `#if` / `#elif` integer constant expression.
pub fn eval_if_expr(
    raw_expr: &str,
    macros: &HashMap<String, MacroDef>,
    has_unresolved_include: bool,
) -> CondResult {
    let mut uncertain = false;
    let resolved_defined =
        resolve_defined_operators(raw_expr, macros, has_unresolved_include, &mut uncertain);
    let expanded = expand_string_for_expr(
        &resolved_defined,
        macros,
        has_unresolved_include,
        &mut uncertain,
    );
    if has_unresolved_include {
        // Any remaining bare identifier (other than literals) will evaluate to 0 in `#if`;
        // an unresolved `#include` may have defined it as non-zero.
        let tokens = tokenize(&expanded);
        if tokens
            .iter()
            .any(|t| matches!(t.kind, TokenKind::Ident) && !is_uncertain_exempt_ident(&t.text))
        {
            uncertain = true;
        }
    }

    match parse_and_eval(&expanded) {
        Ok(val) => {
            if uncertain {
                CondResult::Uncertain
            } else if val != 0 {
                CondResult::True
            } else {
                CondResult::False
            }
        }
        Err(()) => CondResult::Uncertain,
    }
}

pub fn eval_ifdef(
    ident: &str,
    negate: bool,
    macros: &HashMap<String, MacroDef>,
    has_unresolved_include: bool,
) -> CondResult {
    if !has_unresolved_include || is_certain_macro_state(ident, macros) {
        let present = macros.contains_key(ident);
        if present ^ negate {
            CondResult::True
        } else {
            CondResult::False
        }
    } else {
        CondResult::Uncertain
    }
}

fn is_uncertain_exempt_ident(name: &str) -> bool {
    name.starts_with("__riscv") || name == "__ASSEMBLER__"
}

fn is_certain_macro_state(ident: &str, macros: &HashMap<String, MacroDef>) -> bool {
    match macros.get(ident) {
        Some(def) => def.certain_despite_unresolved || is_uncertain_exempt_ident(ident),
        None => is_uncertain_exempt_ident(ident),
    }
}

/// Replace `defined(IDENT)` and `defined IDENT` before macro expansion.
fn resolve_defined_operators(
    raw: &str,
    macros: &HashMap<String, MacroDef>,
    has_unresolved_include: bool,
    uncertain: &mut bool,
) -> String {
    let tokens = tokenize(raw);
    let mut out = String::with_capacity(raw.len());
    let mut i = 0usize;

    while i < tokens.len() {
        if tokens[i].kind == TokenKind::Ident && tokens[i].text == "defined" {
            let mut j = i + 1;
            while j < tokens.len() && tokens[j].kind == TokenKind::Space {
                j += 1;
            }
            if j < tokens.len() && tokens[j].kind == TokenKind::Ident {
                let name = &tokens[j].text;
                if has_unresolved_include && !is_certain_macro_state(name, macros) {
                    *uncertain = true;
                }
                out.push(if macros.contains_key(name) { '1' } else { '0' });
                i = j + 1;
                continue;
            }
            if j < tokens.len() && tokens[j].kind == TokenKind::Punct && tokens[j].text == "(" {
                let mut k = j + 1;
                while k < tokens.len() && tokens[k].kind == TokenKind::Space {
                    k += 1;
                }
                if k < tokens.len() && tokens[k].kind == TokenKind::Ident {
                    let name = &tokens[k].text;
                    let mut close = k + 1;
                    while close < tokens.len() && tokens[close].kind == TokenKind::Space {
                        close += 1;
                    }
                    if close < tokens.len()
                        && tokens[close].kind == TokenKind::Punct
                        && tokens[close].text == ")"
                    {
                        if has_unresolved_include && !is_certain_macro_state(name, macros) {
                            *uncertain = true;
                        }
                        out.push(if macros.contains_key(name) { '1' } else { '0' });
                        i = close + 1;
                        continue;
                    }
                }
                *uncertain = true;
            } else {
                *uncertain = true;
            }
        }
        out.push_str(&tokens[i].text);
        i += 1;
    }

    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExprTok {
    Int(i128),
    Ident,
    Op(String),
}

fn parse_and_eval(expr: &str) -> Result<i128, ()> {
    let raw_tokens = tokenize(expr);
    let toks = to_expr_tokens(&raw_tokens)?;
    if toks.is_empty() {
        return Err(());
    }
    let mut parser = ExprParser {
        toks: &toks,
        pos: 0,
        depth: 0,
    };
    let val = parser.parse_ternary()?;
    if parser.pos != toks.len() {
        return Err(());
    }
    Ok(val)
}

fn to_expr_tokens(raw: &[Token]) -> Result<Vec<ExprTok>, ()> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < raw.len() {
        let t = &raw[i];
        match t.kind {
            TokenKind::Space => {}
            TokenKind::Ident => out.push(ExprTok::Ident),
            TokenKind::Number => out.push(ExprTok::Int(parse_c_int(&t.text)?)),
            TokenKind::CharLit => out.push(ExprTok::Int(parse_c_char(&t.text)?)),
            TokenKind::StringLit => return Err(()),
            TokenKind::Hash | TokenKind::HashHash => return Err(()),
            TokenKind::Punct => {
                if let Some(next) = raw.get(i + 1)
                    && next.kind == TokenKind::Punct
                {
                    let two = format!("{}{}", t.text, next.text);
                    if matches!(
                        two.as_str(),
                        "<<" | ">>" | "<=" | ">=" | "==" | "!=" | "&&" | "||"
                    ) {
                        out.push(ExprTok::Op(two));
                        i += 2;
                        continue;
                    }
                }
                out.push(ExprTok::Op(t.text.clone()));
            }
        }
        i += 1;
    }
    Ok(out)
}

fn parse_c_int(s: &str) -> Result<i128, ()> {
    let core = s.trim_end_matches(['u', 'U', 'l', 'L']);
    let clean: String = core.chars().filter(|&c| c != '\'').collect();
    if let Some(hex) = clean
        .strip_prefix("0x")
        .or_else(|| clean.strip_prefix("0X"))
    {
        i128::from_str_radix(hex, 16).map_err(|_| ())
    } else if let Some(bin) = clean
        .strip_prefix("0b")
        .or_else(|| clean.strip_prefix("0B"))
    {
        i128::from_str_radix(bin, 2).map_err(|_| ())
    } else if clean.starts_with('0') && clean.len() > 1 {
        i128::from_str_radix(&clean[1..], 8).map_err(|_| ())
    } else {
        clean.parse::<i128>().map_err(|_| ())
    }
}

fn parse_c_char(s: &str) -> Result<i128, ()> {
    let inner = s
        .strip_prefix('\'')
        .and_then(|r| r.strip_suffix('\''))
        .ok_or(())?;
    let mut chars = inner.chars();
    let ch = chars.next().ok_or(())?;
    if ch != '\\' {
        return Ok(ch as i128);
    }
    let esc = chars.next().ok_or(())?;
    let val = match esc {
        'n' => b'\n' as i128,
        'r' => b'\r' as i128,
        't' => b'\t' as i128,
        '\\' => b'\\' as i128,
        '\'' => b'\'' as i128,
        '"' => b'"' as i128,
        'a' => 0x07,
        'b' => 0x08,
        'f' => 0x0c,
        'v' => 0x0b,
        'x' => {
            let hex: String = chars.collect();
            i128::from_str_radix(&hex, 16).map_err(|_| ())?
        }
        c if ('0'..='7').contains(&c) => {
            let mut oct = String::new();
            oct.push(c);
            for d in chars.take(2) {
                if ('0'..='7').contains(&d) {
                    oct.push(d);
                } else {
                    break;
                }
            }
            i128::from_str_radix(&oct, 8).map_err(|_| ())?
        }
        other => other as i128,
    };
    Ok(val)
}

struct ExprParser<'a> {
    toks: &'a [ExprTok],
    pos: usize,
    depth: usize,
}

const MAX_EXPR_DEPTH: usize = 64;

impl<'a> ExprParser<'a> {
    fn peek_op(&self, op: &str) -> bool {
        matches!(self.toks.get(self.pos), Some(ExprTok::Op(s)) if s == op)
    }

    fn consume_op(&mut self, op: &str) -> bool {
        if self.peek_op(op) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn parse_ternary(&mut self) -> Result<i128, ()> {
        let cond = self.parse_or()?;
        if self.consume_op("?") {
            let t_val = self.parse_ternary()?;
            if !self.consume_op(":") {
                return Err(());
            }
            let f_val = self.parse_ternary()?;
            Ok(if cond != 0 { t_val } else { f_val })
        } else {
            Ok(cond)
        }
    }

    fn parse_or(&mut self) -> Result<i128, ()> {
        let mut lhs = self.parse_and()?;
        while self.consume_op("||") {
            let rhs = self.parse_and()?;
            lhs = i128::from(lhs != 0 || rhs != 0);
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<i128, ()> {
        let mut lhs = self.parse_bitor()?;
        while self.consume_op("&&") {
            let rhs = self.parse_bitor()?;
            lhs = i128::from(lhs != 0 && rhs != 0);
        }
        Ok(lhs)
    }

    fn parse_bitor(&mut self) -> Result<i128, ()> {
        let mut lhs = self.parse_bitxor()?;
        while self.consume_op("|") {
            lhs |= self.parse_bitxor()?;
        }
        Ok(lhs)
    }

    fn parse_bitxor(&mut self) -> Result<i128, ()> {
        let mut lhs = self.parse_bitand()?;
        while self.consume_op("^") {
            lhs ^= self.parse_bitand()?;
        }
        Ok(lhs)
    }

    fn parse_bitand(&mut self) -> Result<i128, ()> {
        let mut lhs = self.parse_eq()?;
        while self.consume_op("&") {
            lhs &= self.parse_eq()?;
        }
        Ok(lhs)
    }

    fn parse_eq(&mut self) -> Result<i128, ()> {
        let mut lhs = self.parse_rel()?;
        loop {
            if self.consume_op("==") {
                lhs = i128::from(lhs == self.parse_rel()?);
            } else if self.consume_op("!=") {
                lhs = i128::from(lhs != self.parse_rel()?);
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_rel(&mut self) -> Result<i128, ()> {
        let mut lhs = self.parse_shift()?;
        loop {
            if self.consume_op("<") {
                lhs = i128::from(lhs < self.parse_shift()?);
            } else if self.consume_op("<=") {
                lhs = i128::from(lhs <= self.parse_shift()?);
            } else if self.consume_op(">") {
                lhs = i128::from(lhs > self.parse_shift()?);
            } else if self.consume_op(">=") {
                lhs = i128::from(lhs >= self.parse_shift()?);
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_shift(&mut self) -> Result<i128, ()> {
        let mut lhs = self.parse_add()?;
        loop {
            if self.consume_op("<<") {
                let rhs = self.parse_add()?;
                let sh = u32::try_from(rhs).map_err(|_| ())?;
                lhs = lhs.wrapping_shl(sh);
            } else if self.consume_op(">>") {
                let rhs = self.parse_add()?;
                let sh = u32::try_from(rhs).map_err(|_| ())?;
                lhs = lhs.wrapping_shr(sh);
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_add(&mut self) -> Result<i128, ()> {
        let mut lhs = self.parse_mul()?;
        loop {
            if self.consume_op("+") {
                lhs = lhs.wrapping_add(self.parse_mul()?);
            } else if self.consume_op("-") {
                lhs = lhs.wrapping_sub(self.parse_mul()?);
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> Result<i128, ()> {
        let mut lhs = self.parse_unary()?;
        loop {
            if self.consume_op("*") {
                lhs = lhs.wrapping_mul(self.parse_unary()?);
            } else if self.consume_op("/") {
                let rhs = self.parse_unary()?;
                lhs = lhs.checked_div(rhs).ok_or(())?;
            } else if self.consume_op("%") {
                let rhs = self.parse_unary()?;
                lhs = lhs.checked_rem(rhs).ok_or(())?;
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<i128, ()> {
        if self.depth >= MAX_EXPR_DEPTH {
            return Err(());
        }
        self.depth += 1;
        let res = if self.consume_op("+") {
            self.parse_unary()
        } else if self.consume_op("-") {
            Ok(self.parse_unary()?.wrapping_neg())
        } else if self.consume_op("!") {
            Ok(i128::from(self.parse_unary()? == 0))
        } else if self.consume_op("~") {
            Ok(!self.parse_unary()?)
        } else {
            self.parse_primary()
        };
        self.depth -= 1;
        res
    }

    fn parse_primary(&mut self) -> Result<i128, ()> {
        if self.consume_op("(") {
            let val = self.parse_ternary()?;
            if !self.consume_op(")") {
                return Err(());
            }
            return Ok(val);
        }
        let tok = self.toks.get(self.pos).ok_or(())?;
        self.pos += 1;
        match tok {
            ExprTok::Int(v) => Ok(*v),
            ExprTok::Ident => Ok(0),
            ExprTok::Op(_) => Err(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_obj(body: &str) -> MacroDef {
        MacroDef {
            params: None,
            variadic_param: None,
            body: body.to_owned(),
            certain_despite_unresolved: true,
        }
    }

    #[test]
    fn evaluates_arithmetic_logic_defined_and_div_by_zero_safely() {
        let mut macros = HashMap::new();
        macros.insert("__riscv_xlen".to_owned(), mk_obj("64"));
        macros.insert("FEAT_A".to_owned(), mk_obj("1"));

        assert_eq!(
            eval_if_expr(
                "defined(FEAT_A) && __riscv_xlen == 64 && !defined MISSING",
                &macros,
                false
            ),
            CondResult::True
        );
        assert_eq!(
            eval_if_expr("(1 + 2 * 3) == (1 << 3) - 1 ? 10 : 0", &macros, false),
            CondResult::True
        );
        assert_eq!(
            eval_if_expr("0x10 == 16UL && 010 == 8 && 'A' == 65", &macros, false),
            CondResult::True
        );
        assert_eq!(
            eval_if_expr("__riscv_xlen == 32", &macros, false),
            CondResult::False
        );
        assert_eq!(eval_if_expr("1 / 0", &macros, false), CondResult::Uncertain);
        assert_eq!(
            eval_if_expr("defined(UNKNOWN_HDR_FLAG)", &macros, true),
            CondResult::Uncertain
        );
        assert_eq!(
            eval_if_expr("__riscv_xlen == 64", &macros, true),
            CondResult::True
        );
    }
}
