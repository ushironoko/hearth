//! Wire-compatibility contract for the graph protocol: omitted request fields
//! retain their defaults, every externally tagged operation survives msgpack,
//! and frozen string vocabularies remain stable for existing clients.

use hearth_proto::{
    GraphBasisEntry, GraphCoverage, GraphDefinitionsResult, GraphDepEdge, GraphDepsResult,
    GraphGuarantee, GraphLanguageStatus, GraphMeta, GraphNeighborhoodResult, GraphNode, GraphOp,
    GraphOutlineResult, GraphOutput, GraphParams, GraphRdepEntry, GraphRdepsResult, GraphResult,
    GraphSearchResult, GraphStatusResult, GraphSymbol, GraphSymbolsResult, content_hash_hex,
};

fn symbol(kind: &str) -> GraphSymbol {
    GraphSymbol {
        name: "alpha".into(),
        kind: kind.into(),
        path: "/tmp/r/src/a.ts".into(),
        node_id: "src/a.ts@8000000000000001".into(),
        line: 3,
        column: 4,
        end_line: None,
        end_column: None,
        start_byte: None,
        end_byte: None,
        depth: 1,
    }
}

fn node(language: Option<&str>) -> GraphNode {
    GraphNode {
        path: "/tmp/r/src/a.ts".into(),
        node_id: "src/a.ts@8000000000000001".into(),
        language: language.map(str::to_owned),
        indexed: true,
    }
}

fn edge(kind: &str) -> GraphDepEdge {
    GraphDepEdge {
        from: "/tmp/r/src/a.ts".into(),
        from_node_id: "src/a.ts@8000000000000001".into(),
        to: "/tmp/r/src/b.ts".into(),
        to_node_id: Some("src/b.ts@00000000000000ab".into()),
        to_kind: "path".into(),
        specifier: "./b".into(),
        kind: kind.into(),
        line: 7,
        guarantee: GraphGuarantee::Exact,
    }
}

fn coverage() -> GraphCoverage {
    GraphCoverage {
        analyzed: 2,
        stubs: 1,
        basis: vec![GraphBasisEntry {
            path: "/tmp/r/src/a.ts".into(),
            content_hash_hex: "8000000000000001".into(),
        }],
    }
}

fn meta() -> GraphMeta {
    GraphMeta {
        guarantee: GraphGuarantee::Approximate,
        root: "/tmp/r".into(),
        universe_files: 11,
        indexed_files: 10,
        unsupported_files: 1,
        oversize_files: 2,
        revalidated_files: 9,
        reindexed_files: 3,
        swept: true,
        sweep_age_ms: 17,
        walk_cache_hit: true,
        repair_truncated: false,
    }
}

fn representative_outputs() -> Vec<GraphOutput> {
    let graph_symbol = symbol("function");
    let graph_node = node(Some("typescript"));
    let graph_edge = edge("import");
    let graph_coverage = coverage();

    vec![
        GraphOutput::Symbols(GraphSymbolsResult {
            path: graph_symbol.path.clone(),
            node_id: graph_symbol.node_id.clone(),
            symbols: vec![graph_symbol.clone()],
            truncated: true,
        }),
        GraphOutput::Outline(GraphOutlineResult {
            path: graph_symbol.path.clone(),
            node_id: graph_symbol.node_id.clone(),
            symbols: vec![graph_symbol.clone()],
            truncated: false,
        }),
        GraphOutput::Search(GraphSearchResult {
            symbols: vec![graph_symbol.clone()],
            limit_reached: true,
        }),
        GraphOutput::Definitions(GraphDefinitionsResult {
            symbols: vec![graph_symbol],
            limit_reached: false,
        }),
        GraphOutput::Deps(GraphDepsResult {
            node: graph_node.clone(),
            edges: vec![graph_edge.clone()],
            unresolved: vec![hearth_proto::GraphUnresolvedImport {
                specifier: "missing".into(),
                line: 8,
                reason: "not found".into(),
            }],
            coverage: graph_coverage.clone(),
        }),
        GraphOutput::Rdeps(GraphRdepsResult {
            node: graph_node.clone(),
            importers: vec![GraphRdepEntry {
                node: graph_node.clone(),
                specifier: Some("./a".into()),
                line: 9,
                guarantee: GraphGuarantee::Approximate,
            }],
            verified: true,
            coverage: graph_coverage.clone(),
        }),
        GraphOutput::Neighborhood(GraphNeighborhoodResult {
            center: graph_node.clone(),
            nodes: vec![graph_node],
            edges: vec![graph_edge],
            coverage: graph_coverage,
        }),
        GraphOutput::Status(GraphStatusResult {
            built: true,
            building: false,
            universe_files: 11,
            indexed_files: 10,
            unsupported_files: 1,
            oversize_files: 2,
            pending_files: 3,
            stale_files: 4,
            failed_files: 5,
            symbols: 101,
            edges: 23,
            components: 6,
            languages: vec![GraphLanguageStatus {
                language: "typescript".into(),
                files: 7,
                symbols: 89,
            }],
            last_sweep_ms_ago: Some(17),
            build_duration_us: Some(31),
        }),
    ]
}

#[test]
fn minimal_json_applies_graph_defaults() {
    let params: GraphParams =
        serde_json::from_str(r#"{"root":"/tmp/r","op":{"symbols":{"path":"src/a.ts"}}}"#).unwrap();
    assert_eq!(params.root, "/tmp/r");
    assert_eq!(
        params.op,
        GraphOp::Symbols {
            path: "src/a.ts".into()
        }
    );
    assert!(!params.hidden);
    assert!(params.respect_gitignore);
    assert!(!params.follow_symlinks);
    assert!(params.files.is_empty());
    assert_eq!(params.max_stale_ms, None);
    assert!(!params.include_basis);

    let search: GraphOp = serde_json::from_str(r#"{"search":{"query":"x"}}"#).unwrap();
    assert_eq!(
        search,
        GraphOp::Search {
            query: "x".into(),
            limit: 200
        }
    );

    let definitions: GraphOp = serde_json::from_str(r#"{"definitions":{"name":"x"}}"#).unwrap();
    assert_eq!(
        definitions,
        GraphOp::Definitions {
            name: "x".into(),
            limit: 200
        }
    );

    let deps: GraphOp = serde_json::from_str(r#"{"deps":{"path":"a"}}"#).unwrap();
    assert_eq!(
        deps,
        GraphOp::Deps {
            path: "a".into(),
            depth: 1
        }
    );

    let rdeps: GraphOp = serde_json::from_str(r#"{"rdeps":{"path":"a"}}"#).unwrap();
    assert_eq!(
        rdeps,
        GraphOp::Rdeps {
            path: "a".into(),
            depth: 1,
            verify: true
        }
    );

    let neighborhood: GraphOp = serde_json::from_str(r#"{"neighborhood":{"path":"a"}}"#).unwrap();
    assert_eq!(
        neighborhood,
        GraphOp::Neighborhood {
            path: "a".into(),
            depth: 1
        }
    );
}

#[test]
fn params_and_all_outputs_round_trip_through_named_msgpack() {
    let rdeps_params = GraphParams {
        root: "/tmp/r".into(),
        op: GraphOp::Rdeps {
            path: "/tmp/r/src/a.ts".into(),
            depth: 3,
            verify: false,
        },
        hidden: true,
        respect_gitignore: false,
        follow_symlinks: true,
        files: vec!["/tmp/r/src/a.ts".into()],
        max_stale_ms: Some(250),
        include_basis: true,
    };
    let bytes = rmp_serde::to_vec_named(&rdeps_params).unwrap();
    let back: GraphParams = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(back, rdeps_params);

    let status_params = GraphParams::new("/tmp/r", GraphOp::Status);
    let bytes = rmp_serde::to_vec_named(&status_params).unwrap();
    let back: GraphParams = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(back, status_params);

    let outputs = representative_outputs();
    assert_eq!(outputs.len(), 8);
    for output in outputs {
        let result = GraphResult {
            meta: meta(),
            output: output.clone(),
        };
        let bytes = rmp_serde::to_vec_named(&result).unwrap();
        let back: GraphResult = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back.meta, result.meta);
        assert_eq!(back.output, output);
    }
}

#[test]
fn graph_tags_serialize_as_exact_camel_case_strings() {
    assert_eq!(
        serde_json::to_string(&GraphGuarantee::Exact).unwrap(),
        "\"exact\""
    );
    assert_eq!(
        serde_json::to_string(&GraphGuarantee::Approximate).unwrap(),
        "\"approximate\""
    );

    let rdeps = serde_json::to_string(&GraphOp::Rdeps {
        path: "a".into(),
        depth: 2,
        verify: true,
    })
    .unwrap();
    assert!(
        rdeps.contains(r#"{"rdeps":{"#),
        "unexpected rdeps wire tag: {rdeps}"
    );
    assert_eq!(
        serde_json::to_string(&GraphOp::Status).unwrap(),
        "\"status\""
    );

    let basis = serde_json::to_string(&GraphBasisEntry {
        path: "/tmp/r/src/a.ts".into(),
        content_hash_hex: "8000000000000001".into(),
    })
    .unwrap();
    assert!(
        basis.contains(r#""contentHashHex":"8000000000000001""#),
        "unexpected basis wire key: {basis}"
    );
}

#[test]
fn missing_optional_graph_fields_stay_off_the_wire() {
    let graph_symbol = symbol("function");
    let json = serde_json::to_string(&graph_symbol).unwrap();
    for key in ["endLine", "endColumn", "startByte", "endByte"] {
        assert!(
            !json.contains(key),
            "unset field {key} leaked into the wire: {json}"
        );
    }

    let legacy_symbol: GraphSymbol = serde_json::from_str(&json).unwrap();
    assert_eq!(legacy_symbol.end_line, None);
    assert_eq!(legacy_symbol.end_column, None);
    assert_eq!(legacy_symbol.start_byte, None);
    assert_eq!(legacy_symbol.end_byte, None);

    let deps = GraphDepsResult {
        node: node(None),
        edges: Vec::new(),
        unresolved: Vec::new(),
        coverage: GraphCoverage {
            analyzed: 1,
            stubs: 0,
            basis: Vec::new(),
        },
    };
    let json = serde_json::to_string(&deps).unwrap();
    assert!(
        !json.contains("\"unresolved\""),
        "empty unresolved leaked: {json}"
    );
    assert!(!json.contains("\"basis\""), "empty basis leaked: {json}");

    let legacy_deps: GraphDepsResult = serde_json::from_str(&json).unwrap();
    assert!(legacy_deps.unresolved.is_empty());
    assert!(legacy_deps.coverage.basis.is_empty());
}

#[test]
fn dependency_edge_node_ids_and_target_kind_use_the_additive_camel_case_shape() {
    let path_edge = serde_json::to_value(edge("import")).unwrap();
    assert_eq!(path_edge["fromNodeId"], "src/a.ts@8000000000000001");
    assert_eq!(path_edge["toNodeId"], "src/b.ts@00000000000000ab");
    assert_eq!(path_edge["toKind"], "path");
    assert!(path_edge.get("from_node_id").is_none());
    assert!(path_edge.get("to_node_id").is_none());
    assert!(path_edge.get("to_kind").is_none());

    let mut external_edge = edge("import");
    external_edge.to = "react".into();
    external_edge.to_node_id = None;
    external_edge.to_kind = "external".into();
    let external_edge = serde_json::to_value(external_edge).unwrap();
    assert_eq!(external_edge["fromNodeId"], "src/a.ts@8000000000000001");
    assert_eq!(external_edge["toKind"], "external");
    assert!(
        external_edge.get("toNodeId").is_none(),
        "an external package must not serialize a file node id"
    );
}

#[test]
fn frozen_graph_vocabularies_serialize_exactly() {
    const SYMBOL_KINDS: [&str; 11] = [
        "function",
        "method",
        "class",
        "interface",
        "module",
        "macro",
        "constant",
        "type",
        "field",
        "property",
        "heading",
    ];
    const EDGE_KINDS: [&str; 7] = [
        "import",
        "reexport",
        "dynamic",
        "require",
        "tsrequire",
        "use",
        "mod",
    ];
    const LANGUAGES: [&str; 19] = [
        "rust",
        "typescript",
        "tsx",
        "javascript",
        "jsx",
        "go",
        "python",
        "ruby",
        "c",
        "cpp",
        "java",
        "csharp",
        "zig",
        "bash",
        "haskell",
        "lua",
        "php",
        "swift",
        "markdown",
    ];

    for kind in SYMBOL_KINDS {
        let value = serde_json::to_value(symbol(kind)).unwrap();
        assert_eq!(value["kind"], kind);
    }
    for kind in EDGE_KINDS {
        let value = serde_json::to_value(edge(kind)).unwrap();
        assert_eq!(value["kind"], kind);
    }
    for kind in ["path", "external"] {
        let mut edge = edge("import");
        edge.to_kind = kind.into();
        let value = serde_json::to_value(edge).unwrap();
        assert_eq!(value["toKind"], kind);
    }
    for language in LANGUAGES {
        let value = serde_json::to_value(node(Some(language))).unwrap();
        assert_eq!(value["language"], language);
    }
}

#[test]
fn content_hash_hex_preserves_high_bits_and_leading_zeroes() {
    let cases = [
        (0x8000_0000_0000_0001, "8000000000000001"),
        (0xab, "00000000000000ab"),
    ];

    for (hash, expected) in cases {
        assert_eq!(content_hash_hex(hash), expected);

        let entry = GraphBasisEntry {
            path: "/tmp/r/src/a.ts".into(),
            content_hash_hex: content_hash_hex(hash),
        };
        let bytes = rmp_serde::to_vec_named(&entry).unwrap();
        let back: GraphBasisEntry = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back.content_hash_hex, expected);
    }
}

#[test]
fn every_graph_op_and_both_envelopes_round_trip_through_named_msgpack() {
    let ops = vec![
        GraphOp::Symbols {
            path: "src/a.ts".into(),
        },
        GraphOp::Outline {
            path: "src/a.ts".into(),
        },
        GraphOp::Search {
            query: "alpha".into(),
            limit: 200,
        },
        GraphOp::Definitions {
            name: "alpha".into(),
            limit: 200,
        },
        GraphOp::Deps {
            path: "src/a.ts".into(),
            depth: 1,
        },
        GraphOp::Rdeps {
            path: "src/a.ts".into(),
            depth: 2,
            verify: false,
        },
        GraphOp::Neighborhood {
            path: "src/a.ts".into(),
            depth: 3,
        },
        GraphOp::Status,
    ];
    for op in ops {
        let params = GraphParams::new("/tmp/r", op);
        let bytes = rmp_serde::to_vec_named(&params).unwrap();
        let back: GraphParams = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, params);

        // Request/Response derive no PartialEq; canonical named re-encoding
        // is the equality witness.
        let request = hearth_proto::Request::Graph(params);
        let bytes = rmp_serde::to_vec_named(&request).unwrap();
        let back: hearth_proto::Request = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(rmp_serde::to_vec_named(&back).unwrap(), bytes);
    }

    let response = hearth_proto::Response::Graph(GraphResult {
        meta: meta(),
        output: GraphOutput::Status(GraphStatusResult {
            built: false,
            building: true,
            universe_files: 0,
            indexed_files: 0,
            unsupported_files: 0,
            oversize_files: 0,
            pending_files: 0,
            stale_files: 0,
            failed_files: 0,
            symbols: 0,
            edges: 0,
            components: 0,
            languages: vec![],
            last_sweep_ms_ago: None,
            build_duration_us: None,
        }),
    });
    let bytes = rmp_serde::to_vec_named(&response).unwrap();
    let back: hearth_proto::Response = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(rmp_serde::to_vec_named(&back).unwrap(), bytes);
}

#[test]
fn legacy_payloads_without_new_flags_default_to_false() {
    // A pre-repair_truncated meta and a pre-building status must reparse with
    // the new flags defaulting to false.
    let mut meta_value = serde_json::to_value(meta()).unwrap();
    meta_value
        .as_object_mut()
        .unwrap()
        .remove("repairTruncated");
    let reparsed: GraphMeta = serde_json::from_value(meta_value).unwrap();
    assert!(!reparsed.repair_truncated);

    let status = GraphStatusResult {
        built: true,
        building: false,
        universe_files: 1,
        indexed_files: 1,
        unsupported_files: 0,
        oversize_files: 0,
        pending_files: 0,
        stale_files: 0,
        failed_files: 0,
        symbols: 1,
        edges: 0,
        components: 1,
        languages: vec![],
        last_sweep_ms_ago: None,
        build_duration_us: None,
    };
    let mut status_value = serde_json::to_value(&status).unwrap();
    status_value.as_object_mut().unwrap().remove("building");
    let reparsed: GraphStatusResult = serde_json::from_value(status_value).unwrap();
    assert!(!reparsed.building);
}
