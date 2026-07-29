(import_statement
  source: (string) @import.source.static)

(export_statement
  source: (string) @import.source.reexport)

(call_expression
  function: (import)
  arguments: (arguments
    . (_) @import.source.dynamic))

(call_expression
  function: (identifier) @import.callee.commonjs
  arguments: (arguments
    . (_) @import.source.commonjs))
