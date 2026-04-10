#[allow(clippy::module_inception)]
pub mod vm;
mod builtins;
mod call;
mod json;
mod promise;
mod generator;
mod regexp;

pub use vm::{Vm, VmError};
