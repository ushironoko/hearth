; Symbol tags for C#.
; Bundled locally because tree-sitter-c-sharp does not export TAGS_QUERY.

(namespace_declaration name: (_) @name) @definition.module

(class_declaration name: (identifier) @name) @definition.class
(struct_declaration name: (identifier) @name) @definition.class
(record_declaration name: (identifier) @name) @definition.class
(enum_declaration name: (identifier) @name) @definition.class
(interface_declaration name: (identifier) @name) @definition.interface

(delegate_declaration name: (identifier) @name) @definition.type

(constructor_declaration name: (identifier) @name) @definition.method
(method_declaration name: (identifier) @name) @definition.method
(property_declaration name: (identifier) @name) @definition.property
(event_declaration name: (identifier) @name) @definition.property
