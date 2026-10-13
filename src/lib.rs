//! Zed extension: starts the Revs language server (a native binary) for every file.
//!
//! The server comes from this project's GitHub release of the same version (a small app
//! bundle for Macs with Apple silicon) and is kept in this extension's folder. A build from
//! source (`make install`) puts its own copy there as `revs-dev`, which wins.

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

struct Revs;

fn is_file(path: &str) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file())
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
        if is_file(&path) {
            return absolute(&path);
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
