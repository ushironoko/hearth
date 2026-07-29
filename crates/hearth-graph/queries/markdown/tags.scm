; Symbol tags for Markdown.
;
; Headings are the natural outline of a prose document, which makes README /
; docs navigable with the same machinery as source files.

; The enclosing `section` is captured rather than the heading itself so that a
; `##` under a `#` nests in the outline — `atx_heading` nodes are siblings and
; carry no containment information.

(section (atx_heading heading_content: (inline) @name)) @definition.heading

(section (setext_heading heading_content: (paragraph) @name)) @definition.heading
