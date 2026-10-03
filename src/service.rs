//! Running opencode service discovery and API access.
//!
//! The opencode 2.x server keeps running after the TUI exits and registers
//! itself in the state dir (`service.json`, or `server.json` in newer
//! builds) with its pid, URL, and password. Session deletes are routed
//! through that server when it operates on the same database file, so its
//! caches and event log stay consistent; everything else keeps using the
//! direct database path.

use crate::error::{AppError, Result};
use base64::Engine;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ServiceInfo {
    pub pid: i32,
    pub url: String,
    pub password: Option<String>,
}

#[derive(Deserialize)]
struct ServiceFile {
    pid: i32,
    url: String,
    #[serde(default)]
    password: Option<String>,
}

/// Discover a live registered service, if any.
pub fn discover() -> Option<ServiceInfo> {
    let dir = crate::config::state_dir()?;
    discover_in(&dir)
}

/// Pure discovery over a state directory (testable without env vars).
fn discover_in(dir: &Path) -> Option<ServiceInfo> {
    for name in ["service.json", "server.json"] {
        let path = dir.join(name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(file) = serde_json::from_str::<ServiceFile>(&text) else {
            continue;
        };
        if !process_alive(file.pid) {
            continue;
        }
        let password = file.password.or_else(|| {
            std::fs::read_to_string(dir.join("password"))
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        });
        return Some(ServiceInfo {
            pid: file.pid,
            url: file.url.trim_end_matches('/').to_string(),
            password,
        });
    }
    None
}

fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let pid = sysinfo::Pid::from_u32(pid as u32);
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    sys.process(pid).is_some()
}

impl ServiceInfo {
    fn auth_header(&self) -> Option<String> {
        self.password.as_ref().map(|password| {
            let raw = format!("opencode:{password}");
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(raw)
            )
        })
    }

    /// Whether this service has the given database file open. Only then is
    /// it safe to route deletes through it (otherwise the tool would
    /// modify the server's database instead of the target one).
    ///
    /// On Linux this checks the process's open file descriptors exactly;
    /// elsewhere it only trusts the default database location (no
    /// `OPENCODE_DB` / `OPENCODE_DATA_DIR` override).
    pub fn targets_db(&self, db_path: &Path) -> bool {
        if cfg!(target_os = "linux") {
            return linux_process_has_open(self.pid, db_path);
        }
        env_unset("OPENCODE_DB") && env_unset("OPENCODE_DATA_DIR")
    }

    /// `DELETE /api/session/{id}`; a 404 `SessionNotFoundError` counts as
    /// success (the session is already gone).
    pub fn delete_session(&self, id: &str) -> Result<()> {
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(15)))
                .http_status_as_error(false)
                .build(),
        );
        let url = format!("{}/api/session/{id}", self.url);
        let mut request = agent.delete(&url);
        if let Some(header) = self.auth_header() {
            request = request.header("Authorization", header);
        }
        let mut response = request
            .call()
            .map_err(|e| AppError::db(format!("opencode API request failed: {e}")))?;
        let status = response.status().as_u16();
        let body = response.body_mut().read_to_string().unwrap_or_default();
        match status {
            200 | 204 => Ok(()),
            404 if body.contains("SessionNotFound") => Ok(()),
            _ => Err(AppError::db(format!(
                "opencode API delete failed for {id} (HTTP {status}): {}",
                body.trim()
            ))),
        }
    }
}

fn env_unset(name: &str) -> bool {
    std::env::var(name).map(|v| v.is_empty()).unwrap_or(true)
}

#[cfg(target_os = "linux")]
fn linux_process_has_open(pid: i32, db_path: &Path) -> bool {
    let Ok(target) = std::fs::canonicalize(db_path) else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(PathBuf::from(format!("/proc/{pid}/fd"))) else {
        return false;
    };
    for entry in entries.flatten() {
        let Ok(link) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let name = link.to_string_lossy();
        if name.ends_with("-wal") || name.ends_with("-shm") || name.ends_with("-journal") {
            continue;
        }
        if let Ok(resolved) = std::fs::canonicalize(&link) {
            if resolved == target {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn temp_dir(stem: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "opencode-dbtool-service-{}-{}",
            std::process::id(),
            stem
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn discovers_service_file_with_inline_password() {
        let dir = temp_dir("inline");
        std::fs::write(
            dir.join("service.json"),
            format!(
                r#"{{"pid":{},"url":"http://127.0.0.1:1234/","version":"2.0.22","password":"secret"}}"#,
                std::process::id()
            ),
        )
        .unwrap();
        let svc = discover_in(&dir).unwrap();
        assert_eq!(svc.pid, std::process::id() as i32);
        assert_eq!(svc.url, "http://127.0.0.1:1234");
        assert_eq!(svc.password.as_deref(), Some("secret"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn discovers_server_file_with_password_file() {
        let dir = temp_dir("password-file");
        std::fs::write(
            dir.join("server.json"),
            format!(
                r#"{{"id":"x","pid":{},"url":"http://127.0.0.1:4321"}}"#,
                std::process::id()
            ),
        )
        .unwrap();
        std::fs::write(dir.join("password"), "from-file\n").unwrap();
        let svc = discover_in(&dir).unwrap();
        assert_eq!(svc.url, "http://127.0.0.1:4321");
        assert_eq!(svc.password.as_deref(), Some("from-file"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stale_pid_is_ignored() {
        let dir = temp_dir("stale");
        std::fs::write(
            dir.join("service.json"),
            r#"{"pid":2147483647,"url":"http://127.0.0.1:1"}"#,
        )
        .unwrap();
        assert!(discover_in(&dir).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn targets_db_checks_open_file_descriptors() {
        let dir = temp_dir("targets");
        let db = dir.join("opencode.db");
        std::fs::write(&db, b"x").unwrap();
        let svc = ServiceInfo {
            pid: std::process::id() as i32,
            url: "http://127.0.0.1:1".into(),
            password: None,
        };
        assert!(!svc.targets_db(&db), "no fd open yet");
        let _held = std::fs::File::open(&db).unwrap();
        assert!(svc.targets_db(&db), "fd is open");
        assert!(!svc.targets_db(&dir.join("other.db")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Minimal one-request HTTP server; returns (url, request line, auth
    /// header) and runs the handler in a thread.
    type MockRequest = (String, Option<String>);
    type MockServer = (
        String,
        std::sync::mpsc::Receiver<MockRequest>,
        std::thread::JoinHandle<()>,
    );

    fn mock_server(status: u16, body: &'static str) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let first = request.lines().next().unwrap_or("").to_string();
            let auth = request
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                .map(|l| l.split_once(':').unwrap().1.trim().to_string());
            tx.send((first, auth)).unwrap();
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });
        (format!("http://{addr}"), rx, handle)
    }

    fn service_at(url: String) -> ServiceInfo {
        ServiceInfo {
            pid: std::process::id() as i32,
            url,
            password: Some("pw".into()),
        }
    }

    #[test]
    fn api_delete_sends_basic_auth() {
        let (url, rx, handle) = mock_server(200, "{}");
        service_at(url)
            .delete_session("ses_1")
            .expect("delete succeeds");
        let (line, auth) = rx.recv().unwrap();
        handle.join().unwrap();
        assert_eq!(line, "DELETE /api/session/ses_1 HTTP/1.1");
        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("opencode:pw")
        );
        assert_eq!(auth.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn api_delete_treats_session_not_found_as_success() {
        let (url, _rx, handle) = mock_server(
            404,
            r#"{"_tag":"SessionNotFoundError","message":"Session not found"}"#,
        );
        service_at(url).delete_session("ses_gone").unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn api_delete_reports_other_errors() {
        let (url, _rx, handle) = mock_server(500, r#"{"message":"boom"}"#);
        let err = service_at(url).delete_session("ses_1").unwrap_err();
        handle.join().unwrap();
        assert!(err.message.contains("HTTP 500"), "got: {err}");
        assert!(err.message.contains("boom"), "got: {err}");
    }
}
