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

enum WalkItem<'tree> {
    Node(Node<'tree>),
    LeaveInstruction(bool),
    LeaveCall(bool),
}

pub fn walk_node(node: Node<'_>, text: &str, visitor: &mut impl AstVisitor) {
    walk_stack(vec![WalkItem::Node(node)], text, visitor);
}

fn walk_stack<'tree>(mut stack: Vec<WalkItem<'tree>>, text: &str, visitor: &mut impl AstVisitor) {
    while let Some(item) = stack.pop() {
        let node = match item {
            WalkItem::Node(n) => n,
            WalkItem::LeaveInstruction(prev) => {
                visitor.leave_instruction(prev);
                continue;
            }
            WalkItem::LeaveCall(prev) => {
                visitor.leave_call_statement(prev);
                continue;
            }
        };
        match node.kind() {
            "comment" | "char" => continue,
            "string" => {
                walk_string_macro_vars(node, text, visitor);
                continue;
            }
            "label" => {
                if let Some(raw) = text.get(node.start_byte()..node.end_byte()) {
                    let name = raw.trim_end_matches(':').trim();
                    if !name.is_empty() && name != "." {
                        visitor.on_label(node, name);
                    }
                }
                continue;
            }
            "numeric_label" => {
                if let Some(raw) = text.get(node.start_byte()..node.end_byte()) {
                    let name = raw.trim_end_matches(':').trim();
                    if !name.is_empty() {
                        visitor.on_numeric_label(node, name);
                    }
                }
                continue;
            }
            "directive" => {
                if visitor.on_directive(node)
                    && let Some(ops) = node.child_by_field_name("operands")
                {
                    stack.push(WalkItem::Node(ops));
                }
                continue;
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
                stack.push(WalkItem::LeaveInstruction(prev));
                if let Some(ops) = node.child_by_field_name("operands") {
                    stack.push(WalkItem::Node(ops));
                }
                continue;
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
                stack.push(WalkItem::LeaveCall(prev));
                if let Some(ops) = node.child_by_field_name("operands") {
                    stack.push(WalkItem::Node(ops));
                }
                continue;
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
                        stack.push(WalkItem::Node(r));
                    }
                    continue;
                }
            }
            "register" => {
                visitor.on_register(node);
                continue;
            }
            "macro_parameter_name" => {
                visitor.on_macro_param_name(node);
                continue;
            }
            "macro_name" => {
                visitor.on_macro_name(node);
                continue;
            }
            "symbol" => {
                if let Some(name) = text.get(node.start_byte()..node.end_byte())
                    && name != "."
                {
                    visitor.on_symbol(node, name);
                }
                continue;
            }
            "numeric_label_reference" => {
                if let Some(raw) = text.get(node.start_byte()..node.end_byte()) {
                    visitor.on_numeric_label_ref(node, raw);
                }
                continue;
            }
            "macro_variable" | "string_macro_variable" => {
                emit_macro_var(node, text, visitor);
                continue;
            }
            _ => {}
        }

        let start = stack.len();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(WalkItem::Node(child));
        }
        stack[start..].reverse();
    }
}

pub fn walk_node_after_offset(
    node: Node<'_>,
    min_start: usize,
    text: &str,
    visitor: &mut impl AstVisitor,
) {
    let mut stack = vec![node];
    while let Some(cur) = stack.pop() {
        let mut cursor = cur.walk();
        let children: Vec<Node<'_>> = cur.children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            if child.end_byte() <= min_start {
                continue;
            }
            if child.start_byte() >= min_start {
                walk_node(child, text, visitor);
            } else {
                stack.push(child);
            }
        }
    }
}

fn walk_string_macro_vars(node: Node<'_>, text: &str, visitor: &mut impl AstVisitor) {
    let mut stack = vec![node];
    while let Some(cur) = stack.pop() {
        if cur.kind() == "string_macro_variable" {
            emit_macro_var(cur, text, visitor);
            continue;
        }
        let start = stack.len();
        let mut cursor = cur.walk();
        for child in cur.children(&mut cursor) {
            stack.push(child);
        }
        stack[start..].reverse();
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
