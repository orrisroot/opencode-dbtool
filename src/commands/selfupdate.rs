//! `self-update`: replace the running binary with the latest GitHub release.
//!
//! Release assets (see `.github/workflows/release.yml`) are named
//! `opencode-dbtool-<target-triple>.tar.gz` (`.zip` on Windows) and contain the
//! binary at `<asset-stem>/opencode-dbtool[.exe]`, one directory below the
//! archive root.

use crate::error::{AppError, Result};
use crate::output;
use self_update::backends::github::Update;
use self_update::ReleaseStatus;
use serde::Serialize;

const REPO_OWNER: &str = "orrisroot";
const REPO_NAME: &str = "opencode-dbtool";
const BIN_NAME: &str = "opencode-dbtool";

/// Target triple of the published release matching this binary.
///
/// Linux maps to the `musl` triple regardless of the libc this binary was
/// built against: only static musl builds are published and they run on any
/// distribution, including gnu-target builds from source.
fn release_target() -> Result<String> {
    let triple = self_update::get_target();
    if triple.ends_with("-apple-darwin") || triple.ends_with("-pc-windows-msvc") {
        return Ok(triple.to_string());
    }
    let arch = triple.split('-').next().unwrap_or("");
    if triple.contains("linux") && matches!(arch, "x86_64" | "aarch64") {
        return Ok(format!("{arch}-unknown-linux-musl"));
    }
    Err(AppError::usage(format!(
        "no prebuilt release for target {triple}; update by rebuilding from source"
    )))
}

/// JSON shape of the `self-update` command output.
#[derive(Serialize)]
struct SelfUpdateOut {
    command: &'static str,
    current_version: String,
    latest_version: String,
    target: String,
    update_available: bool,
    updated: bool,
    dry_run: bool,
    status: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    release_url: Option<String>,
}

fn updater(target: &str, current_version: &str, tag: Option<&str>) -> Result<Update> {
    let mut builder = Update::configure();
    builder
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN_NAME)
        .bin_path_in_archive(format!(
            "{BIN_NAME}-{target}/{BIN_NAME}{}",
            std::env::consts::EXE_SUFFIX
        ))
        .target(target)
        .current_version(current_version)
        .show_output(false)
        .show_download_progress(false)
        .no_confirm(true)
        .check_install_path_writable(true)
        .auth_token_from_env();
    if let Some(tag) = tag {
        builder.release_tag(tag);
    }
    Ok(builder.build()?)
}

/// Check the latest GitHub release; with `--yes` download and replace the
/// running binary, with `--dry-run` stop at the report.
pub fn cmd_self_update(dry_run: bool) -> Result<()> {
    let current_version = env!("CARGO_PKG_VERSION").to_string();
    let target = release_target()?;
    let path = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let update = updater(&target, &current_version, None)?;

    let releases = update.get_latest_release()?;
    let latest = releases.latest().ok_or_else(|| {
        AppError::db(format!("no published release for {REPO_OWNER}/{REPO_NAME}"))
    })?;
    let latest_version = latest.version().to_string();
    let release_url = latest.release_notes_url().map(str::to_string);
    let available = self_update::version::bump_is_greater(&current_version, &latest_version)?;

    let mut out = SelfUpdateOut {
        command: "self-update",
        current_version,
        latest_version: latest_version.clone(),
        target: target.clone(),
        update_available: available,
        updated: false,
        dry_run,
        status: if available {
            "update-available"
        } else {
            "up-to-date"
        }
        .to_string(),
        path,
        release_url,
    };
    if !available || dry_run {
        return output::emit(&serde_json::to_value(&out)?);
    }

    // Apply exactly the release the check reported (pinned by tag), so the
    // install can never silently grab a different release than the one
    // announced - e.g. a pre-release newer than the latest stable, which
    // `/releases/latest` never returns.
    let pinned = updater(
        &target,
        &out.current_version,
        Some(&format!("v{latest_version}")),
    )?;
    match pinned.update_extended()? {
        ReleaseStatus::Updated(release) => {
            out.status = "updated".to_string();
            out.updated = true;
            out.latest_version = release.version().to_string();
            if out.release_url.is_none() {
                out.release_url = release.release_notes_url().map(str::to_string);
            }
        }
        _ => out.status = "up-to-date".to_string(),
    }
    output::emit(&serde_json::to_value(&out)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_target_is_a_published_triple() {
        let target = release_target();
        if let Ok(t) = &target {
            let published = [
                "x86_64-unknown-linux-musl",
                "aarch64-unknown-linux-musl",
                "x86_64-apple-darwin",
                "aarch64-apple-darwin",
                "x86_64-pc-windows-msvc",
                "aarch64-pc-windows-msvc",
            ];
            assert!(published.contains(&t.as_str()), "unexpected target: {t}");
        }
    }

    #[test]
    fn updater_configures_against_the_release_layout() {
        let update = updater("x86_64-unknown-linux-musl", "0.0.1", None).unwrap();
        use self_update::UpdateConfig;
        assert_eq!(update.target(), "x86_64-unknown-linux-musl");
        assert_eq!(update.current_version(), "0.0.1");
        assert_eq!(update.release_tag(), None);
        assert_eq!(
            update.bin_name(),
            format!("opencode-dbtool{}", std::env::consts::EXE_SUFFIX)
        );
        assert_eq!(
            update.bin_path_in_archive(),
            format!(
                "opencode-dbtool-x86_64-unknown-linux-musl/opencode-dbtool{}",
                std::env::consts::EXE_SUFFIX
            )
        );
    }

    #[test]
    fn updater_pins_the_reported_release() {
        let update = updater("aarch64-unknown-linux-musl", "1.0.0", Some("v1.1.0")).unwrap();
        use self_update::UpdateConfig;
        assert_eq!(update.release_tag(), Some("v1.1.0"));
    }
}
