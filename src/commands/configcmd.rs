//! `config`: show effective settings and resolved paths.

use crate::cli::ConfigCmd;
use crate::error::Result;
use crate::output;
use crate::settings::Settings;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Serialize)]
struct ConfigShowOut {
    #[serde(skip_serializing_if = "Option::is_none")]
    config_file: Option<String>,
    config_file_exists: bool,
    source: &'static str,
    settings: Settings,
}

#[derive(Serialize)]
struct ConfigPathOut {
    #[serde(skip_serializing_if = "Option::is_none")]
    config_file: Option<String>,
    config_file_exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    db: Option<String>,
    db_exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    db_error: Option<String>,
    env: serde_json::Map<String, serde_json::Value>,
}

pub fn cmd_config(cmd: &ConfigCmd, settings: &Settings, explicit: Option<&Path>) -> Result<()> {
    match cmd {
        ConfigCmd::Show => cmd_show(settings, explicit),
        ConfigCmd::Path => cmd_path(explicit),
    }
}

/// Explicit `--config` wins over the standard location.
fn config_file(explicit: Option<&Path>) -> Option<PathBuf> {
    explicit
        .map(Path::to_path_buf)
        .or_else(crate::settings::default_path)
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn cmd_show(settings: &Settings, explicit: Option<&Path>) -> Result<()> {
    let path = config_file(explicit);
    let exists = path.as_ref().map(|p| p.is_file()).unwrap_or(false);
    let out = ConfigShowOut {
        config_file: path.as_ref().map(|p| path_string(p)),
        config_file_exists: exists,
        source: if exists { "file" } else { "defaults" },
        settings: settings.clone(),
    };
    if output::table_mode() {
        let mut text = format!(
            "config  {}\nsource  {}\n\n",
            out.config_file.as_deref().unwrap_or("(none)"),
            out.source
        );
        text.push_str(&output::render_value_text(&serde_json::to_value(
            &out.settings,
        )?));
        return output::emit_text(&text);
    }
    output::emit(&serde_json::to_value(&out)?)
}

fn cmd_path(explicit: Option<&Path>) -> Result<()> {
    let path = config_file(explicit);
    let db_result = crate::config::db_path();
    let (db, db_error) = match &db_result {
        Ok(p) => (Some(path_string(p)), None),
        Err(e) => (None, Some(e.message.clone())),
    };
    let mut env = serde_json::Map::new();
    for name in [
        "OPENCODE_DB",
        "OPENCODE_DATA_DIR",
        "OPENCODE_DBTOOL_CONFIG",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
    ] {
        if let Ok(value) = std::env::var(name) {
            if !value.is_empty() {
                env.insert(name.to_string(), serde_json::Value::String(value));
            }
        }
    }
    let out = ConfigPathOut {
        config_file: path.as_ref().map(|p| path_string(p)),
        config_file_exists: path.as_ref().map(|p| p.is_file()).unwrap_or(false),
        data_dir: crate::config::data_dir().map(|p| path_string(&p)),
        state_dir: crate::config::state_dir().map(|p| path_string(&p)),
        db_exists: db_result.as_ref().map(|p| p.exists()).unwrap_or(false),
        db,
        db_error,
        env,
    };
    if output::table_mode() {
        let mut lines: Vec<(&str, String)> = vec![
            (
                "config",
                out.config_file.clone().unwrap_or_else(|| "(none)".into()),
            ),
            (
                "data",
                out.data_dir.clone().unwrap_or_else(|| "(none)".into()),
            ),
            (
                "state",
                out.state_dir.clone().unwrap_or_else(|| "(none)".into()),
            ),
            (
                "db",
                out.db
                    .clone()
                    .or_else(|| out.db_error.clone())
                    .unwrap_or_else(|| "(none)".into()),
            ),
        ];
        if !out.env.is_empty() {
            let vars: Vec<String> = out
                .env
                .iter()
                .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
                .collect();
            lines.push(("env", vars.join(" ")));
        }
        let width = lines.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
        let text = lines
            .iter()
            .map(|(k, v)| format!("{k:width$}  {v}"))
            .collect::<Vec<_>>()
            .join("\n");
        return output::emit_text(&text);
    }
    output::emit(&serde_json::to_value(&out)?)
}
