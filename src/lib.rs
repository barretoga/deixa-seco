//! deixa-seco: núcleo (árvore, varredura, volumes, treemap) e a interface em `ui`.

pub mod cleanup;
pub mod elevation;
pub mod format;
pub mod insights;
pub mod kind;
pub mod scan;
pub mod shell;
pub mod tree;
pub mod treemap;
pub mod ui;
pub mod volumes;

pub use elevation::parse_args;
