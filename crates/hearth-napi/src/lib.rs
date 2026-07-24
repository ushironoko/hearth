//! Node.js bindings for the Hearth tool orchestrator (napi-rs v3).
//!
//! Mirrors the corsa-bind pattern: one thin binding over the shared Rust core.
//! JSON is used at the N-API boundary so the Rust side keeps its typed tools
//! intact. Each tool has a synchronous method (fast, runs on the JS thread) and
//! an `*Async` twin that offloads to a libuv worker thread via `AsyncTask` —
//! no tokio reactor is embedded in the addon.
//!
//! The engine is an explicit object the caller constructs and holds; there is no
//! hidden global singleton, matching the "one resident engine per process" model
//! while keeping ownership in the caller's hands.

use hearth_core::{Engine, EngineConfig};
use hearth_proto::{BashParams, EditParams, GrepParams, ReadParams, WriteParams};
use napi::bindgen_prelude::*;
use napi_derive::napi;
use serde_json::Value;

#[cfg(not(feature = "profiling"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "profiling")]
#[global_allocator]
static GLOBAL: hearth_core::profiler::ProfilingAllocator<mimalloc::MiMalloc> =
    hearth_core::profiler::ProfilingAllocator::from_allocator(mimalloc::MiMalloc);

fn json_err(e: impl std::fmt::Display) -> Error {
    Error::from_reason(e.to_string())
}

/// A resident Hearth engine: shared warm caches, profiler, and self-optimizer.
#[napi]
pub struct HearthEngine {
    engine: Engine,
}

#[napi]
impl HearthEngine {
    /// Construct an engine. `options` is an optional object:
    /// `{ cwd?, walkThreads?, enableWatch?, enableOptimizer?, maxCachedFiles? }`.
    #[napi(constructor)]
    pub fn new(options: Option<Value>) -> Result<Self> {
        let mut cfg = EngineConfig::default();
        if let Some(Value::Object(o)) = options {
            if let Some(Value::String(cwd)) = o.get("cwd") {
                cfg.default_cwd = cwd.into();
            }
            if let Some(v) = o.get("walkThreads").and_then(Value::as_u64) {
                cfg.walk_threads = v as usize;
            }
            if let Some(v) = o.get("enableWatch").and_then(Value::as_bool) {
                cfg.enable_watch = v;
            }
            if let Some(v) = o.get("enableOptimizer").and_then(Value::as_bool) {
                cfg.enable_optimizer = v;
            }
            if let Some(v) = o.get("trustCache").and_then(Value::as_bool) {
                cfg.trust_cache = v;
            }
            if let Some(v) = o.get("warmShell").and_then(Value::as_bool) {
                cfg.warm_shell = v;
            }
            if let Some(v) = o.get("maxCachedFiles").and_then(Value::as_u64) {
                cfg.max_cached_files = v as usize;
            }
            if let Some(v) = o.get("bashTimeoutMs").and_then(Value::as_u64) {
                cfg.bash_timeout_ms = v;
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

    #[napi]
    pub fn read(&self, params: Value) -> Result<Value> {
        let p: ReadParams = serde_json::from_value(params).map_err(json_err)?;
        let r = hearth_tools::read(&self.engine, &p).map_err(json_err)?;
        serde_json::to_value(r).map_err(json_err)
    }

    #[napi]
    pub fn write(&self, params: Value) -> Result<Value> {
        let p: WriteParams = serde_json::from_value(params).map_err(json_err)?;
        let r = hearth_tools::write(&self.engine, &p).map_err(json_err)?;
        serde_json::to_value(r).map_err(json_err)
    }

    #[napi]
    pub fn edit(&self, params: Value) -> Result<Value> {
        let p: EditParams = serde_json::from_value(params).map_err(json_err)?;
        let r = hearth_tools::edit(&self.engine, &p).map_err(json_err)?;
        serde_json::to_value(r).map_err(json_err)
    }

    /// Synchronous grep. Prefer `grepAsync` for large trees.
    #[napi]
    pub fn grep(&self, params: Value) -> Result<Value> {
        let p: GrepParams = serde_json::from_value(params).map_err(json_err)?;
        let r = hearth_tools::grep(&self.engine, &p).map_err(json_err)?;
        serde_json::to_value(r).map_err(json_err)
    }

    /// Synchronous bash. Prefer `bashAsync` for long commands.
    #[napi]
    pub fn bash(&self, params: Value) -> Result<Value> {
        let p: BashParams = serde_json::from_value(params).map_err(json_err)?;
        let r = hearth_tools::bash(&self.engine, &p).map_err(json_err)?;
        serde_json::to_value(r).map_err(json_err)
    }

    /// Async grep: runs on a libuv worker thread, resolves to a JSON string
    /// (the JS wrapper `JSON.parse`s it). Keeps the event loop free for big trees.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn grep_async(&self, params: Value) -> AsyncTask<GrepTask> {
        AsyncTask::new(GrepTask { engine: self.engine.clone(), params })
    }

    /// Async bash: runs on a libuv worker thread, resolves to a JSON string.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn bash_async(&self, params: Value) -> AsyncTask<BashTask> {
        AsyncTask::new(BashTask { engine: self.engine.clone(), params })
    }
}

/// Worker-thread task for `grepAsync`. Returns a JSON string because
/// `serde_json::Value` does not implement napi's `TypeName` bound for async
/// results, whereas `String` does.
pub struct GrepTask {
    engine: Engine,
    params: Value,
}

#[napi]
impl Task for GrepTask {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> Result<Self::Output> {
        let p: GrepParams = serde_json::from_value(self.params.clone()).map_err(json_err)?;
        let r = hearth_tools::grep(&self.engine, &p).map_err(json_err)?;
        serde_json::to_string(&r).map_err(json_err)
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

/// Worker-thread task for `bashAsync`.
pub struct BashTask {
    engine: Engine,
    params: Value,
}

#[napi]
impl Task for BashTask {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> Result<Self::Output> {
        let p: BashParams = serde_json::from_value(self.params.clone()).map_err(json_err)?;
        let r = hearth_tools::bash(&self.engine, &p).map_err(json_err)?;
        serde_json::to_string(&r).map_err(json_err)
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}
