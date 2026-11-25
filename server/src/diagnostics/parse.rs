use std::collections::HashSet;
use std::path::{Path, PathBuf};

use lsp_types::{
    Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, Location, Position, Range, Uri,
};

use crate::context::{FileContext, display_path_for_client, path_to_uri};
use crate::position::{LineIndex, clamp_to_char_boundary, word_range_at};
use crate::preprocessor::parse_cpp_include_line;

use super::hints::{
    CppDefineLine, MacroHintQuery, build_macro_def_hints, collect_cpp_define_lines,
    extract_suggestions, insert_dedup_key,
};
use super::{MacroDefHint, StoredDiagnostic};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagKind {
    Error,
    Warning,
    Note,
}

#[derive(Debug, Clone)]
struct ClangDiagLine<'a> {
    file: &'a str,
    line: u32,
    col: u32,
    kind: DiagKind,
    message: &'a str,
}

struct ParseContext<'a> {
    line_index: LineIndex,
    text: &'a str,
    preprocessed: bool,
    defined_macros: Vec<CppDefineLine>,
    self_uri: Option<Uri>,
    doc_path: Option<&'a Path>,
    file_ctx: Option<&'a FileContext>,
}

struct DiagnosticBody {
    range: Range,
    message: String,
    suggestions: Vec<String>,
    related: Option<Vec<DiagnosticRelatedInformation>>,
    macro_def_hints: Vec<MacroDefHint>,
}

struct IncludedDiagReq<'a> {
    pending_includes: &'a [(String, u32)],
    err_file: &'a str,
    err_line: u32,
    err_col: u32,
    err_msg: &'a str,
}

pub fn parse_clang_output(text: &str, stderr: &str) -> Vec<StoredDiagnostic> {
    parse_clang_output_with_ctx(text, stderr, None, None, None)
}

pub fn parse_clang_output_with_ctx(
    text: &str,
    stderr: &str,
    doc_path: Option<&Path>,
    doc_uri: Option<&Uri>,
    file_ctx: Option<&FileContext>,
) -> Vec<StoredDiagnostic> {
    let preprocessed = file_ctx.is_none_or(|c| c.preprocessed);
    let ctx = ParseContext {
        line_index: LineIndex::new(text),
        text,
        preprocessed,
        defined_macros: if preprocessed {
            collect_cpp_define_lines(text)
        } else {
            Vec::new()
        },
        self_uri: doc_uri.cloned().or_else(|| doc_path.and_then(path_to_uri)),
        doc_path,
        file_ctx,
    };

    let lines: Vec<&str> = stderr.lines().collect();
    let mut out = Vec::new();
    let mut seen: HashSet<(u32, u32, u32, u32, String)> = HashSet::new();
    let mut pending_includes: Vec<(String, u32)> = Vec::new();

    let mut i = 0;
    while i < lines.len() {
        if let Some(inc) = parse_include_header(lines[i]) {
            pending_includes.push(inc);
            i += 1;
            continue;
        }
        let Some(diag) = parse_clang_diag_line(lines[i]) else {
            i += 1;
            continue;
        };
        if diag.kind == DiagKind::Note {
            i += 1;
            continue;
        }
        let severity = match diag.kind {
            DiagKind::Error => DiagnosticSeverity::ERROR,
            DiagKind::Warning => DiagnosticSeverity::WARNING,
            DiagKind::Note => unreachable!(),
        };

        let (macro_notes, next_i) = collect_macro_notes(&lines, i + 1);
        let body = build_diagnostic_body(&ctx, &diag, &macro_notes, &pending_includes);
        pending_includes.clear();
        i = next_i;

        if !insert_dedup_key(&mut seen, body.range, &body.message) {
            continue;
        }

        out.push(StoredDiagnostic {
            diagnostic: Diagnostic {
                range: body.range,
                severity: Some(severity),
                source: Some("clang".to_owned()),
                message: body.message,
                related_information: body.related,
                ..Diagnostic::default()
            },
            suggestions: body.suggestions,
            macro_def_hints: body.macro_def_hints,
        });
    }

    out
}

fn collect_macro_notes<'a>(lines: &[&'a str], start: usize) -> (Vec<ClangDiagLine<'a>>, usize) {
    // Collect following `note: while in macro instantiation` lines up to the next diagnostic.
    let mut macro_notes = Vec::new();
    let mut j = start;
    while j < lines.len() {
        if parse_include_header(lines[j]).is_some() {
            break;
        }
        if let Some(next_diag) = parse_clang_diag_line(lines[j]) {
            if next_diag.kind == DiagKind::Note {
                if next_diag.message.contains("while in macro instantiation") {
                    macro_notes.push(next_diag);
                }
                j += 1;
                continue;
            }
            break;
        }
        j += 1;
    }
    (macro_notes, j)
}

fn build_diagnostic_body(
    ctx: &ParseContext<'_>,
    diag: &ClangDiagLine<'_>,
    macro_notes: &[ClangDiagLine<'_>],
    pending_includes: &[(String, u32)],
) -> DiagnosticBody {
    if diag.file == "<stdin>" {
        let range = word_range_for_clang(&ctx.line_index, ctx.text, diag.line, diag.col);
        let message = diag.message.to_owned();
        let suggestions = extract_suggestions(&message);
        let (related, macro_def_hints) = if ctx.preprocessed && !ctx.defined_macros.is_empty() {
            build_macro_def_hints(&MacroHintQuery {
                line_index: &ctx.line_index,
                text: ctx.text,
                diag_line: range.start.line,
                diag_col_1based: diag.col,
                diag_message: &message,
                defined_macros: &ctx.defined_macros,
                doc_uri: ctx.self_uri.as_ref(),
                doc_path: ctx.doc_path,
            })
        } else {
            (None, Vec::new())
        };
        return DiagnosticBody {
            range,
            message,
            suggestions,
            related,
            macro_def_hints,
        };
    }

    if diag.file == "<instantiation>" {
        let message = format!("{} (in macro instantiation)", diag.message);
        if let Some(stdin_note) = macro_notes.iter().rfind(|n| n.file == "<stdin>") {
            let range =
                word_range_for_clang(&ctx.line_index, ctx.text, stdin_note.line, stdin_note.col);
            return DiagnosticBody {
                range,
                message,
                suggestions: Vec::new(),
                related: None,
                macro_def_hints: Vec::new(),
            };
        }
        if let Some(ext_note) = macro_notes
            .iter()
            .rfind(|n| n.file != "<instantiation>" && !is_clang_preprocessed_temp(n.file))
        {
            return build_included_file_diagnostic(
                ctx,
                &IncludedDiagReq {
                    pending_includes,
                    err_file: ext_note.file,
                    err_line: ext_note.line,
                    err_col: ext_note.col,
                    err_msg: &message,
                },
            );
        }
        let pos = Position::new(0, 0);
        return DiagnosticBody {
            range: Range::new(pos, pos),
            message,
            suggestions: Vec::new(),
            related: None,
            macro_def_hints: Vec::new(),
        };
    }

    build_included_file_diagnostic(
        ctx,
        &IncludedDiagReq {
            pending_includes,
            err_file: diag.file,
            err_line: diag.line,
            err_col: diag.col,
            err_msg: diag.message,
        },
    )
}

fn build_included_file_diagnostic(
    ctx: &ParseContext<'_>,
    req: &IncludedDiagReq<'_>,
) -> DiagnosticBody {
    let target_line = resolve_include_line_for_error(
        ctx.text,
        req.pending_includes,
        req.err_file,
        ctx.preprocessed,
    );
    let range = trimmed_line_range(&ctx.line_index, ctx.text, target_line);
    let display_name = Path::new(req.err_file)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(req.err_file);
    let message = format!(
        "in included file `{display_name}:{}:{}`: {}",
        req.err_line, req.err_col, req.err_msg
    );
    let related = resolve_related_location(
        req.err_file,
        req.err_line,
        req.err_col,
        ctx.doc_path,
        ctx.file_ctx,
    )
    .map(|location| {
        vec![DiagnosticRelatedInformation {
            location,
            message: req.err_msg.to_owned(),
        }]
    });
    DiagnosticBody {
        range,
        message,
        suggestions: Vec::new(),
        related,
        macro_def_hints: Vec::new(),
    }
}

fn word_range_for_clang(
    line_index: &LineIndex,
    text: &str,
    line_1based: u32,
    col_1based: u32,
) -> Range {
    let offset = line_index.clang_pos_to_offset(text, line_1based, col_1based);
    if let Some((ds, de)) = delimited_token_range_at(text, line_index, line_1based, offset) {
        return line_index.byte_span_to_range(text, ds, de);
    }
    let (ws, we) = word_range_at(text, offset);
    if ws < we {
        line_index.byte_span_to_range(text, ws, we)
    } else {
        let pos = line_index.offset_to_position(text, offset);
        Range::new(pos, pos)
    }
}

fn delimited_token_range_at(
    text: &str,
    line_index: &LineIndex,
    line_1based: u32,
    offset: usize,
) -> Option<(usize, usize)> {
    let line_idx = line_1based.saturating_sub(1) as usize;
    let (_, content_end, _) = line_index.line_bounds(text, line_idx);
    if offset >= content_end {
        return None;
    }
    let bytes = text.as_bytes();
    let opener = *bytes.get(offset)?;
    let closer = match opener {
        b'"' => b'"',
        b'<' => b'>',
        _ => return None,
    };

    let mut i = offset + 1;
    let mut end = content_end;
    while i < content_end {
        if opener == b'"' && bytes[i] == b'\\' {
            i = (i + 2).min(content_end);
            continue;
        }
        if bytes[i] == closer {
            end = i + 1;
            break;
        }
        i += 1;
    }
    let end = clamp_to_char_boundary(text, end);
    (offset < end).then_some((offset, end))
}

pub(super) fn trimmed_line_range(line_index: &LineIndex, text: &str, line_0based: usize) -> Range {
    let (ls, ce, _) = line_index.line_bounds(text, line_0based);
    let line_str = text.get(ls..ce).unwrap_or("");
    let lead_ws = line_str.len() - line_str.trim_start().len();
    let trail_ws = line_str.len() - line_str.trim_end().len();
    let start = ls + lead_ws;
    let end = ce.saturating_sub(trail_ws).max(start);
    line_index.byte_span_to_range(text, start, end)
}

fn parse_include_header(line: &str) -> Option<(String, u32)> {
    let rest = line
        .strip_prefix("In file included from ")
        .or_else(|| line.strip_prefix("Included from "))?;
    let trimmed = rest.trim_end_matches([':', ',', ' ', '\t']);
    let (file, line_str) = trimmed.rsplit_once(':')?;
    let line_1 = line_str.trim().parse::<u32>().ok()?;
    Some((file.to_owned(), line_1))
}

fn parse_clang_diag_line(line: &str) -> Option<ClangDiagLine<'_>> {
    let markers = [
        (": fatal error: ", DiagKind::Error),
        (": error: ", DiagKind::Error),
        (": warning: ", DiagKind::Warning),
        (": note: ", DiagKind::Note),
    ];
    let mut best: Option<(usize, usize, DiagKind)> = None;
    for &(pat, kind) in &markers {
        if let Some(idx) = line.find(pat)
            && best.is_none_or(|(b_idx, _, _)| idx < b_idx)
        {
            best = Some((idx, pat.len(), kind));
        }
    }
    let (marker_idx, marker_len, kind) = best?;
    let loc = &line[..marker_idx];
    let message = line[marker_idx + marker_len..].trim();
    let (file_and_line, col_str) = loc.rsplit_once(':')?;
    let (file, line_str) = file_and_line.rsplit_once(':')?;
    let line_1 = line_str.trim().parse::<u32>().ok()?;
    let col_1 = col_str.trim().parse::<u32>().ok()?;
    Some(ClangDiagLine {
        file,
        line: line_1,
        col: col_1,
        kind,
        message,
    })
}

fn is_clang_preprocessed_temp(path: &str) -> bool {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("--") && n.ends_with(".s"))
}

fn resolve_include_line_for_error(
    text: &str,
    pending_includes: &[(String, u32)],
    err_file: &str,
    preprocessed: bool,
) -> usize {
    if let Some((_, line_1)) = pending_includes.iter().rev().find(|(f, _)| f == "<stdin>") {
        return line_1.saturating_sub(1) as usize;
    }

    // With `-x assembler-with-cpp`, `Included from` starts from clang's temp preprocessed file
    // (`/tmp/--XXXXXX.s`), so find the outermost real included file and match it in source.
    let target_file = pending_includes
        .iter()
        .find(|(f, _)| f != "<stdin>" && !is_clang_preprocessed_temp(f))
        .map(|(f, _)| f.as_str())
        .unwrap_or(err_file);

    find_include_line_in_source(text, target_file, preprocessed).unwrap_or(0)
}

fn find_include_line_in_source(text: &str, target_file: &str, preprocessed: bool) -> Option<usize> {
    let target_norm = target_file.replace('\\', "/");
    let target_name = Path::new(&target_norm)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&target_norm);

    let mut fallback = None;
    let mut in_block = false;
    for (idx, line) in text.lines().enumerate() {
        let started_in_block = in_block;
        let stripped = crate::line::strip_comments_with_state(line, &mut in_block, preprocessed);
        if started_in_block {
            continue;
        }
        let Some(inc_target) = extract_include_target(&stripped, preprocessed) else {
            continue;
        };
        let inc_norm = inc_target.replace('\\', "/");
        if paths_match(&target_norm, &inc_norm) {
            return Some(idx);
        }
        let inc_name = Path::new(&inc_norm)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&inc_norm);
        let name_eq = if cfg!(windows) {
            inc_name.eq_ignore_ascii_case(target_name)
        } else {
            inc_name == target_name
        };
        if fallback.is_none() && name_eq {
            fallback = Some(idx);
        }
    }
    fallback
}

fn paths_match(target_norm: &str, inc_norm: &str) -> bool {
    if cfg!(windows) {
        let t = target_norm.to_lowercase();
        let i = inc_norm.to_lowercase();
        t == i || t.ends_with(&format!("/{i}"))
    } else {
        target_norm == inc_norm || target_norm.ends_with(&format!("/{inc_norm}"))
    }
}

fn extract_include_target(line: &str, preprocessed: bool) -> Option<String> {
    let trimmed = line.trim();
    if let Some(after) = trimmed.strip_prefix(".include")
        && after.chars().next().is_some_and(char::is_whitespace)
    {
        let inner = after.trim_start().strip_prefix('"')?;
        let end = inner.find('"')?;
        return Some(inner[..end].to_owned());
    }
    if preprocessed {
        return parse_cpp_include_line(line).map(|(target, _)| target);
    }
    None
}

fn resolve_related_location(
    err_file: &str,
    err_line: u32,
    err_col: u32,
    doc_path: Option<&Path>,
    file_ctx: Option<&FileContext>,
) -> Option<Location> {
    if err_file.starts_with('<') && err_file.ends_with('>') {
        return None;
    }
    let raw = Path::new(err_file);
    let resolved: PathBuf = if raw.is_absolute() {
        raw.to_path_buf()
    } else if let Some(wd) = file_ctx.and_then(|c| c.working_dir.as_deref()) {
        wd.join(raw)
    } else {
        let dir = doc_path.and_then(|p| p.parent())?;
        dir.join(raw)
    };
    let display = if let Some(ctx) = file_ctx {
        display_path_for_client(
            &resolved,
            ctx.client_root.as_deref(),
            ctx.canon_root.as_deref(),
        )
    } else {
        resolved.clone()
    };
    let uri = path_to_uri(&display)?;
    let pos = if std::fs::metadata(&resolved)
        .is_ok_and(|m| m.is_file() && m.len() <= 2 * 1024 * 1024)
        && let Ok(inc_text) = std::fs::read_to_string(&resolved)
    {
        let idx = LineIndex::new(&inc_text);
        let offset = idx.clang_pos_to_offset(&inc_text, err_line, err_col);
        idx.offset_to_position(&inc_text, offset)
    } else {
        Position::new(err_line.saturating_sub(1), err_col.saturating_sub(1))
    };
    Some(Location::new(uri, Range::new(pos, pos)))
}
