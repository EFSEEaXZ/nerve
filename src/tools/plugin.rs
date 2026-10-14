//! `plugin`: list and manage plugins (hot-reloaded tool providers in <data>/plugins).

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::mcp::{OrErr, ToolCtx, ToolDef, ToolOutput, need, tool};

#[derive(Deserialize, JsonSchema)]
pub struct PluginArgs {
    /// list (default) | reload | enable | disable | logs | new | dir
    action: Option<String>,
    /// Plugin name (reload/enable/disable/logs/new). reload without name = all.
    name: Option<String>,
    /// logs: number of last lines (default 60).
    lines: Option<usize>,
    /// new: one-line description of the plugin.
    description: Option<String>,
}

const DESC: &str = "Plugins = extra tools loaded from the plugins folder (bundled: blender, unreal). \
They hot-reload: editing/adding files in a plugin folder updates its tools within ~1 s, no restart. \
Actions: list (state, tools, errors); reload {name?}; enable/disable {name}; logs {name, lines?}; \
new {name, description?} (scaffold a Python plugin, then edit its server.py with fs); dir (folder + how to write one). \
A plugin = folder with plugin.toml + a program speaking MCP over stdio; Python plugins use `from nerve_plugin import Plugin` \
(@p.tool functions, schema from type hints + docstring Args, ctx.progress/ctx.nerve(tool, **args), return str/dict/Image(path)).";

pub fn def() -> ToolDef {
    tool::<PluginArgs, _, _>("plugin", "Plugins", DESC, json!({"destructiveHint": false, "openWorldHint": false}), run)
}

const TEMPLATE_TOML: &str = r#"name = "{name}"
version = "0.1.0"
description = "{desc}"
command = ["python", "-u", "server.py"]

[env]
"#;

const TEMPLATE_PY: &str = r#""""{name}: {desc}"""

from nerve_plugin import Image, Plugin, ToolError

p = Plugin("{name}", version="0.1.0")


@p.tool
def hello(who: str = "world", ctx=None) -> str:
    """Example tool: replace me.

    Args:
        who: whom to greet
    """
    ctx.progress("greeting...")
    return f"hello {who} from {name}"


if __name__ == "__main__":
    p.run()
"#;

async fn run(a: PluginArgs, ctx: ToolCtx) -> Result<ToolOutput, String> {
    let pl = &ctx.app.plugins;
    let action = a.action.as_deref().unwrap_or("list");
    match action {
        "list" | "status" => {
            let s = pl.summary();
            let mut text = format!("plugins in {} ({}):\n", pl.dir.display(), s.len());
            for p in &s {
                let tools: Vec<String> = p["tools"].as_array().map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect()).unwrap_or_default();
                text.push_str(&format!(
                    "- {} {} [{}] tools: {}{}\n",
                    p["name"].as_str().unwrap_or("?"),
                    p["version"].as_str().unwrap_or(""),
                    p["state"].as_str().unwrap_or("?"),
                    if tools.is_empty() { "-".into() } else { tools.join(", ") },
                    p["error"].as_str().map(|e| format!("\n  error: {}", e.replace('\n', "\n  "))).unwrap_or_default()
                ));
            }
            if s.is_empty() {
                text.push_str("(none) - create one with action new\n");
            }
            Ok(ToolOutput::text(text).with_structured(json!({"plugins": s})))
        }
        "reload" => {
            let name = a.name.as_deref().unwrap_or("*");
            pl.scan(Some(name)).await;
            let s: Vec<_> = pl.summary().into_iter().filter(|p| name == "*" || p["name"] == name).collect();
            if s.is_empty() {
                return Err(format!("no plugin `{name}`"));
            }
            let text = s
                .iter()
                .map(|p| format!("{}: {}{}", p["name"].as_str().unwrap_or(""), p["state"].as_str().unwrap_or(""), p["error"].as_str().map(|e| format!(" - {e}")).unwrap_or_default()))
                .collect::<Vec<_>>()
                .join("\n");
            Ok(ToolOutput::text(text))
        }
        "enable" | "disable" => {
            let name = need(a.name.as_deref(), "name")?;
            pl.set_enabled(name, action == "enable").await?;
            Ok(ToolOutput::text(format!("{name}: {action}d")))
        }
        "logs" | "log" => {
            let name = need(a.name.as_deref(), "name")?;
            Ok(ToolOutput::text(pl.log_tail(name, a.lines.unwrap_or(60)).await?))
        }
        "new" | "create" => {
            let name = need(a.name.as_deref(), "name")?;
            if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') || name.starts_with('_') {
                return Err("name: letters, digits, - and _ only (not starting with _)".into());
            }
            let dir = pl.dir.join(name);
            if dir.exists() {
                return Err(format!("{} already exists", dir.display()));
            }
            let desc = a.description.clone().unwrap_or_else(|| format!("{name} tools")).replace('"', "'");
            std::fs::create_dir_all(&dir).ctx("create plugin dir")?;
            std::fs::write(dir.join("server.py"), TEMPLATE_PY.replace("{name}", name).replace("{desc}", &desc)).ctx("write server.py")?;
            std::fs::write(dir.join("plugin.toml"), TEMPLATE_TOML.replace("{name}", name).replace("{desc}", &desc)).ctx("write plugin.toml")?;
            pl.scan(Some(name)).await;
            let st = pl.summary().into_iter().find(|p| p["name"] == name);
            Ok(ToolOutput::text(format!(
                "created {}\n  plugin.toml, server.py (example tool `{name}_hello`)\nstate: {}\nEdit server.py (fs edit/write): nerve reloads it automatically.",
                dir.display(),
                st.as_ref().and_then(|s| s["state"].as_str()).unwrap_or("?")
            )))
        }
        "dir" | "help" => Ok(ToolOutput::text(format!(
            "plugins folder: {}\nPython SDK: {}\\nerve_plugin.py (on PYTHONPATH)\nguide: {}\\README.md\nlogs: <nerve data>\\logs\\plugins\\<name>.log",
            pl.dir.display(),
            pl.sdk_dir().display(),
            pl.dir.display()
        ))),
        o => Err(super::unknown_action("plugin", o, "list, reload, enable, disable, logs, new, dir")),
    }
}
