
pub mod internal;
pub mod registry;

use internal::read_file::ReadFile;
use registry::Registry;

/// 注册所有内置工具到给定的 Registry 中。
pub fn register_builtins(registry: &Registry) {
    registry.add(Box::new(ReadFile::new()));
}
