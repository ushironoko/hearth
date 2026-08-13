//! `hearth` — the thin CLI client.
//!
//! By default it connects to a running `hearthd` (the warm, resident engine).
//! If no authenticated daemon is reachable *before delivery begins*, it falls
//! back to an in-process engine (a cold run). Once sending may have begun it
//! reports an indeterminate result and never replays the operation. The
//! `--no-daemon` flag forces the inline path.

use clap::{Args, Parser, Subcommand};
use hearth_core::{Engine, EngineConfig};
use hearth_proto::*;
use hearth_tools::dispatch;
use hearth_tools::transport::{connect_verified, read_msg, send_request_with_fd};
use std::fmt::Write as _;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::PathBuf;

const DEFAULT_GRAPH_LIMIT: u64 = 200;
const DEFAULT_GRAPH_DEPTH: u32 = 1;

// The CLI is a short-lived client that allocates little; the system allocator
// avoids mimalloc's init cost and keeps process startup as low as possible
// (startup dominates a one-shot CLI call — see docs/BENCHMARKS.md).

#[derive(Parser, Debug)]
#[command(name = "hearth", about = "Hearth tool orchestrator CLI")]
struct Cli {
    #[command(flatten)]
    global: Global,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args, Debug)]
struct Global {
    /// Daemon socket path.
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    /// Never use the daemon; run the engine in-process (a cold run).
    #[arg(long, global = true)]
    no_daemon: bool,
    /// Emit JSON instead of human output.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Read a file (optionally a line window).
    Read {
        path: String,
        #[arg(long)]
        offset: Option<u64>,
        #[arg(long)]
        limit: Option<u64>,
        #[arg(short = 'n', long)]
        line_numbers: bool,
    },
    /// Write a file (content from stdin).
    Write { path: String },
    /// Replace a string in a file.
    Edit {
        path: String,
        #[arg(long)]
        old: String,
        #[arg(long)]
        new: String,
        #[arg(long)]
        all: bool,
    },
    /// Run a shell command.
    Bash {
        command: String,
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    /// Search a tree.
    Grep {
        pattern: String,
        #[arg(default_value = ".")]
        path: String,
        #[arg(short = 'g', long = "glob")]
        globs: Vec<String>,
        #[arg(short = 'i', long)]
        ignore_case: bool,
        #[arg(short = 'S', long)]
        smart_case: bool,
        #[arg(short = 'F', long)]
        fixed: bool,
        #[arg(long)]
        multiline: bool,
        #[arg(short = 'l', long = "files-with-matches")]
        files: bool,
        #[arg(short = 'c', long)]
        count: bool,
        #[arg(short = 'B', long, default_value_t = 0)]
        before: u32,
        #[arg(short = 'A', long, default_value_t = 0)]
        after: u32,
        #[arg(long)]
        hidden: bool,
        #[arg(long)]
        no_ignore: bool,
    },
    /// Query the code symbol and module graph.
    Graph {
        #[command(subcommand)]
        op: GraphCmd,
    },
    /// Health check.
    Ping,
    /// Print the daemon profiler report.
    Stats,
    /// Ask the daemon to shut down.
    Stop,
}

#[derive(Args, Debug)]
struct GraphArgs {
    /// Root directory to query.
    #[arg(long, default_value = ".")]
    root: String,
    /// Include hidden files.
    #[arg(long)]
    hidden: bool,
    /// Do not honor `.gitignore`/`.ignore` rules.
    #[arg(long)]
    no_ignore: bool,
    /// Follow symbolic links while walking.
    #[arg(long)]
    follow_symlinks: bool,
    /// Reuse a matching revalidation sweep up to this age.
    #[arg(long)]
    max_stale_ms: Option<u64>,
    /// Include file/hash basis entries in the response (only dependency
    /// operations carry a basis, so this has no effect until stage B).
    #[arg(long)]
    include_basis: bool,
}

#[derive(Subcommand, Debug)]
enum GraphCmd {
    /// List symbols extracted from a file.
    Symbols {
        path: String,
        #[command(flatten)]
        args: GraphArgs,
    },
    /// Show a file's nested symbol outline.
    Outline {
        path: String,
        #[command(flatten)]
        args: GraphArgs,
    },
    /// Search indexed symbols by name.
    Search {
        query: String,
        #[arg(long, default_value_t = DEFAULT_GRAPH_LIMIT)]
        limit: u64,
        #[command(flatten)]
        args: GraphArgs,
    },
    /// Find definitions with an exact name.
    Defs {
        name: String,
        #[arg(long, default_value_t = DEFAULT_GRAPH_LIMIT)]
        limit: u64,
        #[command(flatten)]
        args: GraphArgs,
    },
    /// Traverse dependencies from a file.
    Deps {
        path: String,
        #[arg(long, default_value_t = DEFAULT_GRAPH_DEPTH)]
        depth: u32,
        #[command(flatten)]
        args: GraphArgs,
    },
    /// Traverse reverse dependencies from a file.
    Rdeps {
        path: String,
        #[arg(long, default_value_t = DEFAULT_GRAPH_DEPTH)]
        depth: u32,
        /// Skip source-universe verification.
        #[arg(long)]
        no_verify: bool,
        #[command(flatten)]
        args: GraphArgs,
    },
    /// Traverse dependencies in both directions around a file.
    Neighborhood {
        path: String,
        #[arg(long, default_value_t = DEFAULT_GRAPH_DEPTH)]
        depth: u32,
        #[command(flatten)]
        args: GraphArgs,
    },
    /// Show graph build and coverage status.
    Status {
        #[command(flatten)]
        args: GraphArgs,
    },
}

fn main() {
    let cli = Cli::parse();
    let req = build_request(&cli.cmd);
    let resp = run(&cli.global, req);
    let code = render(&cli.global, &cli.cmd, &resp);
    std::process::exit(code);
}

fn build_request(cmd: &Cmd) -> Request {
    match cmd {
        Cmd::Read {
            path,
            offset,
            limit,
            line_numbers,
        } => Request::Read(ReadParams {
            path: path.clone(),
            offset: *offset,
            limit: *limit,
            line_numbers: *line_numbers,
            ..ReadParams::new(path.clone())
        }),
        Cmd::Write { path } => {
            let mut content = String::new();
            let _ = std::io::stdin().read_to_string(&mut content);
            Request::Write(WriteParams::new(path.clone(), content))
        }
        Cmd::Edit {
            path,
            old,
            new,
            all,
        } => Request::Edit(EditParams {
            path: path.clone(),
            old_string: old.clone(),
            new_string: new.clone(),
            replace_all: *all,
        }),
        Cmd::Bash {
            command,
            timeout_ms,
        } => Request::Bash(BashParams {
            timeout_ms: *timeout_ms,
            ..BashParams::new(command.clone())
        }),
        Cmd::Grep {
            pattern,
            path,
            globs,
            ignore_case,
            smart_case,
            fixed,
            multiline,
            files,
            count,
            before,
            after,
            hidden,
            no_ignore,
        } => {
            let mode = if *count {
                GrepMode::Count
            } else if *files {
                GrepMode::FilesWithMatches
            } else {
                GrepMode::Content
            };
            Request::Grep(GrepParams {
                pattern: pattern.clone(),
                path: path.clone(),
                mode,
                globs: globs.clone(),
                case_insensitive: *ignore_case,
                smart_case: *smart_case,
                fixed_strings: *fixed,
                multiline: *multiline,
                before_context: *before,
                after_context: *after,
                max_count: None,
                max_total_count: None,
                hidden: *hidden,
                respect_gitignore: !*no_ignore,
                follow_symlinks: false,
            })
        }
        Cmd::Graph { op } => {
            let (op, args) = match op {
                GraphCmd::Symbols { path, args } => (GraphOp::Symbols { path: path.clone() }, args),
                GraphCmd::Outline { path, args } => (GraphOp::Outline { path: path.clone() }, args),
                GraphCmd::Search { query, limit, args } => (
                    GraphOp::Search {
                        query: query.clone(),
                        limit: *limit,
                    },
                    args,
                ),
                GraphCmd::Defs { name, limit, args } => (
                    GraphOp::Definitions {
                        name: name.clone(),
                        limit: *limit,
                    },
                    args,
                ),
                GraphCmd::Deps { path, depth, args } => (
                    GraphOp::Deps {
                        path: path.clone(),
                        depth: *depth,
                    },
                    args,
                ),
                GraphCmd::Rdeps {
                    path,
                    depth,
                    no_verify,
                    args,
                } => (
                    GraphOp::Rdeps {
                        path: path.clone(),
                        depth: *depth,
                        verify: !*no_verify,
                    },
                    args,
                ),
                GraphCmd::Neighborhood { path, depth, args } => (
                    GraphOp::Neighborhood {
                        path: path.clone(),
                        depth: *depth,
                    },
                    args,
                ),
                GraphCmd::Status { args } => (GraphOp::Status, args),
            };
            Request::Graph(GraphParams {
                root: canonical_graph_root(&args.root),
                op,
                hidden: args.hidden,
                respect_gitignore: !args.no_ignore,
                follow_symlinks: args.follow_symlinks,
                files: Vec::new(),
                max_stale_ms: args.max_stale_ms,
                include_basis: args.include_basis,
            })
        }
        Cmd::Ping => Request::Ping,
        Cmd::Stats => Request::Stats,
        Cmd::Stop => Request::Shutdown,
    }
}

fn canonical_graph_root(root: &str) -> String {
    let path = PathBuf::from(root);
    let absolute = path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(&path))
                .unwrap_or(path)
        }
    });
    absolute.to_string_lossy().into_owned()
}

fn socket_path(global: &Global) -> PathBuf {
    if let Some(s) = &global.socket {
        return s.clone();
    }
    if let Some(s) = std::env::var_os("HEARTH_SOCKET") {
        return PathBuf::from(s);
    }
    hearth_tools::transport::default_socket_path()
}

fn run(global: &Global, req: Request) -> Response {
    if global.no_daemon {
        return dispatch_inline(req);
    }

    // Connecting and authenticating the server happen before delivery. Only a
    // failure here is safe to fall back from: no request byte reached a daemon.
    let stream = match connect_verified(&socket_path(global)) {
        Ok(stream) => stream,
        Err(_) => return dispatch_inline(req),
    };

    // Negotiate before any operation or FD transfer. A mismatch is therefore
    // a safe pre-dispatch failure, never an ambiguous mutation outcome.
    let hello = Request::Hello(ProtocolHello {
        version: PROTOCOL_VERSION,
    });
    if let Err(error) = send_request_with_fd(&stream, &hello, None) {
        return Response::Error(ToolError::indeterminate(format!(
            "protocol handshake delivery may have begun: {error}"
        )));
    }
    let mut reader = &stream;
    match read_msg::<_, Response>(&mut reader) {
        Ok(Response::Hello(ProtocolAck { version })) if version == PROTOCOL_VERSION => {}
        Ok(Response::Error(error)) => return Response::Error(error),
        Ok(_) => {
            return Response::Error(ToolError::invalid(
                "daemon returned an incompatible protocol handshake",
            ));
        }
        Err(error) => {
            return Response::Error(ToolError::indeterminate(format!(
                "protocol handshake response was lost or invalid: {error}"
            )));
        }
    }

    // A read destined for stdout can be streamed: pass our stdout fd so the
    // daemon writes the content straight to it, skipping payload serialization.
    let fd = if matches!(req, Request::Read(_)) && !global.json {
        Some(std::io::stdout().as_raw_fd())
    } else {
        None
    };
    if let Err(error) = send_request_with_fd(&stream, &req, fd) {
        return Response::Error(ToolError::indeterminate(format!(
            "daemon request delivery may have begun; request was not replayed: {error}"
        )));
    }

    match read_msg::<_, Response>(&mut reader) {
        Ok(response) => response,
        Err(error) => Response::Error(ToolError::indeterminate(format!(
            "daemon response was lost or invalid; request was not replayed: {error}"
        ))),
    }
}

fn dispatch_inline(req: Request) -> Response {
    let engine = Engine::new(EngineConfig {
        enable_optimizer: false,
        enable_watch: false,
        ..EngineConfig::default()
    });
    dispatch(&engine, req)
}

fn render(global: &Global, cmd: &Cmd, resp: &Response) -> i32 {
    if global.json {
        if let Ok(s) = serde_json::to_string(resp) {
            println!("{s}");
        }
        return match resp {
            Response::Error(_) => 1,
            _ => 0,
        };
    }
    let count_mode = matches!(cmd, Cmd::Grep { count: true, .. });
    match resp {
        Response::Hello(_) => {
            eprintln!("error: unexpected protocol handshake response");
            1
        }
        Response::Read(r) => {
            print!("{}", r.content);
            0
        }
        Response::Streamed(_) => {
            // Content was already written to our stdout by the daemon.
            0
        }
        Response::Write(r) => {
            eprintln!("wrote {} bytes", r.bytes_written);
            0
        }
        Response::Edit(r) => {
            eprintln!("{} replacement(s)", r.replacements);
            0
        }
        Response::Bash(r) => {
            print!("{}", r.stdout);
            eprint!("{}", r.stderr);
            if r.timed_out {
                eprintln!("(timed out)");
            }
            r.exit_code
        }
        Response::Grep(g) => {
            let mut out = String::new();
            for f in &g.files {
                if count_mode {
                    let _ = writeln!(out, "{}:{}", f.path, f.match_count);
                } else if f.lines.is_empty() {
                    let _ = writeln!(out, "{}", f.path);
                } else {
                    for l in &f.lines {
                        let sep = if l.is_match { ':' } else { '-' };
                        let _ = writeln!(out, "{}:{}{}{}", f.path, l.line_number, sep, l.text);
                    }
                }
            }
            print!("{out}");
            if g.incomplete {
                eprintln!("error: grep could not search every selected file completely");
                2
            } else if g.limit_reached {
                eprintln!("warning: grep result limit reached");
                0
            } else if g.total_matches == 0 {
                1
            } else {
                0
            }
        }
        Response::EditBatch(r) => {
            eprintln!("{} replacement(s)", r.replacements);
            0
        }
        Response::Invalidate(r) => {
            eprintln!(
                "invalidated {} file(s), {} walk(s)",
                r.files_invalidated, r.walks_invalidated
            );
            0
        }
        Response::Pong => {
            println!("pong");
            0
        }
        Response::Stats(s) => {
            print!("{s}");
            0
        }
        Response::ShuttingDown => {
            eprintln!("daemon shutting down");
            0
        }
        Response::Error(e) => {
            eprintln!("error: {e}");
            1
        }
        Response::Graph(g) => render_graph(g),
    }
}

fn render_graph(result: &GraphResult) -> i32 {
    if result.meta.guarantee == GraphGuarantee::Approximate {
        eprintln!("note: graph result is approximate");
    }

    let mut out = String::new();
    let empty_is_error = match &result.output {
        GraphOutput::Symbols(result) => {
            render_graph_symbols(&mut out, &result.symbols);
            false
        }
        GraphOutput::Outline(result) => {
            render_graph_symbols(&mut out, &result.symbols);
            false
        }
        GraphOutput::Search(result) => {
            render_graph_symbols(&mut out, &result.symbols);
            result.symbols.is_empty()
        }
        GraphOutput::Definitions(result) => {
            render_graph_symbols(&mut out, &result.symbols);
            result.symbols.is_empty()
        }
        GraphOutput::Deps(result) => {
            render_graph_edges(&mut out, &result.edges);
            render_graph_unresolved(&mut out, &result.unresolved);
            result.edges.is_empty() && result.unresolved.is_empty()
        }
        GraphOutput::Rdeps(result) => {
            render_graph_rdeps(&mut out, result);
            result.importers.is_empty()
        }
        GraphOutput::Neighborhood(result) => {
            render_graph_edges(&mut out, &result.edges);
            result.edges.is_empty()
        }
        GraphOutput::Status(result) => {
            render_graph_status(&mut out, result);
            false
        }
    };
    print!("{out}");
    i32::from(empty_is_error)
}

fn render_graph_symbols(out: &mut String, symbols: &[GraphSymbol]) {
    let mut symbols: Vec<_> = symbols.iter().collect();
    symbols.sort_by(|left, right| {
        (
            &left.path,
            left.line,
            left.column,
            &left.kind,
            &left.name,
            &left.node_id,
        )
            .cmp(&(
                &right.path,
                right.line,
                right.column,
                &right.kind,
                &right.name,
                &right.node_id,
            ))
    });
    for symbol in symbols {
        let _ = writeln!(
            out,
            "{}:{}:{}\t{}\t{}",
            symbol.path, symbol.line, symbol.column, symbol.kind, symbol.name
        );
    }
}

fn render_graph_edges(out: &mut String, edges: &[GraphDepEdge]) {
    let mut lines: Vec<_> = edges
        .iter()
        .map(|edge| {
            format!(
                "{} --[{} {:?}]--> {} [{}]",
                edge.from,
                edge.kind,
                edge.specifier,
                edge.to,
                graph_guarantee_label(edge.guarantee)
            )
        })
        .collect();
    lines.sort();
    for line in lines {
        let _ = writeln!(out, "{line}");
    }
}

fn render_graph_unresolved(out: &mut String, unresolved: &[GraphUnresolvedImport]) {
    let mut lines: Vec<_> = unresolved
        .iter()
        .map(|entry| {
            format!(
                "unresolved --[{:?}]--> ? line {} ({})",
                entry.specifier, entry.line, entry.reason
            )
        })
        .collect();
    lines.sort();
    for line in lines {
        let _ = writeln!(out, "{line}");
    }
}

fn render_graph_rdeps(out: &mut String, result: &GraphRdepsResult) {
    let mut lines: Vec<_> = result
        .importers
        .iter()
        .map(|entry| {
            let evidence = entry.specifier.as_deref().map_or_else(
                || "grep".to_owned(),
                |specifier| format!("import {specifier:?}"),
            );
            format!(
                "{} --[{evidence}]--> {} [{}]",
                entry.node.node_id,
                result.node.node_id,
                graph_guarantee_label(entry.guarantee)
            )
        })
        .collect();
    lines.sort();
    for line in lines {
        let _ = writeln!(out, "{line}");
    }
}

fn graph_guarantee_label(guarantee: GraphGuarantee) -> &'static str {
    match guarantee {
        GraphGuarantee::Exact => "exact",
        GraphGuarantee::Approximate => "approximate",
    }
}

fn render_graph_status(out: &mut String, status: &GraphStatusResult) {
    let _ = writeln!(out, "built: {}", status.built);
    let _ = writeln!(out, "building: {}", status.building);
    let _ = writeln!(out, "universe_files: {}", status.universe_files);
    let _ = writeln!(out, "indexed_files: {}", status.indexed_files);
    let _ = writeln!(out, "unsupported_files: {}", status.unsupported_files);
    let _ = writeln!(out, "oversize_files: {}", status.oversize_files);
    let _ = writeln!(out, "pending_files: {}", status.pending_files);
    let _ = writeln!(out, "stale_files: {}", status.stale_files);
    let _ = writeln!(out, "failed_files: {}", status.failed_files);
    let _ = writeln!(out, "symbols: {}", status.symbols);
    let _ = writeln!(out, "edges: {}", status.edges);
    let _ = writeln!(out, "components: {}", status.components);
    let _ = writeln!(out, "languages: {}", status.languages.len());

    let mut languages: Vec<_> = status.languages.iter().collect();
    languages.sort_by(|left, right| left.language.cmp(&right.language));
    for language in languages {
        let _ = writeln!(
            out,
            "language.{}.files: {}",
            language.language, language.files
        );
        let _ = writeln!(
            out,
            "language.{}.symbols: {}",
            language.language, language.symbols
        );
    }

    let _ = writeln!(
        out,
        "last_sweep_ms_ago: {}",
        status
            .last_sweep_ms_ago
            .map_or_else(|| "none".to_owned(), |value| value.to_string())
    );
    let _ = writeln!(
        out,
        "build_duration_us: {}",
        status
            .build_duration_us
            .map_or_else(|| "none".to_owned(), |value| value.to_string())
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn graph_node(path: &str, node_id: &str) -> GraphNode {
        GraphNode {
            path: path.into(),
            node_id: node_id.into(),
            language: Some("typescript".into()),
            indexed: true,
        }
    }

    fn coverage() -> GraphCoverage {
        GraphCoverage {
            analyzed: 1,
            stubs: 0,
            basis: Vec::new(),
        }
    }

    fn meta(guarantee: GraphGuarantee) -> GraphMeta {
        GraphMeta {
            guarantee,
            root: "/tmp/root".into(),
            universe_files: 1,
            indexed_files: 1,
            unsupported_files: 0,
            oversize_files: 0,
            revalidated_files: 1,
            reindexed_files: 0,
            swept: true,
            sweep_age_ms: 0,
            walk_cache_hit: false,
            repair_truncated: false,
        }
    }

    #[test]
    fn unresolved_only_deps_render_details_and_exit_successfully() {
        let unresolved = GraphUnresolvedImport {
            specifier: "missing-package".into(),
            line: 7,
            reason: "package not found".into(),
        };
        let mut out = String::new();
        render_graph_unresolved(&mut out, std::slice::from_ref(&unresolved));
        assert!(!out.is_empty());
        assert!(out.contains("\"missing-package\""));
        assert!(out.contains("package not found"));

        let result = GraphResult {
            meta: meta(GraphGuarantee::Exact),
            output: GraphOutput::Deps(GraphDepsResult {
                node: graph_node("/tmp/root/a.ts", "a.ts@0000000000000001"),
                edges: Vec::new(),
                unresolved: vec![unresolved],
                coverage: coverage(),
            }),
        };
        assert_eq!(render_graph(&result), 0);
    }

    #[test]
    fn edge_guarantee_labels_match_the_wire_vocabulary() {
        let mut out = String::new();
        render_graph_edges(
            &mut out,
            &[GraphDepEdge {
                from: "/tmp/root/a.ts".into(),
                from_node_id: "a.ts@0000000000000001".into(),
                to: "/tmp/root/b.ts".into(),
                to_node_id: Some("b.ts@0000000000000002".into()),
                to_kind: "path".into(),
                specifier: "./b".into(),
                kind: "import".into(),
                line: 1,
                guarantee: GraphGuarantee::Approximate,
            }],
        );
        assert!(out.trim_end().ends_with("[approximate]"));

        let labels: BTreeSet<_> = [GraphGuarantee::Exact, GraphGuarantee::Approximate]
            .map(graph_guarantee_label)
            .into_iter()
            .collect();
        assert_eq!(labels, BTreeSet::from(["approximate", "exact"]));
    }

    #[test]
    fn rdeps_distinguish_import_specifiers_from_grep_evidence() {
        let target = graph_node("/tmp/root/b.ts", "b.ts@0000000000000002");
        let result = GraphRdepsResult {
            node: target,
            importers: vec![
                GraphRdepEntry {
                    node: graph_node("/tmp/root/a.ts", "a.ts@0000000000000001"),
                    specifier: Some("./b".into()),
                    line: 1,
                    guarantee: GraphGuarantee::Exact,
                },
                GraphRdepEntry {
                    node: graph_node("/tmp/root/grep.ts", "grep.ts@0000000000000003"),
                    specifier: None,
                    line: 4,
                    guarantee: GraphGuarantee::Approximate,
                },
            ],
            verified: false,
            coverage: coverage(),
        };
        let mut out = String::new();
        render_graph_rdeps(&mut out, &result);

        assert!(out.contains("--[import \"./b\"]-->"));
        assert!(out.contains("--[grep]-->"));
        assert!(!out.contains("--[import \"\"]-->"));
    }
}
