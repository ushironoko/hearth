//! Hearth tools: `read`, `write`, `edit`, `bash`, `grep`.
//!
//! Every tool is a plain function taking `&Engine` and a params struct and
//! returning a result struct. No hidden globals: the caller owns the engine and
//! the shared caches live inside it. The daemon, CLI, and napi addon all call
//! these same functions.

mod bash;
mod edit;
mod grep;
mod read;
mod shell;
pub mod transport;
mod util;
mod write;

pub use bash::bash;
pub use edit::edit;
pub use grep::grep;
pub use read::{read, read_bytes};
pub use write::{write, write_owned};

pub use hearth_core::Engine;
pub use hearth_proto as proto;

/// Dispatch a protocol [`Request`] against the engine, returning a [`Response`].
/// This is the single entry point the daemon and napi layer share.
pub fn dispatch(engine: &Engine, req: hearth_proto::Request) -> hearth_proto::Response {
    use hearth_proto::{Request, Response};
    match req {
        Request::Read(p) => match read(engine, &p) {
            Ok(r) => Response::Read(r),
            Err(e) => Response::Error(e),
        },
        Request::Write(p) => match write(engine, &p) {
            Ok(r) => Response::Write(r),
            Err(e) => Response::Error(e),
        },
        Request::Edit(p) => match edit(engine, &p) {
            Ok(r) => Response::Edit(r),
            Err(e) => Response::Error(e),
        },
        Request::Bash(p) => match bash(engine, &p) {
            Ok(r) => Response::Bash(r),
            Err(e) => Response::Error(e),
        },
        Request::Grep(p) => match grep(engine, &p) {
            Ok(r) => Response::Grep(r),
            Err(e) => Response::Error(e),
        },
        Request::Ping => Response::Pong,
        Request::Stats => Response::Stats(engine.profiler_report()),
        Request::Shutdown => Response::ShuttingDown,
    }
}
