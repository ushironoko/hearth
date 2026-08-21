//! Node.js bindings for the Hearth tool orchestrator (napi-rs v3).
//!
//! One thin binding over the shared Rust core. Every tool has a synchronous
//! method (fast, runs on the JS thread) and an `*Async` twin that offloads to a
//! libuv worker thread via `AsyncTask` — no tokio reactor is embedded.
//!
//! The engine is an explicit object the caller constructs and holds; there is no
//! hidden global singleton, matching the "one resident engine per process" model
//! while keeping ownership in the caller's hands.
//!
//! **Cancellation.** Every async method accepts an optional `AbortSignal`. A
//! signal that is *already* aborted rejects before any work starts; one aborted
//! mid-flight latches a [`CancelToken`] the native side polls at its own safe
//! points. Nothing is preempted: a file mutation keeps its per-path lock until
//! its bytes are committed, and `grep` joins every worker before settling, so
//! when the returned promise rejects nothing is still running. Cancellation
//! surfaces as its own error kind, distinct from `timeout` and from ordinary
//! I/O failure.
//!
//! **Errors.** Every failed call — synchronous throw or promise rejection —
//! surfaces a JS `Error` carrying the structured fields callers branch on:
//! `code` is the stable kind tag (`notFound`, `permission`, `noMatch`,
//! `multipleMatches`, `overlap`, `noChange`, `invalidInput`, `timeout`,
//! `cancelled`, `indeterminate`, `io`, `internal`), `editIndex` is the 0-based
//! index of the failing replacement when one `editBatch` edit is at fault, and
//! `path` is the file involved when one is. The message still leads with
//! `"<kind>: "`, but that is presentation — the properties are the contract.

use hearth_core::{CancelToken, Engine, EngineConfig};
use hearth_proto as proto;
use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

mod types;
pub use types::*;

#[cfg(not(feature = "profiling"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "profiling")]
#[global_allocator]
static GLOBAL: hearth_core::profiler::ProfilingAllocator<mimalloc::MiMalloc> =
    hearth_core::profiler::ProfilingAllocator::from_allocator(mimalloc::MiMalloc);

/// The plain `"<kind>: <message>"` error, for contexts with no JS-thread
/// access (a worker thread mid-`compute`) and as the fallback when building
/// the structured object itself fails.
fn plain_err(e: &proto::ToolError) -> Error {
    Error::new(
        Status::GenericFailure,
        format!("{}: {}", e.kind.as_str(), e.message),
    )
}

/// Keep the tool error for `reject` — which runs on the JS thread and can
/// build the structured object — and hand `compute` the placeholder it must
/// return meanwhile.
fn stash(slot: &mut Option<proto::ToolError>, e: proto::ToolError) -> Error {
    let placeholder = plain_err(&e);
    *slot = Some(e);
    placeholder
}

/// Turn a tool error into a JS `Error` carrying the structured fields a caller
/// branches on — `code` (the stable error kind), `editIndex` (which `edits[]`
/// entry failed), `path` — so no caller ever parses prose.
///
/// The fields are set on a real JS error object; wrapping that object back
/// into [`Error`] keeps a reference to it, so a synchronous throw and a
/// promise rejection both surface this exact object, properties intact.
fn structured_err(env: &Env, e: proto::ToolError) -> Error {
    build_structured(env, &e).unwrap_or_else(|_| plain_err(&e))
}

fn build_structured(env: &Env, e: &proto::ToolError) -> Result<Error> {
    let mut error = env.create_error(plain_err(e))?;
    // `napi_create_error` filled `code` with the napi status; overwrite it
    // with the kind tag, which is the value callers actually branch on.
    error.set_named_property("code", e.kind.as_str())?;
    if let Some(index) = e.edit_index {
        error.set_named_property("editIndex", index)?;
    }
    if let Some(path) = &e.path {
        error.set_named_property("path", path.as_str())?;
    }
    Ok(Error::from(error.to_unknown()))
}

/// A JS `AbortSignal` bound to a native [`CancelToken`].
///
/// napi's own `AbortSignal` only reports *transitions*, so an already-aborted
/// signal would never fire. Reading `aborted` while converting is what makes
/// "a pre-aborted call does no work" true rather than merely likely.
pub struct Abort {
    token: CancelToken,
    signal: AbortSignal,
}

impl FromNapiValue for Abort {
    unsafe fn from_napi_value(env: sys::napi_env, napi_val: sys::napi_value) -> Result<Self> {
        let object = unsafe { Object::from_napi_value(env, napi_val)? };
        let already = object
            .get_named_property::<bool>("aborted")
            .unwrap_or(false);
        let signal = unsafe { AbortSignal::from_napi_value(env, napi_val)? };

        let token = CancelToken::new();
        if already {
            token.cancel();
        }
        let on_abort = token.clone();
        signal.on_abort(move || on_abort.cancel());
        Ok(Self { token, signal })
    }
}

impl TypeName for Abort {
    fn type_name() -> &'static str {
        "AbortSignal"
    }
    fn value_type() -> ValueType {
        ValueType::Object
    }
}

impl ValidateNapiValue for Abort {}

/// Split an optional signal into the token the worker polls and the handle
/// napi needs to cancel the queued work item.
fn split(signal: Option<Abort>) -> (CancelToken, Option<AbortSignal>) {
    match signal {
        Some(Abort { token, signal }) => (token, Some(signal)),
        None => (CancelToken::none(), None),
    }
}

/// The stream callback `bashStream` calls for every chunk.
type ChunkCallback = ThreadsafeFunction<BashChunk, (), BashChunk, Status, false>;

/// How long a stalled JS consumer is waited on before the promise settles
/// anyway. Measured from the *last* delivery, so a slow-but-progressing
/// consumer is never cut off — only one that has stopped draining entirely.
const CHUNK_DELIVERY_IDLE_LIMIT: Duration = Duration::from_secs(30);

/// Delivers chunks to JS, and knows when JS has actually received them.
///
/// `AsyncTask` settles its promise from the JS thread as soon as `compute`
/// returns, while a non-blocking threadsafe call is only *queued* at that
/// point. Without a barrier the promise can therefore settle before the last
/// chunks are delivered — which a streaming caller sees as silently truncated
/// output, and a `collectOutput: false` caller sees as no output at all.
///
/// Sends stay non-blocking, so a chatty command is not throttled by a JS
/// round-trip per chunk; the barrier at the end restores the guarantee that a
/// resolved promise means every chunk has been handed over.
struct ChunkStream {
    callback: ChunkCallback,
    progress: Arc<(Mutex<Progress>, Condvar)>,
}

#[derive(Default)]
struct Progress {
    /// Dispatched but not yet run on the JS thread.
    pending: usize,
    /// Monotonic count of deliveries, so the barrier can tell "stalled" from
    /// "slow".
    delivered: u64,
}

impl ChunkStream {
    fn new(callback: ChunkCallback) -> Self {
        Self {
            callback,
            progress: Arc::new((Mutex::new(Progress::default()), Condvar::new())),
        }
    }

    fn send(&self, chunk: proto::BashChunk) {
        {
            let (state, _) = &*self.progress;
            state.lock().unwrap().pending += 1;
        }
        let progress = Arc::clone(&self.progress);
        let status = self.callback.call_with_return_value(
            chunk.into(),
            ThreadsafeFunctionCallMode::NonBlocking,
            move |_returned, _env| {
                settle(&progress);
                Ok(())
            },
        );
        if status != Status::Ok {
            // The completion callback will never run — most likely the
            // function is closing because the environment is shutting down —
            // so release the slot here or the barrier would wait it out.
            settle(&self.progress);
        }
    }

    /// Block until every dispatched chunk has run on the JS thread.
    ///
    /// Returns false if the consumer stalled and the wait was abandoned, which
    /// the caller reports rather than hiding.
    fn drain(&self) -> bool {
        let (state, delivered) = &*self.progress;
        let mut guard = state.lock().unwrap();
        let mut last_delivered = guard.delivered;
        let mut idle_since = Instant::now();
        while guard.pending > 0 {
            let remaining = CHUNK_DELIVERY_IDLE_LIMIT.saturating_sub(idle_since.elapsed());
            if remaining.is_zero() {
                return false;
            }
            let (next, _) = delivered.wait_timeout(guard, remaining).unwrap();
            guard = next;
            if guard.delivered != last_delivered {
                last_delivered = guard.delivered;
                idle_since = Instant::now();
            }
        }
        true
    }
}

fn settle(progress: &Arc<(Mutex<Progress>, Condvar)>) {
    let (state, delivered) = &**progress;
    let mut guard = state.lock().unwrap();
    guard.pending = guard.pending.saturating_sub(1);
    guard.delivered += 1;
    drop(guard);
    delivered.notify_all();
}

/// A resident Hearth engine: shared warm caches, warm shells, and profiler.
///
/// Construct one per process and keep it; the caches only pay off while it
/// lives. Dropping it stops the optimizer and kills every pooled shell.
#[napi]
pub struct HearthEngine {
    engine: Engine,
}

#[napi]
impl HearthEngine {
    #[napi(constructor)]
    pub fn new(options: Option<EngineOptions>) -> Result<Self> {
        // A cdylib gets no Rust runtime init, so SIGPIPE keeps its default
        // "terminate the process" disposition. A pooled shell that dies while
        // Hearth is writing its script would then take the host process down.
        // SAFETY: setting a signal disposition at load time, before any thread
        // depends on it.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };

        let mut cfg = EngineConfig::default();
        if let Some(o) = options {
            if let Some(cwd) = o.cwd {
                cfg.default_cwd = cwd.into();
            }
            if let Some(v) = o.walk_threads {
                cfg.walk_threads = (v as usize).max(1);
            }
            if let Some(v) = o.enable_watch {
                cfg.enable_watch = v;
            }
            if let Some(v) = o.enable_optimizer {
                cfg.enable_optimizer = v;
            }
            if let Some(v) = o.trust_cache {
                cfg.trust_cache = v;
            }
            if let Some(v) = o.warm_shell {
                cfg.warm_shell = v;
            }
            if let Some(v) = o.max_cached_files {
                cfg.max_cached_files = v as usize;
            }
            if let Some(v) = o.bash_timeout_ms {
                cfg.bash_timeout_ms = v.max(1) as u64;
            }
            if let Some(shell) = o.shell {
                cfg.shell = Some(shell.into());
            }
        }
        Ok(Self {
            engine: Engine::new(cfg),
        })
    }

    /// Enable the profiler (timing + allocation tracking).
    #[napi]
    pub fn enable_profiler(&self) {
        hearth_core::profiler::global_profiler().enable();
    }

    /// The profiler report as text.
    #[napi]
    pub fn stats(&self) -> String {
        self.engine.profiler_report()
    }

    // -- read ------------------------------------------------------------

    #[napi]
    pub fn read(&self, env: Env, params: ReadParams) -> Result<ReadResult> {
        hearth_tools::read(&self.engine, &params.into())
            .map(Into::into)
            .map_err(|e| structured_err(&env, e))
    }

    #[napi(
        ts_args_type = "params: ReadParams, signal?: AbortSignal",
        ts_return_type = "Promise<ReadResult>"
    )]
    pub fn read_async(&self, params: ReadParams, signal: Option<Abort>) -> AsyncTask<ReadTask> {
        let (cancel, signal) = split(signal);
        AsyncTask::with_optional_signal(
            ReadTask {
                engine: self.engine.clone(),
                params: params.into(),
                cancel,
                failure: None,
            },
            signal,
        )
    }

    /// Read a file's raw bytes as a Node `Buffer` — binary-safe, and it skips
    /// the UTF-8 string construction `read` does.
    #[napi]
    pub fn read_bytes(&self, env: Env, params: ReadParams) -> Result<Buffer> {
        hearth_tools::read_bytes(&self.engine, &params.into())
            .map(Buffer::from)
            .map_err(|e| structured_err(&env, e))
    }

    #[napi(
        ts_args_type = "params: ReadParams, signal?: AbortSignal",
        ts_return_type = "Promise<Buffer>"
    )]
    pub fn read_bytes_async(
        &self,
        params: ReadParams,
        signal: Option<Abort>,
    ) -> AsyncTask<ReadBytesTask> {
        let (cancel, signal) = split(signal);
        AsyncTask::with_optional_signal(
            ReadBytesTask {
                engine: self.engine.clone(),
                params: params.into(),
                cancel,
                failure: None,
            },
            signal,
        )
    }

    // -- write -----------------------------------------------------------

    #[napi]
    pub fn write(&self, env: Env, params: WriteParams) -> Result<WriteResult> {
        hearth_tools::write(&self.engine, &params.into())
            .map(Into::into)
            .map_err(|e| structured_err(&env, e))
    }

    /// Fast write: `path` and `content` are passed directly, so the string is
    /// **moved** into the cache — one fewer full-content copy than `write`.
    #[napi]
    pub fn write_fast(&self, env: Env, path: String, content: String) -> Result<WriteResult> {
        hearth_tools::write_owned(&self.engine, &path, content, true)
            .map(Into::into)
            .map_err(|e| structured_err(&env, e))
    }

    #[napi(
        ts_args_type = "params: WriteParams, signal?: AbortSignal",
        ts_return_type = "Promise<WriteResult>"
    )]
    pub fn write_async(&self, params: WriteParams, signal: Option<Abort>) -> AsyncTask<WriteTask> {
        let (cancel, signal) = split(signal);
        AsyncTask::with_optional_signal(
            WriteTask {
                engine: self.engine.clone(),
                params: params.into(),
                cancel,
                failure: None,
            },
            signal,
        )
    }

    // -- edit ------------------------------------------------------------

    #[napi]
    pub fn edit(&self, env: Env, params: EditParams) -> Result<EditResult> {
        hearth_tools::edit(&self.engine, &params.into())
            .map(Into::into)
            .map_err(|e| structured_err(&env, e))
    }

    /// Apply several disjoint replacements to one file in one atomic commit.
    /// Each `oldText` is matched against the original file, not against the
    /// result of an earlier edit in the same call.
    #[napi]
    pub fn edit_batch(&self, env: Env, params: EditBatchParams) -> Result<EditBatchResult> {
        hearth_tools::edit_batch(&self.engine, &params.into())
            .map(Into::into)
            .map_err(|e| structured_err(&env, e))
    }

    #[napi(
        ts_args_type = "params: EditBatchParams, signal?: AbortSignal",
        ts_return_type = "Promise<EditBatchResult>"
    )]
    pub fn edit_batch_async(
        &self,
        params: EditBatchParams,
        signal: Option<Abort>,
    ) -> AsyncTask<EditBatchTask> {
        let (cancel, signal) = split(signal);
        AsyncTask::with_optional_signal(
            EditBatchTask {
                engine: self.engine.clone(),
                params: params.into(),
                cancel,
                failure: None,
            },
            signal,
        )
    }

    // -- grep ------------------------------------------------------------

    /// Synchronous grep. Prefer `grepAsync` for large trees.
    #[napi]
    pub fn grep(&self, env: Env, params: GrepParams) -> Result<GrepResult> {
        hearth_tools::grep(&self.engine, &params.into())
            .map(Into::into)
            .map_err(|e| structured_err(&env, e))
    }

    #[napi(
        ts_args_type = "params: GrepParams, signal?: AbortSignal",
        ts_return_type = "Promise<GrepResult>"
    )]
    pub fn grep_async(&self, params: GrepParams, signal: Option<Abort>) -> AsyncTask<GrepTask> {
        let (cancel, signal) = split(signal);
        AsyncTask::with_optional_signal(
            GrepTask {
                engine: self.engine.clone(),
                params: params.into(),
                cancel,
                failure: None,
            },
            signal,
        )
    }

    // -- find ------------------------------------------------------------

    /// Synchronous glob discovery. Prefer `findAsync` for cold large trees.
    #[napi]
    pub fn find(&self, env: Env, params: FindParams) -> Result<FindResult> {
        let params = proto::FindParams::try_from(params).map_err(|e| structured_err(&env, e))?;
        hearth_tools::find(&self.engine, &params)
            .map(Into::into)
            .map_err(|e| structured_err(&env, e))
    }

    #[napi(
        ts_args_type = "params: FindParams, signal?: AbortSignal",
        ts_return_type = "Promise<FindResult>"
    )]
    pub fn find_async(
        &self,
        env: Env,
        params: FindParams,
        signal: Option<Abort>,
    ) -> Result<AsyncTask<FindTask>> {
        let params = proto::FindParams::try_from(params).map_err(|e| structured_err(&env, e))?;
        let (cancel, signal) = split(signal);
        Ok(AsyncTask::with_optional_signal(
            FindTask {
                engine: self.engine.clone(),
                params,
                cancel,
                failure: None,
            },
            signal,
        ))
    }

    // -- graph -----------------------------------------------------------

    /// Warm explicit seed files and their resolved direct in-root imports.
    #[napi]
    pub fn graph_prefetch(
        &self,
        env: Env,
        params: GraphPrefetchParams,
    ) -> Result<GraphPrefetchResult> {
        let params =
            proto::GraphPrefetchParams::try_from(params).map_err(|e| structured_err(&env, e))?;
        hearth_tools::graph_prefetch(&self.engine, &params)
            .map(Into::into)
            .map_err(|e| structured_err(&env, e))
    }

    /// Asynchronously warm explicit seeds and their direct imports.
    #[napi(
        ts_args_type = "params: GraphPrefetchParams, signal?: AbortSignal",
        ts_return_type = "Promise<GraphPrefetchResult>"
    )]
    pub fn graph_prefetch_async(
        &self,
        env: Env,
        params: GraphPrefetchParams,
        signal: Option<Abort>,
    ) -> Result<AsyncTask<GraphPrefetchTask>> {
        let params =
            proto::GraphPrefetchParams::try_from(params).map_err(|e| structured_err(&env, e))?;
        let (cancel, signal) = split(signal);
        Ok(AsyncTask::with_optional_signal(
            GraphPrefetchTask {
                engine: self.engine.clone(),
                params,
                cancel,
                failure: None,
            },
            signal,
        ))
    }

    /// Synchronously list symbols extracted from one file.
    #[napi]
    pub fn graph_symbols(&self, env: Env, params: GraphSymbolsParams) -> Result<GraphResult> {
        hearth_tools::graph(&self.engine, &params.into())
            .map(Into::into)
            .map_err(|e| structured_err(&env, e))
    }

    /// Synchronously return the nested symbol outline for one file.
    #[napi]
    pub fn graph_outline(&self, env: Env, params: GraphOutlineParams) -> Result<GraphResult> {
        hearth_tools::graph(&self.engine, &params.into())
            .map(Into::into)
            .map_err(|e| structured_err(&env, e))
    }

    /// Synchronously search indexed symbols by name.
    #[napi]
    pub fn graph_search(&self, env: Env, params: GraphSearchParams) -> Result<GraphResult> {
        hearth_tools::graph(&self.engine, &params.into())
            .map(Into::into)
            .map_err(|e| structured_err(&env, e))
    }

    /// Synchronously find definitions with an exact name.
    #[napi]
    pub fn graph_definitions(
        &self,
        env: Env,
        params: GraphDefinitionsParams,
    ) -> Result<GraphResult> {
        hearth_tools::graph(&self.engine, &params.into())
            .map(Into::into)
            .map_err(|e| structured_err(&env, e))
    }

    /// Synchronously traverse dependencies from one file.
    #[napi]
    pub fn graph_deps(&self, env: Env, params: GraphDepsParams) -> Result<GraphResult> {
        hearth_tools::graph(&self.engine, &params.into())
            .map(Into::into)
            .map_err(|e| structured_err(&env, e))
    }

    /// Synchronously traverse reverse dependencies from one file.
    #[napi]
    pub fn graph_rdeps(&self, env: Env, params: GraphRdepsParams) -> Result<GraphResult> {
        hearth_tools::graph(&self.engine, &params.into())
            .map(Into::into)
            .map_err(|e| structured_err(&env, e))
    }

    /// Synchronously traverse both directions around one file.
    #[napi]
    pub fn graph_neighborhood(
        &self,
        env: Env,
        params: GraphNeighborhoodParams,
    ) -> Result<GraphResult> {
        hearth_tools::graph(&self.engine, &params.into())
            .map(Into::into)
            .map_err(|e| structured_err(&env, e))
    }

    /// Synchronously return graph build and coverage status.
    #[napi]
    pub fn graph_status(&self, env: Env, params: GraphStatusParams) -> Result<GraphResult> {
        hearth_tools::graph(&self.engine, &params.into())
            .map(Into::into)
            .map_err(|e| structured_err(&env, e))
    }

    /// Asynchronously list symbols extracted from one file.
    #[napi(
        ts_args_type = "params: GraphSymbolsParams, signal?: AbortSignal",
        ts_return_type = "Promise<GraphResult>"
    )]
    pub fn graph_symbols_async(
        &self,
        params: GraphSymbolsParams,
        signal: Option<Abort>,
    ) -> AsyncTask<GraphTask> {
        let (cancel, signal) = split(signal);
        AsyncTask::with_optional_signal(
            GraphTask {
                engine: self.engine.clone(),
                params: params.into(),
                cancel,
                failure: None,
            },
            signal,
        )
    }

    /// Asynchronously return the nested symbol outline for one file.
    #[napi(
        ts_args_type = "params: GraphOutlineParams, signal?: AbortSignal",
        ts_return_type = "Promise<GraphResult>"
    )]
    pub fn graph_outline_async(
        &self,
        params: GraphOutlineParams,
        signal: Option<Abort>,
    ) -> AsyncTask<GraphTask> {
        let (cancel, signal) = split(signal);
        AsyncTask::with_optional_signal(
            GraphTask {
                engine: self.engine.clone(),
                params: params.into(),
                cancel,
                failure: None,
            },
            signal,
        )
    }

    /// Asynchronously search indexed symbols by name.
    #[napi(
        ts_args_type = "params: GraphSearchParams, signal?: AbortSignal",
        ts_return_type = "Promise<GraphResult>"
    )]
    pub fn graph_search_async(
        &self,
        params: GraphSearchParams,
        signal: Option<Abort>,
    ) -> AsyncTask<GraphTask> {
        let (cancel, signal) = split(signal);
        AsyncTask::with_optional_signal(
            GraphTask {
                engine: self.engine.clone(),
                params: params.into(),
                cancel,
                failure: None,
            },
            signal,
        )
    }

    /// Asynchronously find definitions with an exact name.
    #[napi(
        ts_args_type = "params: GraphDefinitionsParams, signal?: AbortSignal",
        ts_return_type = "Promise<GraphResult>"
    )]
    pub fn graph_definitions_async(
        &self,
        params: GraphDefinitionsParams,
        signal: Option<Abort>,
    ) -> AsyncTask<GraphTask> {
        let (cancel, signal) = split(signal);
        AsyncTask::with_optional_signal(
            GraphTask {
                engine: self.engine.clone(),
                params: params.into(),
                cancel,
                failure: None,
            },
            signal,
        )
    }

    /// Asynchronously traverse dependencies from one file.
    #[napi(
        ts_args_type = "params: GraphDepsParams, signal?: AbortSignal",
        ts_return_type = "Promise<GraphResult>"
    )]
    pub fn graph_deps_async(
        &self,
        params: GraphDepsParams,
        signal: Option<Abort>,
    ) -> AsyncTask<GraphTask> {
        let (cancel, signal) = split(signal);
        AsyncTask::with_optional_signal(
            GraphTask {
                engine: self.engine.clone(),
                params: params.into(),
                cancel,
                failure: None,
            },
            signal,
        )
    }

    /// Asynchronously traverse reverse dependencies from one file.
    #[napi(
        ts_args_type = "params: GraphRdepsParams, signal?: AbortSignal",
        ts_return_type = "Promise<GraphResult>"
    )]
    pub fn graph_rdeps_async(
        &self,
        params: GraphRdepsParams,
        signal: Option<Abort>,
    ) -> AsyncTask<GraphTask> {
        let (cancel, signal) = split(signal);
        AsyncTask::with_optional_signal(
            GraphTask {
                engine: self.engine.clone(),
                params: params.into(),
                cancel,
                failure: None,
            },
            signal,
        )
    }

    /// Asynchronously traverse both directions around one file.
    #[napi(
        ts_args_type = "params: GraphNeighborhoodParams, signal?: AbortSignal",
        ts_return_type = "Promise<GraphResult>"
    )]
    pub fn graph_neighborhood_async(
        &self,
        params: GraphNeighborhoodParams,
        signal: Option<Abort>,
    ) -> AsyncTask<GraphTask> {
        let (cancel, signal) = split(signal);
        AsyncTask::with_optional_signal(
            GraphTask {
                engine: self.engine.clone(),
                params: params.into(),
                cancel,
                failure: None,
            },
            signal,
        )
    }

    /// Asynchronously return graph build and coverage status.
    #[napi(
        ts_args_type = "params: GraphStatusParams, signal?: AbortSignal",
        ts_return_type = "Promise<GraphResult>"
    )]
    pub fn graph_status_async(
        &self,
        params: GraphStatusParams,
        signal: Option<Abort>,
    ) -> AsyncTask<GraphTask> {
        let (cancel, signal) = split(signal);
        AsyncTask::with_optional_signal(
            GraphTask {
                engine: self.engine.clone(),
                params: params.into(),
                cancel,
                failure: None,
            },
            signal,
        )
    }

    // -- bash ------------------------------------------------------------

    /// Synchronous bash. Blocks the event loop for the command's whole
    /// duration; prefer `bashAsync` or `bashStream`.
    #[napi]
    pub fn bash(&self, env: Env, params: BashParams) -> Result<BashResult> {
        hearth_tools::bash(&self.engine, &params.into())
            .map(Into::into)
            .map_err(|e| structured_err(&env, e))
    }

    #[napi(
        ts_args_type = "params: BashParams, signal?: AbortSignal",
        ts_return_type = "Promise<BashResult>"
    )]
    pub fn bash_async(&self, params: BashParams, signal: Option<Abort>) -> AsyncTask<BashTask> {
        let (cancel, signal) = split(signal);
        AsyncTask::with_optional_signal(
            BashTask {
                engine: self.engine.clone(),
                params: params.into(),
                cancel,
                on_chunk: None,
                failure: None,
            },
            signal,
        )
    }

    /// Run a command, delivering ordered output chunks to `onChunk` while it
    /// runs. A timeout or an abort still resolves, with the partial output and
    /// `timedOut`/`aborted` set, so a caller keeps what it already rendered.
    #[napi(
        ts_args_type = "params: BashParams, onChunk: (chunk: BashChunk) => void, signal?: AbortSignal",
        ts_return_type = "Promise<BashResult>"
    )]
    pub fn bash_stream(
        &self,
        params: BashParams,
        on_chunk: ChunkCallback,
        signal: Option<Abort>,
    ) -> AsyncTask<BashTask> {
        let (cancel, signal) = split(signal);
        AsyncTask::with_optional_signal(
            BashTask {
                engine: self.engine.clone(),
                params: params.into(),
                cancel,
                on_chunk: Some(ChunkStream::new(on_chunk)),
                failure: None,
            },
            signal,
        )
    }

    // -- cache invalidation ----------------------------------------------

    /// Drop `path` from the file cache and any walk that could have enumerated
    /// it. Use after a mutation Hearth did not perform itself.
    #[napi]
    pub fn invalidate_path(&self, path: String) -> InvalidateResult {
        self.engine.invalidate_path(Path::new(&path)).into()
    }

    /// Drop everything cached at or beneath `root`.
    ///
    /// This is the conservative hammer to reach for after a shell command: an
    /// arbitrary command can create, delete, rename or rewrite anything under
    /// its cwd, and no cheaper invalidation is sound.
    #[napi]
    pub fn invalidate_root(&self, root: String) -> InvalidateResult {
        self.engine.invalidate_root(Path::new(&root)).into()
    }

    /// The scoped form: pick recursion and which caches to touch.
    #[napi]
    pub fn invalidate(
        &self,
        path: String,
        recursive: Option<bool>,
        scope: Option<CacheScope>,
    ) -> InvalidateResult {
        self.engine
            .invalidate(
                Path::new(&path),
                recursive.unwrap_or(false),
                scope.map(Into::into).unwrap_or(proto::CacheScope::All),
            )
            .into()
    }

    /// Drop every cached file and walk.
    #[napi]
    pub fn clear_caches(&self) -> InvalidateResult {
        self.engine.clear_caches().into()
    }
}

// ---------------------------------------------------------------------------
// worker-thread tasks
// ---------------------------------------------------------------------------

/// Generate the boilerplate for a task that runs one cancellable tool call on a
/// libuv worker thread and resolves to a typed object.
macro_rules! tool_task {
    ($task:ident, $params:ty, $native:ty, $js:ty, $call:path) => {
        pub struct $task {
            engine: Engine,
            params: $params,
            cancel: CancelToken,
            /// The structured failure, kept for `reject` on the JS thread.
            failure: Option<proto::ToolError>,
        }

        #[napi]
        impl Task for $task {
            type Output = $native;
            type JsValue = $js;

            fn compute(&mut self) -> Result<Self::Output> {
                $call(&self.engine, &self.params, &self.cancel)
                    .map_err(|e| stash(&mut self.failure, e))
            }

            fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
                Ok(output.into())
            }

            fn reject(&mut self, env: Env, err: Error) -> Result<Self::JsValue> {
                match self.failure.take() {
                    Some(e) => Err(structured_err(&env, e)),
                    None => Err(err),
                }
            }
        }
    };
}

tool_task!(
    ReadTask,
    proto::ReadParams,
    proto::ReadResult,
    ReadResult,
    hearth_tools::read_cancellable
);
tool_task!(
    WriteTask,
    proto::WriteParams,
    proto::WriteResult,
    WriteResult,
    hearth_tools::write_cancellable
);
tool_task!(
    EditBatchTask,
    proto::EditBatchParams,
    proto::EditBatchResult,
    EditBatchResult,
    hearth_tools::edit_batch_cancellable
);
tool_task!(
    GrepTask,
    proto::GrepParams,
    proto::GrepResult,
    GrepResult,
    hearth_tools::grep_cancellable
);
tool_task!(
    FindTask,
    proto::FindParams,
    proto::FindResult,
    FindResult,
    hearth_tools::find_cancellable
);
tool_task!(
    GraphPrefetchTask,
    proto::GraphPrefetchParams,
    proto::GraphPrefetchResult,
    GraphPrefetchResult,
    hearth_tools::graph_prefetch_cancellable
);
tool_task!(
    GraphTask,
    proto::GraphParams,
    proto::GraphResult,
    GraphResult,
    hearth_tools::graph_cancellable
);

/// `readBytes` resolves to a `Buffer`, so it does not fit the macro's
/// `Output: Into<JsValue>` shape.
pub struct ReadBytesTask {
    engine: Engine,
    params: proto::ReadParams,
    cancel: CancelToken,
    /// The structured failure, kept for `reject` on the JS thread.
    failure: Option<proto::ToolError>,
}

#[napi]
impl Task for ReadBytesTask {
    type Output = Vec<u8>;
    type JsValue = Buffer;

    fn compute(&mut self) -> Result<Self::Output> {
        hearth_tools::read_bytes_cancellable(&self.engine, &self.params, &self.cancel)
            .map_err(|e| stash(&mut self.failure, e))
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(Buffer::from(output))
    }

    fn reject(&mut self, env: Env, err: Error) -> Result<Self::JsValue> {
        match self.failure.take() {
            Some(e) => Err(structured_err(&env, e)),
            None => Err(err),
        }
    }
}

/// `bash` optionally streams, so it carries the callback alongside its params.
pub struct BashTask {
    engine: Engine,
    params: proto::BashParams,
    cancel: CancelToken,
    on_chunk: Option<ChunkStream>,
    /// The structured failure, kept for `reject` on the JS thread.
    failure: Option<proto::ToolError>,
}

#[napi]
impl Task for BashTask {
    type Output = proto::BashResult;
    type JsValue = BashResult;

    fn compute(&mut self) -> Result<Self::Output> {
        let result = match &self.on_chunk {
            Some(stream) => {
                let result = hearth_tools::bash_stream(
                    &self.engine,
                    &self.params,
                    &self.cancel,
                    &mut |chunk| stream.send(chunk),
                );
                // Do not let the promise settle ahead of the chunks it
                // describes. `result` is held first so a command failure is
                // still reported even if the consumer then stalls.
                if !stream.drain() && result.is_ok() {
                    Err(proto::ToolError::internal(
                        "the chunk callback stopped accepting output",
                    ))
                } else {
                    result
                }
            }
            None => hearth_tools::bash_cancellable(&self.engine, &self.params, &self.cancel),
        };
        result.map_err(|e| stash(&mut self.failure, e))
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output.into())
    }

    fn reject(&mut self, env: Env, err: Error) -> Result<Self::JsValue> {
        match self.failure.take() {
            Some(e) => Err(structured_err(&env, e)),
            None => Err(err),
        }
    }
}
