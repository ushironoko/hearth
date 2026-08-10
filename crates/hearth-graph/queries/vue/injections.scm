; Inline top-level Vue scripts. The generic JavaScript pattern is a fallback;
; range deduplication prefers the explicit language patterns below.

((component
  (script_element
    (raw_text) @injection.content))
 (#set! injection.language "javascript"))

; Quoted TypeScript: lang="ts" / lang='typescript'.
((component
  (script_element
    (start_tag
      (attribute
        (attribute_name) @_attr
        (quoted_attribute_value (attribute_value) @_lang)))
    (raw_text) @injection.content))
 (#eq? @_attr "lang")
 (#match? @_lang "^(ts|typescript)$")
 (#set! injection.language "typescript"))

; Unquoted TypeScript: lang=ts / lang=typescript.
((component
  (script_element
    (start_tag
      (attribute
        (attribute_name) @_attr
        (attribute_value) @_lang))
    (raw_text) @injection.content))
 (#eq? @_attr "lang")
 (#match? @_lang "^(ts|typescript)$")
 (#set! injection.language "typescript"))

; Quoted and unquoted TSX.
((component
  (script_element
    (start_tag
      (attribute
        (attribute_name) @_attr
        (quoted_attribute_value (attribute_value) @_lang)))
    (raw_text) @injection.content))
 (#eq? @_attr "lang")
 (#eq? @_lang "tsx")
 (#set! injection.language "tsx"))

((component
  (script_element
    (start_tag
      (attribute
        (attribute_name) @_attr
        (attribute_value) @_lang))
    (raw_text) @injection.content))
 (#eq? @_attr "lang")
 (#eq? @_lang "tsx")
 (#set! injection.language "tsx"))

; Quoted and unquoted JSX.
((component
  (script_element
    (start_tag
      (attribute
        (attribute_name) @_attr
        (quoted_attribute_value (attribute_value) @_lang)))
    (raw_text) @injection.content))
 (#eq? @_attr "lang")
 (#eq? @_lang "jsx")
 (#set! injection.language "jsx"))

((component
  (script_element
    (start_tag
      (attribute
        (attribute_name) @_attr
        (attribute_value) @_lang))
    (raw_text) @injection.content))
 (#eq? @_attr "lang")
 (#eq? @_lang "jsx")
 (#set! injection.language "jsx"))
