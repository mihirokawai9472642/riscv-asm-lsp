use lsp_types::{TextDocumentContentChangeEvent, Uri};
use std::collections::HashMap;
use std::path::Path;
use tree_sitter::{InputEdit, Parser, Point, Tree};

use crate::context::{normalize_path, uri_to_path};
use crate::position::LineIndex;

#[derive(Debug, Clone)]
pub struct Document {
    pub uri: Uri,
    pub version: i32,
    pub text: String,
    pub line_index: LineIndex,
    pub tree: Option<Tree>,
}

impl Document {
    fn new(uri: Uri, version: i32, text: String, parser: &mut Parser) -> Self {
        let line_index = LineIndex::new(&text);
        let tree = parser.parse(&text, None);
        Self {
            uri,
            version,
            text,
            line_index,
            tree,
        }
    }

    fn apply_changes(
        &mut self,
        version: i32,
        changes: Vec<TextDocumentContentChangeEvent>,
        parser: &mut Parser,
    ) {
        self.version = version;
        for change in changes {
            match change.range {
                None => {
                    self.text = change.text;
                    self.line_index = LineIndex::new(&self.text);
                    self.tree = parser.parse(&self.text, None);
                }
                Some(range) => {
                    let start_byte = self.line_index.position_to_offset(&self.text, range.start);
                    let old_end_byte = self.line_index.position_to_offset(&self.text, range.end);
                    let (start_byte, old_end_byte) = if start_byte <= old_end_byte {
                        (start_byte, old_end_byte)
                    } else {
                        (old_end_byte, start_byte)
                    };

                    let start_point = byte_to_ts_point(&self.line_index, &self.text, start_byte);
                    let old_end_point =
                        byte_to_ts_point(&self.line_index, &self.text, old_end_byte);
                    let old_has_newline = self
                        .text
                        .get(start_byte..old_end_byte)
                        .is_some_and(|s| s.contains('\n'));

                    let new_end_byte = start_byte + change.text.len();
                    self.text
                        .replace_range(start_byte..old_end_byte, &change.text);
                    self.line_index.apply_edit(
                        start_byte,
                        old_end_byte,
                        old_has_newline,
                        &change.text,
                        &self.text,
                    );
                    let new_end_point =
                        byte_to_ts_point(&self.line_index, &self.text, new_end_byte);

                    let old_tree = self.tree.as_mut().map(|tree| {
                        tree.edit(&InputEdit {
                            start_byte,
                            old_end_byte,
                            new_end_byte,
                            start_position: start_point,
                            old_end_position: old_end_point,
                            new_end_position: new_end_point,
                        });
                        &*tree
                    });
                    self.tree = parser.parse(&self.text, old_tree);
                }
            }
        }
    }
}

fn byte_to_ts_point(line_index: &LineIndex, text: &str, byte_offset: usize) -> Point {
    let clamped = crate::position::clamp_to_char_boundary(text, byte_offset);
    let pos = line_index.offset_to_position(text, clamped);
    let row = pos.line as usize;
    let (line_start, _, _) = line_index.line_bounds(text, row);
    let col = clamped.saturating_sub(line_start);
    Point::new(row, col)
}

pub struct DocumentStore {
    docs: HashMap<String, Document>,
    parser: Parser,
}

impl DocumentStore {
    pub fn new() -> Result<Self, String> {
        let mut parser = Parser::new();
        let language = tree_sitter_riscv::LANGUAGE.into();
        parser
            .set_language(&language)
            .map_err(|err| format!("failed to initialize tree-sitter-riscv: {err}"))?;
        Ok(Self {
            docs: HashMap::new(),
            parser,
        })
    }

    pub fn open(&mut self, uri: Uri, version: i32, text: String) -> &Document {
        let key = uri.as_str().to_owned();
        let doc = Document::new(uri, version, text, &mut self.parser);
        self.docs.insert(key.clone(), doc);
        &self.docs[&key]
    }

    pub fn change(
        &mut self,
        uri: &Uri,
        version: i32,
        changes: Vec<TextDocumentContentChangeEvent>,
    ) -> Option<&Document> {
        let key = uri.as_str();
        let doc = self.docs.get_mut(key)?;
        doc.apply_changes(version, changes, &mut self.parser);
        Some(doc)
    }

    pub fn close(&mut self, uri: &Uri) -> Option<Document> {
        self.docs.remove(uri.as_str())
    }

    pub fn get(&self, uri: &Uri) -> Option<&Document> {
        self.docs.get(uri.as_str())
    }

    pub fn is_open_path(&self, canon_path: &Path) -> bool {
        self.docs
            .values()
            .any(|doc| uri_to_path(&doc.uri).is_some_and(|p| normalize_path(&p) == canon_path))
    }

    pub fn iter(&self) -> impl Iterator<Item = &Document> {
        self.docs.values()
    }

    pub fn all_open_snapshots(&self) -> Vec<(Uri, i32, String)> {
        self.docs
            .values()
            .map(|doc| (doc.uri.clone(), doc.version, doc.text.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};
    use std::str::FromStr;

    #[test]
    fn incremental_edits_preserve_tree_and_multibyte_positions() {
        let mut store = DocumentStore::new().expect("parser must initialize");
        let uri = Uri::from_str("file:///test.S").expect("valid uri");
        store.open(uri.clone(), 1, "# 日本語 🚀\naddi a0, zero, 1\n".to_owned());

        // Replace `1` with `42` on line 1
        store.change(
            &uri,
            2,
            vec![TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(1, 15), Position::new(1, 16))),
                range_length: None,
                text: "42".to_owned(),
            }],
        );
        let doc = store.get(&uri).expect("doc exists");
        assert_eq!(doc.version, 2);
        assert_eq!(doc.text, "# 日本語 🚀\naddi a0, zero, 42\n");
        assert!(doc.tree.is_some());

        // Out-of-range edit must not panic
        store.change(
            &uri,
            3,
            vec![TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(99, 99), Position::new(100, 100))),
                range_length: None,
                text: "nop\n".to_owned(),
            }],
        );
        let doc = store.get(&uri).expect("doc exists");
        assert!(doc.text.ends_with("nop\n"));
    }
}
