// The previous contents of this file (console / Math / global function
// thunks built from `JsObject::function_native`) were dead code: the live
// VM bakes those built-ins in directly via sentinels in `vm/vm.rs`. The
// module is kept as a stub so embedders can plug in their own host
// objects through `Engine::register_host_fn` once the embedder API
// stabilises.
