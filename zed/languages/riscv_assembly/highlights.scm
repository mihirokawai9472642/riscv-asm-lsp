; Directives
[
  (macro_mnemonic)
  (integer_mnemonic)
  (float_mnemonic)
  (string_mnemonic)
  (control_mnemonic)
] @keyword.directive

[
  (elf_type_tag)
  (option_flag)
  (macro_parameter_qualifier)
] @type

; Labels & symbols
[
  (label)
  (macro_label)
  (numeric_label)
  (numeric_label_reference)
] @label

(symbol) @variable

; Instructions & registers
(instruction_mnemonic) @function
(register) @variable.special

; Macros
(macro_name) @function.macro

[
  (macro_variable)
  (string_macro_variable)
  (macro_parameter_name)
] @variable.parameter

; Literals
[
  (octal)
  (binary)
  (decimal)
  (hexadecimal)
] @number

(float) @number.float
(char) @character
(string) @string

; Preprocessor directives (tree-sitter-riscv parses `#...` lines as comments)
((comment) @preproc
  (#match? @preproc "^#\\s*(include|include_next|import|define|undef|if|ifdef|ifndef|elif|elifdef|elifndef|else|endif|error|warning|pragma|line|ident|sccs|assert|unassert|embed)\\b"))

; Comments
(comment) @comment

; Punctuation & operators
[
  "="
  ","
  ";"
] @punctuation.delimiter

[
  "("
  ")"
] @punctuation.bracket

[
  (bitwise_or_operator)
  (logical_or_operator)
  (bitwise_and_operator)
  (logical_and_operator)
  (bitwise_xor_operator)
  (relational_operator)
  (shift_operator)
  (additive_operator)
  (multiplicative_operator)
  (equality_operator)
  (assignment_operator)
  (unary_minus_operator)
  (bitwise_not_operator)
  (logical_not_operator)
] @operator
