use std::path::Path;

use hearth_graph::{LanguageRegistry, ParserPool, extract_symbols};

#[test]
fn empty_registry_treats_source_files_as_unsupported() {
    let registry = LanguageRegistry::empty();
    assert!(!registry.supports_symbols(Path::new("src/lib.rs")));

    let mut pool = ParserPool::new(&registry);
    let symbols = extract_symbols("fn main() {}\n", "src/lib.rs", &mut pool);

    assert!(symbols.is_empty());
}
