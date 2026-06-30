#[allow(clippy::module_inception)]
pub mod vm;
mod builtins;
mod call;
mod embed;
mod json;
mod promise;
mod generator;
mod regexp;
mod map;
mod typedarray;

pub use vm::{Vm, VmError};
