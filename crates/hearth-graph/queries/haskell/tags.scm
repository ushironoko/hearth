; Symbol tags for Haskell.
; tree-sitter-haskell ships no tags.scm, so octorus bundles its own.
;
; A function defined by several equations produces one match per equation;
; adjacent duplicates are collapsed by the extractor.

(function name: (variable) @name) @definition.function

(data_type name: (name) @name) @definition.class
(newtype name: (name) @name) @definition.class
(type_synomym name: (name) @name) @definition.type

(class name: (name) @name) @definition.interface

(header module: (module) @name) @definition.module
