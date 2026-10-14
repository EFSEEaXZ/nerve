//! Built-in tools. Each module exposes `def() -> ToolDef`; grouped tools take an `action`.

pub mod browser;
pub mod exec;
pub mod fs;
pub mod image;
pub mod job;
pub mod net;
pub mod plugin;
pub mod sys;
pub mod ui;

use crate::mcp::Registry;

pub fn registry() -> Registry {
    Registry::new(vec![exec::def(), job::def(), fs::def(), image::def(), sys::def(), net::def(), ui::def(), browser::def(), plugin::def()])
}

/// Parse a comma/space separated or JSON-ish list of actions etc. (helper for messages)
pub fn unknown_action(tool: &str, action: &str, valid: &str) -> String {
    format!("unknown {tool} action `{action}`; valid: {valid}")
}
