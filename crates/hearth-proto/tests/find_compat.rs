use hearth_proto::{FindParams, FindResult, Request, Response};

#[test]
fn find_params_defaults_are_stable_when_fields_are_omitted() {
    let params: FindParams = serde_json::from_str(r#"{"pattern":"*.rs"}"#).unwrap();
    assert_eq!(params, FindParams::new("*.rs"));
    assert_eq!(params.path, ".");
    assert_eq!(params.limit, None);
    assert!(params.hidden);
    assert!(params.respect_gitignore);
    assert!(!params.follow_symlinks);
    assert!(params.exclude_globs.is_empty());
}

#[test]
fn find_request_and_response_round_trip_through_msgpack() {
    let request = Request::Find(FindParams {
        pattern: "src/**/*.rs".into(),
        path: "/tmp/project".into(),
        limit: Some(12),
        hidden: true,
        respect_gitignore: false,
        follow_symlinks: true,
        exclude_globs: vec!["**/.git/**".into()],
    });
    let bytes = rmp_serde::to_vec_named(&request).unwrap();
    let decoded: Request = rmp_serde::from_slice(&bytes).unwrap();
    let Request::Find(decoded) = decoded else {
        panic!("find request variant was not preserved");
    };
    assert_eq!(decoded.pattern, "src/**/*.rs");
    assert_eq!(decoded.exclude_globs, vec!["**/.git/**"]);

    let response = Response::Find(FindResult {
        paths: vec!["src/lib.rs".into()],
        total_matches: 2,
        walk_cache_hit: true,
        limit_reached: true,
        output_limit_reached: false,
        root: "/tmp/project".into(),
    });
    let bytes = rmp_serde::to_vec_named(&response).unwrap();
    let decoded: Response = rmp_serde::from_slice(&bytes).unwrap();
    let Response::Find(decoded) = decoded else {
        panic!("find response variant was not preserved");
    };
    assert_eq!(decoded.paths, vec!["src/lib.rs"]);
    assert_eq!(decoded.total_matches, 2);
    assert!(decoded.walk_cache_hit);
    assert!(decoded.limit_reached);
}
