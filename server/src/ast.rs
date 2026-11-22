use tree_sitter::Node;

use crate::index::parse_macro_var_token;

/// Shared tree-sitter traversal for RISC-V assembly files.
/// Both the workspace symbol indexer and the semantic token provider drive from this walker
/// so syntax-node classification rules live in one place.
pub trait AstVisitor {
    fn on_label(&mut self, _node: Node<'_>, _name: &str) {}
    fn on_numeric_label(&mut self, _node: Node<'_>, _name: &str) {}
    /// Return `true` to automatically walk `operands`, or `false` if handled custom.
    fn on_directive(&mut self, _node: Node<'_>) -> bool {
        true
    }
    /// Return an opaque state token passed back to `leave_instruction` after walking operands.
    fn enter_instruction(&mut self, _mnem: Node<'_>, _name: &str) -> bool {
        false
    }
    fn leave_instruction(&mut self, _prev_state: bool) {}
    fn enter_call_statement(&mut self, _name_node: Node<'_>, _name: &str) -> bool {
        false
    }
    fn leave_call_statement(&mut self, _prev_state: bool) {}
    fn on_assignment(
        &mut self,
        _full: Node<'_>,
        _left: Node<'_>,
        _name: &str,
        _right: Option<Node<'_>>,
    ) {
    }
    fn on_register(&mut self, _node: Node<'_>) {}
    fn on_numeric_label_ref(&mut self, _node: Node<'_>, _raw: &str) {}
    fn on_macro_var(&mut self, _param: &str, _span: (usize, usize)) {}
    fn on_macro_param_name(&mut self, _node: Node<'_>) {}
    fn on_macro_name(&mut self, _node: Node<'_>) {}
    fn on_symbol(&mut self, _node: Node<'_>, _name: &str) {}
}

pub fn walk_node(node: Node<'_>, text: &str, visitor: &mut impl AstVisitor) {
    match node.kind() {
        "comment" | "char" => return,
        "string" => {
            walk_string_macro_vars(node, text, visitor);
            return;
        }
        "label" => {
            if let Some(raw) = text.get(node.start_byte()..node.end_byte()) {
                let name = raw.trim_end_matches(':').trim();
                if !name.is_empty() && name != "." {
                    visitor.on_label(node, name);
                }
            }
            return;
        }
        "numeric_label" => {
            if let Some(raw) = text.get(node.start_byte()..node.end_byte()) {
                let name = raw.trim_end_matches(':').trim();
                if !name.is_empty() {
                    visitor.on_numeric_label(node, name);
                }
            }
            return;
        }
        "directive" => {
            if visitor.on_directive(node)
                && let Some(ops) = node.child_by_field_name("operands")
            {
                walk_node(ops, text, visitor);
            }
            return;
        }
        "instruction" => {
            let prev = if let Some(mnem) = node.child_by_field_name("mnemonic")
                && let Some(name) = text.get(mnem.start_byte()..mnem.end_byte())
                && !name.is_empty()
            {
                visitor.enter_instruction(mnem, name)
            } else {
                false
            };
            if let Some(ops) = node.child_by_field_name("operands") {
                walk_node(ops, text, visitor);
            }
            visitor.leave_instruction(prev);
            return;
        }
        "call_statement" => {
            let prev = if let Some(name_node) = node.child_by_field_name("name")
                && let Some(name) = text.get(name_node.start_byte()..name_node.end_byte())
                && name != "."
            {
                visitor.enter_call_statement(name_node, name)
            } else {
                false
            };
            if let Some(ops) = node.child_by_field_name("operands") {
                walk_node(ops, text, visitor);
            }
            visitor.leave_call_statement(prev);
            return;
        }
        "binary_expression" => {
            if let Some(op) = node.child_by_field_name("operator")
                && op.kind() == "assignment_operator"
                && let Some(left) = node.child_by_field_name("left")
                && left.kind() == "symbol"
                && let Some(name) = text.get(left.start_byte()..left.end_byte())
            {
                let right = node.child_by_field_name("right");
                if name != "." {
                    visitor.on_assignment(node, left, name, right);
                }
                if let Some(r) = right {
                    walk_node(r, text, visitor);
                }
                return;
            }
        }
        "register" => {
            visitor.on_register(node);
            return;
        }
        "macro_parameter_name" => {
            visitor.on_macro_param_name(node);
            return;
        }
        "macro_name" => {
            visitor.on_macro_name(node);
            return;
        }
        "symbol" => {
            if let Some(name) = text.get(node.start_byte()..node.end_byte())
                && name != "."
            {
                visitor.on_symbol(node, name);
            }
            return;
        }
        "numeric_label_reference" => {
            if let Some(raw) = text.get(node.start_byte()..node.end_byte()) {
                visitor.on_numeric_label_ref(node, raw);
            }
            return;
        }
        "macro_variable" | "string_macro_variable" => {
            emit_macro_var(node, text, visitor);
            return;
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_node(child, text, visitor);
    }
}

pub fn walk_node_after_offset(
    node: Node<'_>,
    min_start: usize,
    text: &str,
    visitor: &mut impl AstVisitor,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.end_byte() <= min_start {
            continue;
        }
        if child.start_byte() >= min_start {
            walk_node(child, text, visitor);
        } else {
            walk_node_after_offset(child, min_start, text, visitor);
        }
    }
}

fn walk_string_macro_vars(node: Node<'_>, text: &str, visitor: &mut impl AstVisitor) {
    if node.kind() == "string_macro_variable" {
        emit_macro_var(node, text, visitor);
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_string_macro_vars(child, text, visitor);
    }
}

fn emit_macro_var(node: Node<'_>, text: &str, visitor: &mut impl AstVisitor) {
    let start = node.start_byte();
    let end = node.end_byte();
    if let Some(raw) = text.get(start..end)
        && let Some(param) = parse_macro_var_token(raw)
    {
        // Span covering `\param` (avoid including trailing `\()...` in rename span)
        let param_end = (start + 1 + param.len()).min(end);
        visitor.on_macro_var(param, (start, param_end));
    }
}
