#![allow(unpredictable_function_pointer_comparisons)]

pub mod state_agent;

/// 兼容层:旧的 `crate::agent::*` 路径指向新的 `state_agent` 实现。
/// 迁移完成后可移除。
pub mod agent {
    pub use crate::state_agent::*;
}

pub mod books;
pub mod book_chat;
pub mod db;
pub mod explain;
pub mod formula_ocr;
pub mod layout;
pub mod model;
pub mod onnx_runtime;
pub mod page_ocr;
pub mod pdf;
pub mod permission;
pub mod settings;
pub mod skills;
pub mod store;
pub mod tools;
pub mod translate;
pub mod ui;
