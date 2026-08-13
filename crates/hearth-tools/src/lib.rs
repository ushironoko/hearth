//! Hearth tools: `read`, `write`, `edit`, `bash`, `grep`.
//!
//! Every tool is a plain function taking `&Engine` and a params struct and
//! returning a result struct. No hidden globals: the caller owns the engine and
//! the shared caches live inside it. The daemon, CLI, and napi addon all call
//! these same functions.
//!
//! Each tool comes in two forms: a plain one, and a `*_cancellable` twin that
//! also takes a [`CancelToken`]. They are the same code path — the plain form
//! passes a token that can never be latched, which costs nothing — so there is
//! no second implementation to drift.

mod bash;
mod edit;
mod edit_text;
mod graph;
mod grep;
mod read;
mod shell;
pub mod transport;
mod util;
mod write;

pub use bash::{bash, bash_cancellable, bash_stream};
pub use edit::{edit, edit_batch, edit_batch_cancellable, edit_cancellable};
pub use graph::{graph, graph_cancellable, graph_clear};
pub use grep::{grep, grep_cancellable};
pub use read::{read, read_bytes, read_bytes_cancellable, read_cancellable};
pub use write::{write, write_cancellable, write_owned, write_owned_cancellable};

/// The pi-compatible text machinery behind `edit_batch`, exposed so a contract
/// suite can exercise the matching rules directly.
pub use edit_text::{
    AppliedEdits, apply_edits, apply_edits_opts, detect_crlf, diff_hunks,
    normalize_for_fuzzy_match, normalize_to_lf, restore_line_endings, split_line_count, strip_bom,
};

pub use hearth_core::{CancelToken, Engine};
pub use hearth_proto as proto;

/// Dispatch a protocol [`Request`] against the engine, returning a [`Response`].
/// This is the single entry point the daemon and napi layer share.
pub fn dispatch(engine: &Engine, req: hearth_proto::Request) -> hearth_proto::Response {
    dispatch_cancellable(engine, req, &CancelToken::none())
}

/// Dispatch a protocol request with cooperative cancellation for long-running
/// read/search/graph/Bash work. Mutations poll only at their safe points.
pub fn dispatch_cancellable(
    engine: &Engine,
    req: hearth_proto::Request,
    cancel: &CancelToken,
) -> hearth_proto::Response {
    use hearth_proto::{Request, Response};
    match req {
        Request::Read(p) => match read_cancellable(engine, &p, cancel) {
            Ok(r) => Response::Read(r),
            Err(e) => Response::Error(e),
        },
        Request::Write(p) => match write_cancellable(engine, &p, cancel) {
            Ok(r) => Response::Write(r),
            Err(e) => Response::Error(e),
        },
        Request::Edit(p) => match edit_cancellable(engine, &p, cancel) {
            Ok(r) => Response::Edit(r),
            Err(e) => Response::Error(e),
        },
        Request::EditBatch(p) => match edit_batch_cancellable(engine, &p, cancel) {
            Ok(r) => Response::EditBatch(r),
            Err(e) => Response::Error(e),
        },
        Request::Bash(p) => match bash_cancellable(engine, &p, cancel) {
            Ok(r) => Response::Bash(r),
            Err(e) => Response::Error(e),
        },
        Request::Grep(p) => match grep_cancellable(engine, &p, cancel) {
            Ok(r) => Response::Grep(r),
            Err(e) => Response::Error(e),
        },
        Request::Graph(p) => match graph_cancellable(engine, &p, cancel) {
            Ok(r) => Response::Graph(r),
            Err(e) => Response::Error(e),
        },
        Request::Invalidate(p) => Response::Invalidate(engine.invalidate(
            std::path::Path::new(&p.path),
            p.recursive,
            p.scope,
        )),
        Request::ClearCaches => Response::Invalidate(engine.clear_caches()),
        Request::Ping => Response::Pong,
        Request::Stats => Response::Stats(engine.profiler_report()),
        Request::Shutdown => Response::ShuttingDown,
    }
}
