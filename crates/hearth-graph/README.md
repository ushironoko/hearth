# hearth-graph

`hearth-graph` is a standalone Rust library for tree-sitter symbol extraction,
incremental symbol indexing, import analysis, and module-graph queries. It has
no global engine state and does not require Hearth's cache or transport crates.
Hosts own the language registry, source loading, cancellation, and concurrency
policy.

## Capabilities

- injectable [`LanguageRegistry`](https://docs.rs/hearth-graph/latest/hearth_graph/struct.LanguageRegistry.html)
  with an optional bundled grammar set
- one-pass symbol and import analysis over tree-sitter parse trees
- Vue 3 SFC analysis through JavaScript, TypeScript, JSX, and TSX script injections
- incremental `SymbolIndex` search and definition lookup
- incremental `ModuleGraph` dependency, reverse-dependency, and neighborhood
  queries with `Exact` or `Approximate` guarantees
- JavaScript/TypeScript resolution through `oxc_resolver`
- best-effort Rust module resolution
- cancellation-aware parallel index construction through a host-provided
  `SourceLoader`

Vue support covers inline JavaScript, TypeScript, JSX, and TSX `<script>`
blocks. External `src` scripts and custom non-JavaScript block languages are
not currently modeled.

## Quick start

```rust
use hearth_graph::{LanguageRegistry, ParserPool, extract_symbols};

let registry = LanguageRegistry::bundled();
let mut parsers = ParserPool::new(&registry);
let symbols = extract_symbols(
    "fn main() { println!(\"hello\"); }\n",
    "src/main.rs",
    &mut parsers,
);

assert!(symbols.iter().any(|symbol| symbol.name == "main"));
```

The default feature set includes the bundled languages, filesystem loader, and
both resolver implementations. A host can disable defaults and inject its own
grammar:

```rust
use hearth_graph::{LanguageRegistry, LanguageSpec};

# fn register(language: tree_sitter::Language) {
let mut registry = LanguageRegistry::empty();
registry.register(
    LanguageSpec::new("custom", language, ["custom"])
        .with_tags_query("(function_definition name: (identifier) @name) @definition.function"),
);
# }
```

## Features

| Feature | Enabled by default | Purpose |
| --- | --- | --- |
| `bundled-languages` | yes | Bundled grammars, symbol queries, import extractors, and Vue 3 SFC script injections |
| `fs` | yes | `FsLoader` implementation for direct filesystem indexing |
| `resolve-js` | yes | JavaScript and TypeScript resolution through `oxc_resolver` |
| `resolve-rust` | yes | Best-effort Rust module resolution |

`resolve-rust` deliberately reports partial completeness. Exact Rust resolution
requires Cargo target metadata and a module declaration tree that can model
`cfg`, `#[path]`, macros, and inline modules.

Resolver config reads use no-follow opens and opened-handle validation on Unix
and Windows. Targets without a secure no-follow primitive fail closed instead
of reading resolver configuration through a potentially redirected path.

## Versioning and MSRV

The minimum supported Rust version is **1.95**. The crate is currently in the
`0.3` series; public APIs may evolve between minor releases. Pin an exact
version when integrating a compatibility facade.

The detailed cache adapter, freshness model, and graph guarantees used by the
Hearth tools are documented in the
[Hearth architecture guide](https://github.com/ushironoko/hearth/blob/main/docs/ARCHITECTURE.md#graph).

## License

MIT
