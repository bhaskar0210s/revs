//! Zed extension: starts the Revs language server (a native binary) for every file.
//!
//! The server comes from this project's GitHub release of the same version (a small app
//! bundle for Macs with Apple silicon) and is kept in this extension's folder. Every file of
//! it must match the checksums this extension carries (`server.sha256`), so a release that
//! changed after the extension was published never runs. A build from source
//! (`make install`) puts its own copy there as `revs-dev`, which wins.

use sha2::{Digest, Sha256};
use std::fs;
use zed_extension_api::{self as zed, serde_json::Value, settings::LspSettings, LanguageServerId, Result};

const SERVER: &str = "revs";
/// The server release that goes with this version of the extension.
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DOWNLOAD: &str = "https://github.com/bhaskar0210s/revs/releases/download";
const ASSET: &str = "revs-aarch64-apple-darwin.zip";
/// The server inside the app bundle (the bundle also opens `revs://` links).
const BINARY: &str = "Revs.app/Contents/MacOS/revs";
const DEV: &str = "revs-dev";
/// SHA-256 and path of every file in this version's release (written by `make checksums`).
const CHECKSUMS: &str = include_str!("../server.sha256");

struct Revs;

fn is_file(path: &str) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

/// Collects the files under `dir` as paths starting with `prefix`; false if it holds
/// anything but files and folders.
fn files(dir: &str, prefix: &str, found: &mut Vec<String>) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let relative = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => {
                if !files(&format!("{dir}/{name}"), &relative, found) {
                    return false;
                }
            }
            Ok(kind) if kind.is_file() => found.push(relative),
            _ => return false,
        }
    }
    true
}

/// Whether `folder` holds exactly the release's files, each with its checksum.
fn verified(folder: &str) -> bool {
    matches_checksums(folder, CHECKSUMS)
}

/// Whether `folder` holds exactly the files `checksums` lists (`shasum -a 256` lines).
fn matches_checksums(folder: &str, checksums: &str) -> bool {
    let expected: Vec<(&str, &str)> = checksums.lines().filter_map(|line| line.split_once("  ")).collect();
    let mut found = Vec::new();
    if expected.is_empty() || !files(folder, "", &mut found) || found.len() != expected.len() {
        return false;
    }
    expected.iter().all(|(sum, path)| {
        found.iter().any(|file| file == path)
            && fs::read(format!("{folder}/{path}")).is_ok_and(|bytes| {
                let digest = Sha256::digest(&bytes);
                digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>() == *sum
            })
    })
}

/// `path` (relative to this extension's folder) as an absolute path for Zed to run.
fn absolute(path: &str) -> Result<String> {
    let folder = std::env::current_dir().map_err(|error| format!("no extension folder: {error}"))?;
    Ok(folder.join(path).to_string_lossy().into_owned())
}

impl Revs {
    fn server(&mut self, id: &LanguageServerId) -> Result<String> {
        let dev = format!("{DEV}/{BINARY}");
        if is_file(&dev) {
            return absolute(&dev);
        }
        let folder = format!("revs-{VERSION}");
        let path = format!("{folder}/{BINARY}");
        if is_file(&path) && verified(&folder) {
            return absolute(&path);
        }
        if CHECKSUMS.trim().is_empty() {
            // Only commits between a version change and its checksums lack them.
            let message = format!("this build of the Revs extension has no checksums for the Revs {VERSION} server, so it can't download one");
            zed::set_language_server_installation_status(id, &zed::LanguageServerInstallationStatus::Failed(message.clone()));
            return Err(message);
        }
        let (os, arch) = zed::current_platform();
        if !matches!((os, arch), (zed::Os::Mac, zed::Architecture::Aarch64)) {
            let message = "Revs runs on Macs with Apple silicon (macOS 27 or later)".to_string();
            zed::set_language_server_installation_status(id, &zed::LanguageServerInstallationStatus::Failed(message.clone()));
            return Err(message);
        }
        zed::set_language_server_installation_status(id, &zed::LanguageServerInstallationStatus::Downloading);
        let url = format!("{DOWNLOAD}/v{VERSION}/{ASSET}");
        let _ = fs::remove_dir_all(&folder);
        if let Err(error) = zed::download_file(&url, &folder, zed::DownloadedFileType::Zip) {
            let message = format!("couldn't download the Revs server: {error}");
            zed::set_language_server_installation_status(id, &zed::LanguageServerInstallationStatus::Failed(message.clone()));
            return Err(message);
        }
        if !verified(&folder) {
            let _ = fs::remove_dir_all(&folder);
            let message = format!("the downloaded Revs server doesn't match the checksums of Revs {VERSION}, so it wasn't started");
            zed::set_language_server_installation_status(id, &zed::LanguageServerInstallationStatus::Failed(message.clone()));
            return Err(message);
        }
        zed::make_file_executable(&path)?;
        // Servers of earlier versions aren't needed any more.
        for entry in fs::read_dir(".").into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("revs-") && name != folder && name != DEV {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
        zed::set_language_server_installation_status(id, &zed::LanguageServerInstallationStatus::None);
        absolute(&path)
    }
}

impl zed::Extension for Revs {
    fn new() -> Self {
        Revs
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let binary = LspSettings::for_worktree(SERVER, worktree)
            .ok()
            .and_then(|settings| settings.binary);
        let args = binary
            .as_ref()
            .and_then(|binary| binary.arguments.clone())
            .unwrap_or_else(|| vec!["lsp".into()]);
        let mut env: Vec<(String, String)> = binary
            .as_ref()
            .and_then(|binary| binary.env.clone())
            .map(|env| env.into_iter().collect())
            .unwrap_or_default();
        // Keeps the server's memory close to what it actually uses (~2 MB less after big files).
        if !env.iter().any(|(key, _)| key == "MallocSpaceEfficient") {
            env.push(("MallocSpaceEfficient".into(), "1".into()));
        }
        let command = match binary.and_then(|binary| binary.path) {
            Some(path) => path,
            None => self.server(language_server_id)?,
        };
        Ok(zed::Command { command, args, env })
    }

    fn language_server_initialization_options(
        &mut self,
        _language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Option<Value>> {
        Ok(LspSettings::for_worktree(SERVER, worktree)
            .ok()
            .and_then(|settings| settings.initialization_options))
    }

    fn language_server_workspace_configuration(
        &mut self,
        _language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Option<Value>> {
        Ok(LspSettings::for_worktree(SERVER, worktree)
            .ok()
            .and_then(|settings| settings.settings))
    }
}

zed::register_extension!(Revs);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_are_well_formed() {
        // `shasum -a 256` lines for files of the bundle, the server among them.
        for line in CHECKSUMS.lines() {
            let (sum, path) = line.split_once("  ").expect(line);
            assert!(sum.len() == 64 && sum.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')), "{line}");
            assert!(path.starts_with("Revs.app/Contents/"), "{line}");
        }
        assert!(CHECKSUMS.is_empty() || CHECKSUMS.lines().any(|line| line.ends_with(&format!("  {BINARY}"))));
    }

    #[test]
    fn only_the_listed_files_pass() {
        let dir = std::env::temp_dir().join(format!("revs-checksums-{}", std::process::id()));
        let folder = dir.to_string_lossy().into_owned();
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("Revs.app/Contents/MacOS")).unwrap();
        fs::write(dir.join("Revs.app/Contents/Info.plist"), "plist\n").unwrap();
        fs::write(dir.join("Revs.app/Contents/MacOS/revs"), "binary\n").unwrap();
        let sum = |text: &str| Sha256::digest(text.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        let checksums = format!(
            "{}  Revs.app/Contents/Info.plist\n{}  Revs.app/Contents/MacOS/revs\n",
            sum("plist\n"),
            sum("binary\n")
        );
        assert!(matches_checksums(&folder, &checksums));
        assert!(!matches_checksums(&folder, ""), "no checksums, no server");
        // A changed file, an extra file, a missing file, a link: all refused.
        fs::write(dir.join("Revs.app/Contents/MacOS/revs"), "other\n").unwrap();
        assert!(!matches_checksums(&folder, &checksums));
        fs::write(dir.join("Revs.app/Contents/MacOS/revs"), "binary\n").unwrap();
        fs::write(dir.join("Revs.app/Contents/MacOS/extra"), "x").unwrap();
        assert!(!matches_checksums(&folder, &checksums));
        fs::remove_file(dir.join("Revs.app/Contents/MacOS/extra")).unwrap();
        fs::remove_file(dir.join("Revs.app/Contents/Info.plist")).unwrap();
        assert!(!matches_checksums(&folder, &checksums));
        std::os::unix::fs::symlink("/etc/hosts", dir.join("Revs.app/Contents/Info.plist")).unwrap();
        assert!(!matches_checksums(&folder, &checksums));
        fs::remove_dir_all(&dir).unwrap();
    }
}
