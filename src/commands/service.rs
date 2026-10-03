//! `service status`: diagnose the running opencode service.

use crate::db::{env_status, EnvStatus};
use crate::error::Result;
use crate::output;
use crate::service;
use serde::Serialize;
use std::path::Path;

#[derive(Serialize)]
struct ServiceStatusOut {
    #[serde(flatten)]
    env: EnvStatus,
    registered: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    /// Whether the service has this exact database file open (Linux only).
    #[serde(skip_serializing_if = "Option::is_none")]
    db_matches: Option<bool>,
    /// `true` when `GET /api/info` answered with valid credentials.
    #[serde(skip_serializing_if = "Option::is_none")]
    api_ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

pub fn cmd_service_status(db_path: &Path) -> Result<()> {
    let mut out = ServiceStatusOut {
        env: env_status(db_path),
        registered: false,
        pid: None,
        url: None,
        version: None,
        db_matches: None,
        api_ok: None,
        api_error: None,
        note: None,
    };
    let Some(info) = service::discover() else {
        out.note = Some(
            "no registered opencode service (service.json / server.json missing or stale)"
                .to_string(),
        );
        return output::emit(&serde_json::to_value(&out)?);
    };

    out.registered = true;
    out.pid = Some(info.pid);
    out.url = Some(info.url.clone());
    out.version = info.version.clone();
    out.db_matches = info.db_match(db_path);
    match info.api_info() {
        Ok(value) => {
            out.api_ok = Some(true);
            if let Some(version) = value.get("version").and_then(|v| v.as_str()) {
                out.version = Some(version.to_string());
            }
        }
        Err(e) => {
            out.api_ok = Some(false);
            out.api_error = Some(e.to_string());
        }
    }

    out.note = match out.db_matches {
        Some(false) => Some(
            "the service is not using this database; API-routed deletes are disabled".to_string(),
        ),
        None => Some(
            "database match cannot be verified on this platform (Linux only); \
             API-routed deletes are disabled"
                .to_string(),
        ),
        _ if out.api_ok == Some(false) => Some(
            "the API did not answer; deletes will fall back to the running-instance guard"
                .to_string(),
        ),
        _ => None,
    };
    output::emit(&serde_json::to_value(&out)?)
}
