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
    pub version: Option<String>,
}

#[derive(Deserialize)]
struct ServiceFile {
    pid: i32,
    url: String,
    #[serde(default)]
    version: Option<String>,
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
            version: file.version,
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

/// Executable path of a running process (used to invoke its own
/// `service stop` / `service start` subcommands).
fn process_exe(pid: i32) -> Option<PathBuf> {
    let pid = sysinfo::Pid::from_u32(pid as u32);
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    sys.process(pid)
        .and_then(|p| p.exe().map(Path::to_path_buf))
}

/// Last-resort termination if the CLI's own `service stop` did not end
/// the process.
fn kill_process(pid: i32) -> bool {
    let pid = sysinfo::Pid::from_u32(pid as u32);
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    match sys.process(pid) {
        Some(process) => process.kill(),
        None => false,
    }
}

/// Pids (other than `exclude`) that have `db_path` open. Linux only:
/// elsewhere the list is empty and the post-stop check is skipped.
pub fn processes_with_db_open(db_path: &Path, exclude: &[i32]) -> Vec<i32> {
    #[cfg(target_os = "linux")]
    {
        let mut out = Vec::new();
        for pid in crate::sys::running_pids().unwrap_or_default() {
            if exclude.contains(&pid) {
                continue;
            }
            if linux_process_has_open(pid, db_path) {
                out.push(pid);
            }
        }
        out
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (db_path, exclude);
        Vec::new()
    }
}

/// A service stopped for maintenance, restarted explicitly (and, best
/// effort, when dropped) so opencode is never left stopped silently.
pub struct ServiceRestart {
    service: ServiceInfo,
    exe: PathBuf,
    stopped: bool,
}

impl ServiceRestart {
    /// Stop the registered service through its own `service stop`
    /// command, falling back to terminating the pid if it does not exit.
    pub fn stop(service: &ServiceInfo) -> Result<Self> {
        let exe = process_exe(service.pid).ok_or_else(|| {
            AppError::usage(format!(
                "--restart-service: cannot locate the opencode executable for pid {}",
                service.pid
            ))
        })?;
        let output = std::process::Command::new(&exe)
            .args(["service", "stop"])
            .output()
            .map_err(|e| {
                AppError::db(format!(
                    "--restart-service: cannot run {} service stop: {e}",
                    exe.display()
                ))
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if !process_alive(service.pid) {
                // The service did stop despite the non-zero exit; put it
                // back so opencode is not left down.
                let _ = std::process::Command::new(&exe)
                    .args(["service", "start"])
                    .output();
                return Err(AppError::db(format!(
                    "--restart-service: {} service stop failed ({stderr}); \
                     the service had already stopped and was restarted",
                    exe.display()
                )));
            }
            return Err(AppError::db(format!(
                "--restart-service: {} service stop failed: {stderr}",
                exe.display()
            )));
        }
        wait_for_exit(service.pid, 100);
        if process_alive(service.pid) {
            kill_process(service.pid);
            wait_for_exit(service.pid, 50);
        }
        if process_alive(service.pid) {
            return Err(AppError::db(format!(
                "--restart-service: opencode service (pid {}) did not stop",
                service.pid
            )));
        }
        Ok(ServiceRestart {
            service: service.clone(),
            exe,
            stopped: true,
        })
    }

    /// Start the service again with its own `service start`.
    pub fn restart(&mut self) -> Result<()> {
        if !self.stopped {
            return Ok(());
        }
        let output = std::process::Command::new(&self.exe)
            .args(["service", "start"])
            .output()
            .map_err(|e| {
                AppError::db(format!(
                    "--restart-service: cannot run {} service start: {e}",
                    self.exe.display()
                ))
            })?;
        if !output.status.success() {
            return Err(AppError::db(format!(
                "--restart-service: {} service start failed: {}",
                self.exe.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        self.stopped = false;
        Ok(())
    }

    pub fn pid(&self) -> i32 {
        self.service.pid
    }
}

impl Drop for ServiceRestart {
    fn drop(&mut self) {
        if self.stopped {
            let _ = self.restart();
        }
    }
}

fn wait_for_exit(pid: i32, steps: usize) {
    if pid <= 0 {
        return;
    }
    let pid = sysinfo::Pid::from_u32(pid as u32);
    let mut sys = sysinfo::System::new();
    for _ in 0..steps {
        sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        if sys.process(pid).is_none() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
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
    /// The check is exact on Linux (open file descriptors); `None` on
    /// other platforms, where API routing is disabled.
    pub fn db_match(&self, db_path: &Path) -> Option<bool> {
        #[cfg(target_os = "linux")]
        {
            Some(linux_process_has_open(self.pid, db_path))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = db_path;
            None
        }
    }

    /// API routing requires an exact database match.
    pub fn targets_db(&self, db_path: &Path) -> bool {
        self.db_match(db_path).unwrap_or(false)
    }

    fn agent() -> ureq::Agent {
        ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(15)))
                .http_status_as_error(false)
                // The URL is the locally registered server: never route it
                // through an HTTP(S)_PROXY from the environment.
                .proxy(None)
                .build(),
        )
    }

    fn get_json(&self, path: &str) -> Result<(u16, String)> {
        let url = format!("{}{path}", self.url);
        let mut request = Self::agent().get(&url);
        if let Some(header) = self.auth_header() {
            request = request.header("Authorization", header);
        }
        let mut response = request
            .call()
            .map_err(|e| AppError::db(format!("opencode API request failed: {e}")))?;
        let status = response.status().as_u16();
        let body = response.body_mut().read_to_string().unwrap_or_default();
        Ok((status, body))
    }

    /// `GET /api/info` (used by `service status`).
    pub fn api_info(&self) -> Result<serde_json::Value> {
        let (status, body) = self.get_json("/api/info")?;
        if status != 200 {
            return Err(AppError::db(format!(
                "opencode API info failed (HTTP {status}): {}",
                body.trim()
            )));
        }
        serde_json::from_str(&body)
            .map_err(|e| AppError::db(format!("invalid /api/info response: {e}")))
    }

    /// `GET /api/experimental/session/{id}/export` (raw export body).
    pub fn export_session(&self, id: &str) -> Result<String> {
        let (status, body) = self.get_json(&format!("/api/experimental/session/{id}/export"))?;
        if status != 200 {
            return Err(AppError::db(format!(
                "opencode API export failed for {id} (HTTP {status}): {}",
                body.trim()
            )));
        }
        Ok(body)
    }

    /// `POST /api/experimental/session/import`; returns the server's JSON
    /// response (or a wrapper when it is not JSON).
    pub fn import_session(&self, body: &str) -> Result<serde_json::Value> {
        let value: serde_json::Value = serde_json::from_str(body)
            .map_err(|e| AppError::usage(format!("invalid import file: {e}")))?;
        let url = format!("{}/api/experimental/session/import", self.url);
        let mut request = Self::agent().post(&url);
        if let Some(header) = self.auth_header() {
            request = request.header("Authorization", header);
        }
        let mut response = request
            .send_json(&value)
            .map_err(|e| AppError::db(format!("opencode API request failed: {e}")))?;
        let status = response.status().as_u16();
        let text = response.body_mut().read_to_string().unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(AppError::db(format!(
                "opencode API import failed (HTTP {status}): {}",
                text.trim()
            )));
        }
        Ok(serde_json::from_str(&text)
            .unwrap_or_else(|_| serde_json::json!({ "status": status, "body": text })))
    }

    /// `DELETE /api/session/{id}`; a 404 `SessionNotFoundError` counts as
    /// success (the session is already gone).
    pub fn delete_session(&self, id: &str) -> Result<()> {
        let url = format!("{}/api/session/{id}", self.url);
        let mut request = Self::agent().delete(&url);
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
    fn process_exe_finds_the_current_process() {
        let exe = process_exe(std::process::id() as i32).expect("current process has an exe");
        assert!(exe.is_absolute(), "got: {}", exe.display());
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
            version: None,
        };
        assert!(!svc.targets_db(&db), "no fd open yet");
        let _held = std::fs::File::open(&db).unwrap();
        assert!(svc.targets_db(&db), "fd is open");
        assert!(!svc.targets_db(&dir.join("other.db")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Minimal one-request HTTP server; returns (url, request line, auth
    /// header, full request) and runs the handler in a thread.
    type MockRequest = (String, Option<String>, String);
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
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            // Read until the headers and the declared body have arrived.
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let n = stream.read(&mut tmp).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header_end = pos + 4;
                    let headers = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
                    let content_length = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= header_end + content_length {
                        break;
                    }
                }
            }
            let request = String::from_utf8_lossy(&buf).to_string();
            let first = request.lines().next().unwrap_or("").to_string();
            let auth = request
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                .map(|l| l.split_once(':').unwrap().1.trim().to_string());
            tx.send((first, auth, request)).unwrap();
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
            version: None,
        }
    }

    #[test]
    fn api_delete_sends_basic_auth() {
        let (url, rx, handle) = mock_server(200, "{}");
        service_at(url)
            .delete_session("ses_1")
            .expect("delete succeeds");
        let (line, auth, _request) = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("mock server received no request");
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

    #[test]
    fn api_export_returns_the_body() {
        let (url, rx, handle) = mock_server(200, r#"{"session":"data"}"#);
        let body = service_at(url).export_session("ses_1").unwrap();
        let (line, _auth, _request) = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("mock server received no request");
        handle.join().unwrap();
        assert_eq!(line, "GET /api/experimental/session/ses_1/export HTTP/1.1");
        assert_eq!(body, r#"{"session":"data"}"#);
    }

    #[test]
    fn api_import_posts_the_json_body() {
        let (url, rx, handle) = mock_server(200, r#"{"id":"ses_new"}"#);
        let value = service_at(url)
            .import_session(r#"{"session":{"id":"x"}}"#)
            .unwrap();
        let (line, _auth, request) = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("mock server received no request");
        handle.join().unwrap();
        assert_eq!(line, "POST /api/experimental/session/import HTTP/1.1");
        let sent: serde_json::Value =
            serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap_or(""))
                .expect("request body is JSON");
        assert_eq!(sent["session"]["id"], "x");
        assert_eq!(value["id"], "ses_new");
    }

    #[test]
    fn api_import_rejects_invalid_json_without_a_request() {
        let svc = service_at("http://127.0.0.1:1".into());
        let err = svc.import_session("not json").unwrap_err();
        assert_eq!(err.code, 2, "usage error");
        assert!(err.message.contains("invalid import file"), "got: {err}");
    }
}
