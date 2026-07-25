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
//! **Errors.** Every rejection's `message` starts with `"<kind>: "` — one of
//! `notFound`, `permission`, `noMatch`, `multipleMatches`, `overlap`,
//! `noChange`, `invalidInput`, `timeout`, `cancelled`, `indeterminate`, `io`,
//! `internal`. Synchronous methods additionally set that kind as the JS
//! `Error.code`; the async ones cannot, because napi fixes a worker task's
//! error type, which is why the message prefix is the format to branch on.

use hearth_core::{CancelToken, Engine, EngineConfig};
use hearth_proto as proto;
use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;
use std::path::Path;

mod types;
pub use types::*;

#[cfg(not(feature = "profiling"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "profiling")]
#[global_allocator]
static GLOBAL: hearth_core::profiler::ProfilingAllocator<mimalloc::MiMalloc> =
    hearth_core::profiler::ProfilingAllocator::from_allocator(mimalloc::MiMalloc);

/// Turn a tool error into a JS `Error` whose `code` is the stable error kind,
/// so a caller branches on `err.code === "cancelled"` rather than on prose.
fn tool_err(e: proto::ToolError) -> Error<String> {
    Error::new(e.kind.as_str().to_string(), format!("{}: {}", e.kind.as_str(), e.message))
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
        let already = object.get_named_property::<bool>("aborted").unwrap_or(false);
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
        Ok(Self { engine: Engine::new(cfg) })
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
    pub fn read(&self, params: ReadParams) -> Result<ReadResult, String> {
        hearth_tools::read(&self.engine, &params.into()).map(Into::into).map_err(tool_err)
    }

    #[napi(
        ts_args_type = "params: ReadParams, signal?: AbortSignal",
        ts_return_type = "Promise<ReadResult>"
    )]
    pub fn read_async(
        &self,
        params: ReadParams,
        signal: Option<Abort>,
    ) -> AsyncTask<ReadTask> {
        let (cancel, signal) = split(signal);
        AsyncTask::with_optional_signal(
            ReadTask { engine: self.engine.clone(), params: params.into(), cancel },
            signal,
        )
    }

    /// Read a file's raw bytes as a Node `Buffer` — binary-safe, and it skips
    /// the UTF-8 string construction `read` does.
    #[napi]
    pub fn read_bytes(&self, params: ReadParams) -> Result<Buffer, String> {
        hearth_tools::read_bytes(&self.engine, &params.into())
            .map(Buffer::from)
            .map_err(tool_err)
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
            ReadBytesTask { engine: self.engine.clone(), params: params.into(), cancel },
            signal,
        )
    }

    // -- write -----------------------------------------------------------

    #[napi]
    pub fn write(&self, params: WriteParams) -> Result<WriteResult, String> {
        hearth_tools::write(&self.engine, &params.into()).map(Into::into).map_err(tool_err)
    }

    /// Fast write: `path` and `content` are passed directly, so the string is
    /// **moved** into the cache — one fewer full-content copy than `write`.
    #[napi]
    pub fn write_fast(&self, path: String, content: String) -> Result<WriteResult, String> {
        hearth_tools::write_owned(&self.engine, &path, content, true)
            .map(Into::into)
            .map_err(tool_err)
    }

    #[napi(
        ts_args_type = "params: WriteParams, signal?: AbortSignal",
        ts_return_type = "Promise<WriteResult>"
    )]
    pub fn write_async(
        &self,
        params: WriteParams,
        signal: Option<Abort>,
    ) -> AsyncTask<WriteTask> {
        let (cancel, signal) = split(signal);
        AsyncTask::with_optional_signal(
            WriteTask { engine: self.engine.clone(), params: params.into(), cancel },
            signal,
        )
    }

    // -- edit ------------------------------------------------------------

    #[napi]
    pub fn edit(&self, params: EditParams) -> Result<EditResult, String> {
        hearth_tools::edit(&self.engine, &params.into()).map(Into::into).map_err(tool_err)
    }

    /// Apply several disjoint replacements to one file in one atomic commit.
    /// Each `oldText` is matched against the original file, not against the
    /// result of an earlier edit in the same call.
    #[napi]
    pub fn edit_batch(&self, params: EditBatchParams) -> Result<EditBatchResult, String> {
        hearth_tools::edit_batch(&self.engine, &params.into()).map(Into::into).map_err(tool_err)
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
            EditBatchTask { engine: self.engine.clone(), params: params.into(), cancel },
            signal,
        )
    }

    // -- grep ------------------------------------------------------------

    /// Synchronous grep. Prefer `grepAsync` for large trees.
    #[napi]
    pub fn grep(&self, params: GrepParams) -> Result<GrepResult, String> {
        hearth_tools::grep(&self.engine, &params.into()).map(Into::into).map_err(tool_err)
    }

    #[napi(
        ts_args_type = "params: GrepParams, signal?: AbortSignal",
        ts_return_type = "Promise<GrepResult>"
    )]
    pub fn grep_async(
        &self,
        params: GrepParams,
        signal: Option<Abort>,
    ) -> AsyncTask<GrepTask> {
        let (cancel, signal) = split(signal);
        AsyncTask::with_optional_signal(
            GrepTask { engine: self.engine.clone(), params: params.into(), cancel },
            signal,
        )
    }

    // -- bash ------------------------------------------------------------

    /// Synchronous bash. Blocks the event loop for the command's whole
    /// duration; prefer `bashAsync` or `bashStream`.
    #[napi]
    pub fn bash(&self, params: BashParams) -> Result<BashResult, String> {
        hearth_tools::bash(&self.engine, &params.into()).map(Into::into).map_err(tool_err)
    }

    #[napi(
        ts_args_type = "params: BashParams, signal?: AbortSignal",
        ts_return_type = "Promise<BashResult>"
    )]
    pub fn bash_async(
        &self,
        params: BashParams,
        signal: Option<Abort>,
    ) -> AsyncTask<BashTask> {
        let (cancel, signal) = split(signal);
        AsyncTask::with_optional_signal(
            BashTask { engine: self.engine.clone(), params: params.into(), cancel, on_chunk: None },
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
                on_chunk: Some(on_chunk),
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
        }

        #[napi]
        impl Task for $task {
            type Output = $native;
            type JsValue = $js;

            fn compute(&mut self) -> Result<Self::Output> {
                $call(&self.engine, &self.params, &self.cancel).map_err(task_err)
            }

            fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
                Ok(output.into())
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

/// `readBytes` resolves to a `Buffer`, so it does not fit the macro's
/// `Output: Into<JsValue>` shape.
pub struct ReadBytesTask {
    engine: Engine,
    params: proto::ReadParams,
    cancel: CancelToken,
}

#[napi]
impl Task for ReadBytesTask {
    type Output = Vec<u8>;
    type JsValue = Buffer;

    fn compute(&mut self) -> Result<Self::Output> {
        hearth_tools::read_bytes_cancellable(&self.engine, &self.params, &self.cancel)
            .map_err(task_err)
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(Buffer::from(output))
    }
}

/// `bash` optionally streams, so it carries the callback alongside its params.
pub struct BashTask {
    engine: Engine,
    params: proto::BashParams,
    cancel: CancelToken,
    on_chunk: Option<ChunkCallback>,
}

#[napi]
impl Task for BashTask {
    type Output = proto::BashResult;
    type JsValue = BashResult;

    fn compute(&mut self) -> Result<Self::Output> {
        match &self.on_chunk {
            // Non-blocking: a slow JS consumer must not stall the pipe readers,
            // which is what a full pipe buffer would turn into a deadlock.
            Some(callback) => hearth_tools::bash_stream(
                &self.engine,
                &self.params,
                &self.cancel,
                &mut |chunk| {
                    callback.call(chunk.into(), ThreadsafeFunctionCallMode::NonBlocking);
                },
            ),
            None => hearth_tools::bash_cancellable(&self.engine, &self.params, &self.cancel),
        }
        .map_err(task_err)
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output.into())
    }
}

/// A worker-thread error.
///
/// `Task` pins the error type to `Error<Status>`, so unlike the synchronous
/// methods an async rejection cannot put the kind in `Error.code`. Both paths
/// therefore also lead the message with `"<kind>: "`, which is the one format a
/// caller has to know.
fn task_err(e: proto::ToolError) -> Error {
    Error::new(Status::GenericFailure, format!("{}: {}", e.kind.as_str(), e.message))
}
