//! `hearth` — the thin CLI client.
//!
//! By default it connects to a running `hearthd` (the warm, resident engine).
//! If no daemon is reachable it falls back to an in-process engine (a cold run),
//! so the CLI always works. The `--no-daemon` flag forces the inline path.

use clap::{Args, Parser, Subcommand};
use hearth_core::{Engine, EngineConfig};
use hearth_proto::*;
use hearth_tools::dispatch;
use hearth_tools::transport::{read_msg, send_request_with_fd};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

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
    /// Health check.
    Ping,
    /// Print the daemon profiler report.
    Stats,
    /// Ask the daemon to shut down.
    Stop,
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
        Cmd::Read { path, offset, limit, line_numbers } => Request::Read(ReadParams {
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
        Cmd::Edit { path, old, new, all } => Request::Edit(EditParams {
            path: path.clone(),
            old_string: old.clone(),
            new_string: new.clone(),
            replace_all: *all,
        }),
        Cmd::Bash { command, timeout_ms } => {
            Request::Bash(BashParams { timeout_ms: *timeout_ms, ..BashParams::new(command.clone()) })
        }
        Cmd::Grep {
            pattern, path, globs, ignore_case, smart_case, fixed, multiline, files, count,
            before, after, hidden, no_ignore,
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
        Cmd::Ping => Request::Ping,
        Cmd::Stats => Request::Stats,
        Cmd::Stop => Request::Shutdown,
    }
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
    // A read destined for stdout can be streamed: pass our stdout fd so the
    // daemon writes the content straight to it, skipping payload serialization.
    let stream_to_stdout = matches!(req, Request::Read(_)) && !global.json;
    if !global.no_daemon
        && let Ok(stream) = UnixStream::connect(socket_path(global)) {
        let fd = if stream_to_stdout {
            Some(std::io::stdout().as_raw_fd())
        } else {
            None
        };
        if send_request_with_fd(&stream, &req, fd).is_ok() {
            let mut rd = &stream;
            if let Ok(resp) = read_msg::<_, Response>(&mut rd) {
                return resp;
            }
        }
    }
    // Inline fallback: a fresh, one-shot engine (cold).
    let engine = Engine::new(EngineConfig {
        enable_optimizer: false,
        enable_watch: false,
        ..EngineConfig::default()
    });
    dispatch(&engine, req)
}

fn render(global: &Global, cmd: &Cmd, resp: &Response) -> i32 {
    use std::fmt::Write as _;
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
            if g.total_matches == 0 { 1 } else { 0 }
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
    }
}

