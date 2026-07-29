; Symbol tags for Zig.
; tree-sitter-zig ships no tags.scm, so octorus bundles its own.
;
; Zig models `const Foo = struct {...}` as a variable_declaration whose
; initializer is a container declaration, so type-like symbols are matched
; through the declaration rather than a dedicated node.

(function_declaration name: (identifier) @name) @definition.function

(variable_declaration (identifier) @name (struct_declaration)) @definition.class
(variable_declaration (identifier) @name (enum_declaration)) @definition.class
(variable_declaration (identifier) @name (union_declaration)) @definition.class
(variable_declaration (identifier) @name (opaque_declaration)) @definition.class

(source_file (variable_declaration (identifier) @name)) @definition.constant
