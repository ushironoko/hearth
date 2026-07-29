; Symbol tags for Bash / POSIX shell.
; tree-sitter-bash ships no tags.scm, so octorus bundles its own.
;
; Only top-level assignments are tagged; assignments inside functions are
; locals and would drown the outline.

(function_definition name: (word) @name) @definition.function

(program (variable_assignment name: (variable_name) @name)) @definition.constant
