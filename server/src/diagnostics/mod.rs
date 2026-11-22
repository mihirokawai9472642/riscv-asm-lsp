mod clang;
mod hints;
mod parse;
mod worker;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use lsp_server::Message;
use lsp_types::Diagnostic;

pub use parse::{parse_clang_output, parse_clang_output_with_ctx};
pub use worker::DiagnosticsWorker;

pub type MessageSink = Arc<dyn Fn(Message) + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacroDefHint {
    pub macro_name: String,
    pub def_line: u32,
}

#[derive(Debug, Clone)]
pub struct StoredDiagnostic {
    pub diagnostic: Diagnostic,
    pub suggestions: Vec<String>,
    pub macro_def_hints: Vec<MacroDefHint>,
}

pub type DiagnosticsCache = Arc<Mutex<HashMap<String, Vec<StoredDiagnostic>>>>;

#[cfg(test)]
mod tests {
    use std::path::Path;

    use lsp_types::{DiagnosticSeverity, Position, Uri};

    use super::*;

    #[test]
    fn parses_clang_errors_with_multibyte_prefix_and_suggestions() {
        let src = "/* 日本語 */ mvi a0, 1\n    adid a0, a1, 1\n";
        let byte_col = "/* 日本語 */ ".len() + 1;
        let stderr = format!(
            "<stdin>:1:{byte_col}: error: unrecognized instruction mnemonic, did you mean: mv?\n\
             <stdin>:2:5: error: unrecognized instruction mnemonic, did you mean: add, addi?\n"
        );
        let diags = parse_clang_output(src, &stderr);
        assert_eq!(diags.len(), 2);

        // Line 0: "/* 日本語 */ " is 10 UTF-16 code units!
        assert_eq!(diags[0].diagnostic.range.start, Position::new(0, 10));
        assert_eq!(diags[0].diagnostic.range.end, Position::new(0, 13));
        assert_eq!(diags[0].suggestions, vec!["mv"]);

        assert_eq!(diags[1].diagnostic.range.start, Position::new(1, 4));
        assert_eq!(diags[1].diagnostic.range.end, Position::new(1, 8));
        assert_eq!(diags[1].suggestions, vec!["add", "addi"]);
    }

    #[test]
    fn parses_macro_instantiation_and_included_file_errors() {
        let src = "\
    .include \"bad.inc\"
#include \"bad.h\"
.macro TWICE
    nop
    mvx a0, a1
.endm
    TWICE
";
        let stderr = "\
Included from /tmp/--0bca6a.s:8:
/tmp/proj/bad.inc:1:5: error: unrecognized instruction mnemonic, did you mean: mv?
    mov a0, a1
    ^
In file included from <stdin>:2:
/tmp/proj/bad.h:1:2: error: bad header
    1 | #error bad header
      |  ^
<instantiation>:2:5: error: unrecognized instruction mnemonic, did you mean: mv?
    mvx a0, a1
    ^
<stdin>:7:5: note: while in macro instantiation
    TWICE
    ^
";
        let diags = parse_clang_output(src, stderr);
        assert_eq!(diags.len(), 3, "diags: {diags:?}");

        // 0: `.include "bad.inc"` on line 0
        assert_eq!(diags[0].diagnostic.range.start.line, 0);
        assert!(diags[0].diagnostic.message.contains("bad.inc:1:5"));
        assert!(diags[0].diagnostic.related_information.is_some());

        // 1: `#include "bad.h"` on line 1
        assert_eq!(diags[1].diagnostic.range.start.line, 1);
        assert!(diags[1].diagnostic.message.contains("bad.h:1:2"));
        assert!(diags[1].diagnostic.message.contains("bad header"));

        // 2: `TWICE` on line 6
        assert_eq!(diags[2].diagnostic.range.start, Position::new(6, 4));
        assert_eq!(diags[2].diagnostic.range.end, Position::new(6, 9));
        assert!(
            diags[2]
                .diagnostic
                .message
                .contains("in macro instantiation")
        );
    }

    #[test]
    fn parses_nested_macros_and_nested_asm_includes() {
        let src = "\
    .include \"mid.inc\"
.macro INNER
    mvx a0, a1
.endm
.macro OUTER
    INNER
.endm
    OUTER
";
        let stderr = "\
Included from /tmp/--490bbc.s:8:
Included from /tmp/proj/mid.inc:1:
/tmp/proj/bad.inc:1:5: warning: some warning in included file
    nop
    ^
<instantiation>:1:1: error: unrecognized instruction mnemonic
mvx a0, a1
^
<instantiation>:1:1: note: while in macro instantiation
INNER
^
<stdin>:8:5: note: while in macro instantiation
    OUTER
    ^
";
        let diags = parse_clang_output(src, stderr);
        assert_eq!(diags.len(), 2, "diags: {diags:?}");
        assert_eq!(diags[0].diagnostic.range.start.line, 0);
        assert_eq!(
            diags[0].diagnostic.severity,
            Some(DiagnosticSeverity::WARNING)
        );
        assert!(diags[0].diagnostic.message.contains("bad.inc:1:5"));

        assert_eq!(diags[1].diagnostic.range.start, Position::new(7, 4));
        assert_eq!(diags[1].diagnostic.range.end, Position::new(7, 9));
    }

    #[test]
    fn missing_include_file_covers_delimited_filename() {
        let src =
            "    .include \"not_there.inc\"\n#include \"missing.h\"\n#include <sys_missing.h>\n";
        let stderr = "\
<stdin>:1:14: error: Could not find include file 'not_there.inc'
    .include \"not_there.inc\"
             ^
<stdin>:2:10: fatal error: 'missing.h' file not found
    2 | #include \"missing.h\"
      |          ^~~~~~~~~~~
<stdin>:3:10: fatal error: 'sys_missing.h' file not found
    3 | #include <sys_missing.h>
      |          ^~~~~~~~~~~~~~~
";
        let diags = parse_clang_output(src, stderr);
        assert_eq!(diags.len(), 3, "diags: {diags:?}");
        assert_eq!(diags[0].diagnostic.range.start, Position::new(0, 13));
        assert_eq!(diags[0].diagnostic.range.end, Position::new(0, 28));
        assert_eq!(diags[1].diagnostic.range.start, Position::new(1, 9));
        assert_eq!(diags[1].diagnostic.range.end, Position::new(1, 20));
        assert_eq!(diags[2].diagnostic.range.start, Position::new(2, 9));
        assert_eq!(diags[2].diagnostic.range.end, Position::new(2, 24));
    }

    #[test]
    fn attaches_related_info_and_macro_hint_for_spaced_define_error() {
        let src = "# define X 5 is the size\n_start:\n    li a0, X\n";
        let stderr =
            "<stdin>:3:14: error: unexpected token\n    li a0, 5 is the size\n             ^\n";
        let uri: Uri = "file:///tmp/spaced.S".parse().expect("uri");
        let diags = parse_clang_output_with_ctx(
            src,
            stderr,
            Some(Path::new("/tmp/spaced.S")),
            Some(&uri),
            None,
        );
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].diagnostic.range.start.line, 2);
        assert_eq!(diags[0].macro_def_hints.len(), 1);
        assert_eq!(diags[0].macro_def_hints[0].macro_name, "X");
        assert_eq!(diags[0].macro_def_hints[0].def_line, 0);
        let rel = diags[0]
            .diagnostic
            .related_information
            .as_ref()
            .expect("related_information");
        assert_eq!(rel.len(), 1);
        assert_eq!(rel[0].location.uri, uri);
        assert_eq!(rel[0].location.range.start.line, 0);
        assert!(rel[0].message.contains("`X` expands to `5 is the size`"));
        assert!(rel[0].message.contains("line 1"));
        assert!(rel[0].message.contains("not a comment"));
    }

    #[test]
    fn linux_style_macro_error_gets_neutral_related_info_without_quickfix() {
        let src = "\
#ifdef BIG
# define CZ clz
#else
# define CZ ctz
#endif
    CZ a0, a1
";
        let stderr = "<stdin>:6:5: error: instruction requires the following: 'Zbb' (Basic Bit-Manipulation)\n    ctz a0, a1\n    ^\n";
        let uri: Uri = "file:///tmp/cz.S".parse().expect("uri");
        let diags = parse_clang_output_with_ctx(
            src,
            stderr,
            Some(Path::new("/tmp/cz.S")),
            Some(&uri),
            None,
        );
        assert_eq!(diags.len(), 1);
        assert!(
            diags[0].macro_def_hints.is_empty(),
            "unexpected quickfix hint for real macro: {:?}",
            diags[0].macro_def_hints
        );
        let rel = diags[0]
            .diagnostic
            .related_information
            .as_ref()
            .expect("neutral related_information");
        assert_eq!(rel.len(), 1);
        assert_eq!(rel[0].location.range.start.line, 3);
        assert_eq!(rel[0].message, "`CZ` expands to `ctz` (defined on line 4)");

        // Even with a prose define, an error whose column is before the macro usage stays neutral
        let src2 = "# define X 5 is the size\n_start:\n    mvi a0, X\n";
        let stderr2 = "<stdin>:3:5: error: unrecognized instruction mnemonic, did you mean: mv?\n    mvi a0, 5 is the size\n    ^\n";
        let uri2: Uri = "file:///tmp/col_before.S".parse().expect("uri");
        let diags2 = parse_clang_output_with_ctx(
            src2,
            stderr2,
            Some(Path::new("/tmp/col_before.S")),
            Some(&uri2),
            None,
        );
        assert_eq!(diags2.len(), 1);
        assert!(diags2[0].macro_def_hints.is_empty());
        let rel2 = diags2[0]
            .diagnostic
            .related_information
            .as_ref()
            .expect("neutral related_information");
        assert_eq!(
            rel2[0].message,
            "`X` expands to `5 is the size` (defined on line 1)"
        );
    }
}
