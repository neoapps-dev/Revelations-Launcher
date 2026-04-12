use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

#[cfg(target_os = "macos")]
use std::process::Stdio;

use tauri::{AppHandle, Emitter, State, Manager};
use tauri::menu::{Menu, MenuItem, MenuEvent};
use tauri::tray::{TrayIcon, TrayIconBuilder, TrayIconEvent};
use futures_util::StreamExt;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tauri_plugin_opener::OpenerExt;
use serde::{Deserialize, Serialize};

const MAX_DOWNLOAD_BYTES: u64 = 4 * 1024 * 1024 * 1024; // 4 GB
const MAX_SKIN_BYTES: usize = 64 * 1024; // 64 KB

/// Create a Command that hides the console window on Windows.
fn hidden_command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    cmd
}

/// Sanitize a path component to prevent directory traversal.
/// Strips any directory separators and rejects ".." segments.
fn sanitize_path_component(name: &str) -> Result<String, String> {
    let sanitized: String = name
        .replace(['/', '\\'], "")
        .replace("..", "");
    if sanitized.is_empty() {
        return Err("Invalid path component: name is empty after sanitization".to_string());
    }
    Ok(sanitized)
}

/// Validate that a constructed path stays within the expected base directory.
fn validate_path_within(path: &PathBuf, base: &PathBuf) -> Result<(), String> {
    let canonical_base = base.canonicalize().unwrap_or_else(|_| base.clone());
    let canonical_path = path.canonicalize().unwrap_or_else(|_| path.clone());
    if !canonical_path.starts_with(&canonical_base) {
        return Err("Path traversal detected: target is outside allowed directory".to_string());
    }
    Ok(())
}

/// Validate download URL: HTTPS only, no private/local networks.
fn validate_download_url(url: &str) -> Result<(), String> {
    let parsed = url.strip_prefix("https://")
        .ok_or_else(|| "Only HTTPS URLs are allowed for downloads".to_string())?;

    let host = parsed.split('/').next().unwrap_or("");
    let host = host.split(':').next().unwrap_or(host);

    let blocked_hosts = ["localhost", "127.0.0.1", "0.0.0.0", "[::1]", "169.254.169.254"];
    if blocked_hosts.contains(&host) {
        return Err("Downloads from local/private addresses are not allowed".to_string());
    }

    if host.starts_with("10.")
        || host.starts_with("192.168.")
        || (host.starts_with("172.") && is_private_172(host))
    {
        return Err("Downloads from private network addresses are not allowed".to_string());
    }

    Ok(())
}

fn is_private_172(host: &str) -> bool {
    if let Some(second) = host.strip_prefix("172.").and_then(|r| r.split('.').next()) {
        if let Ok(n) = second.parse::<u8>() {
            return (16..=31).contains(&n);
        }
    }
    false
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct McServer {
    pub name: String,
    pub ip: String,
    pub port: u16,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SkinLibraryItem {
    pub id: String,
    pub name: String,
    pub skin_base64: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CustomEdition {
    pub id: String,
    pub name: String,
    pub desc: String,
    pub url: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    pub username: String,
    pub linux_runner: Option<String>,
    pub skin_base64: Option<String>,
    pub skin_model: Option<String>,
    pub skin_library: Option<Vec<SkinLibraryItem>>,
    pub theme_style_id: Option<String>,
    pub theme_palette_id: Option<String>,
    pub apple_silicon_performance_boost: Option<bool>,
    pub custom_editions: Option<Vec<CustomEdition>>,
    pub profile: Option<String>,
    pub keep_launcher_open: Option<bool>,
    pub enable_tray_icon: Option<bool>,
    pub animations_enabled: Option<bool>,
    pub vfx_enabled: Option<bool>,
    pub rpc_enabled: Option<bool>,
    pub music_vol: Option<u32>,
    pub sfx_vol: Option<u32>,
    pub legacy_mode: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ThemePalette {
    pub id: String,
    pub name: String,
    pub colors: serde_json::Value,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Runner {
    pub id: String,
    pub name: String,
    pub path: String,
    pub r#type: String,
}

pub struct DownloadState { pub token: Arc<Mutex<Option<CancellationToken>>> }
pub struct GameState { pub child: Arc<Mutex<Option<tokio::process::Child>>> }

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
#[cfg(target_os = "macos")]
struct MacosSetupProgressPayload {
    stage: String,
    message: String,
    percent: Option<f64>,
}

fn get_app_dir(app: &AppHandle) -> PathBuf {
    app.path().app_local_data_dir().unwrap_or_else(|_| {
        std::env::current_dir().unwrap_or_default()
    })
}

#[cfg(target_os = "macos")]
fn get_macos_runtime_dir(app: &AppHandle) -> PathBuf {
    let home = app
        .path()
        .home_dir()
        .ok()
        .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"));
    home.join("Library")
        .join("Application Support")
        .join("com.revelations.lce")
        .join("runtime")
}

#[cfg(target_os = "macos")]
fn emit_macos_setup_progress(window: &tauri::Window, stage: &str, message: String, percent: Option<f64>) {
    let _ = window.emit(
        "macos-setup-progress",
        MacosSetupProgressPayload {
            stage: stage.to_string(),
            message,
            percent,
        },
    );
}

#[cfg(target_os = "macos")]
fn find_executable_recursive(root: &PathBuf, file_name: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_executable_recursive(&path, file_name) {
                return Some(found);
            }
            continue;
        }

        if path.file_name().and_then(|n| n.to_str()) == Some(file_name) {
            return Some(path);
        }
    }
    None
}

#[cfg(unix)]
fn unix_path_to_wine_z_path(unix_path: &PathBuf) -> String {
    let p = unix_path.to_string_lossy();
    let mut out = String::with_capacity(p.len() + 3);
    out.push_str("Z:");
    for ch in p.chars() {
        if ch == '/' {
            out.push('\\');
        } else {
            out.push(ch);
        }
    }
    out
}

fn get_config_path(app: &AppHandle) -> PathBuf {
    get_app_dir(app).join("revelations_config.json")
}

#[tauri::command]
fn save_config(app: AppHandle, config: AppConfig) {
    let path = get_config_path(&app);
    let _ = fs::create_dir_all(path.parent().unwrap());
    if let Ok(json) = serde_json::to_string(&config) {
        let _ = fs::write(path, json);
    }
}

#[tauri::command]
fn load_config(app: AppHandle) -> AppConfig {
    let path = get_config_path(&app);
    if let Ok(content) = fs::read_to_string(path) {
        if let Ok(config) = serde_json::from_str(&content) {
            return config;
        }
    }

    let old_path = get_app_dir(&app).join("revelations_config.txt");
    let username = fs::read_to_string(old_path).unwrap_or_else(|_| "Player".into());
    AppConfig {
        username,
        linux_runner: None,
        skin_base64: None,
        skin_model: None,
        skin_library: None,
        theme_style_id: None,
        theme_palette_id: None,
        apple_silicon_performance_boost: None,
        custom_editions: None,
        profile: Some("legacy_evolved".into()),
        keep_launcher_open: None,
        enable_tray_icon: Some(true),
        animations_enabled: Some(true),
        vfx_enabled: Some(true),
        rpc_enabled: Some(true),
        music_vol: Some(50),
        sfx_vol: Some(100),
        legacy_mode: Some(false),
    }
}

#[tauri::command]
fn get_external_palettes(app: AppHandle) -> Vec<ThemePalette> {
    let themes_dir = get_app_dir(&app).join("themes");
    let _ = fs::create_dir_all(&themes_dir);
    let mut palettes = Vec::new();

    if let Ok(entries) = fs::read_dir(themes_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(palette) = serde_json::from_str::<ThemePalette>(&content) {
                        palettes.push(palette);
                    }
                }
            }
        }
    }
    palettes
}

#[tauri::command]
fn import_theme(app: AppHandle) -> Result<String, String> {
    let file = rfd::FileDialog::new()
        .add_filter("JSON Theme", &["json"])
        .set_title("Import Theme Palette")
        .pick_file();

    if let Some(src_path) = file {
        let content = fs::read_to_string(&src_path).map_err(|e| e.to_string())?;
        let palette: ThemePalette = serde_json::from_str(&content).map_err(|_| "Invalid theme JSON format".to_string())?;

        let themes_dir = get_app_dir(&app).join("themes");
        let _ = fs::create_dir_all(&themes_dir);

        let safe_id = sanitize_path_component(&palette.id)?;
        let dest_path = themes_dir.join(format!("{}.json", safe_id));
        fs::write(dest_path, content).map_err(|e| e.to_string())?;

        Ok(palette.name)
    } else {
        Err("CANCELED".into())
    }
}

#[tauri::command]
fn get_available_runners(app: AppHandle) -> Vec<Runner> {
    let mut runners = Vec::new();
    let mut seen_paths: std::collections::HashSet<String> = std::collections::HashSet::new();

    #[cfg(target_os = "linux")]
    {
        if let Ok(output) = Command::new("which").arg("wine").output() {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !seen_paths.contains(&path) {
                    seen_paths.insert(path.clone());
                    runners.push(Runner {
                        id: "wine".to_string(),
                        name: "System Wine".to_string(),
                        path,
                        r#type: "wine".to_string(),
                    });
                }
            }
        }

        if let Ok(output) = Command::new("ls").arg("/usr/share/revelations-launcher/wine/bin/wine").output() {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !seen_paths.contains(&path) {
                    seen_paths.insert(path.clone());
                    runners.push(Runner {
                        id: "flatpaksucks".to_string(),
                        name: "Default for Flatpak".to_string(),
                        path,
                        r#type: "wine".to_string(),
                    });
                }
            }
        }

        let home = std::env::var("HOME").unwrap_or_default();
        let steam_paths = [
            format!("{}/.local/share/Steam/compatibilitytools.d", home),
            format!("{}/.local/share/Steam/steamapps/common", home),
        ];

        for base_path in steam_paths {
            if let Ok(entries) = fs::read_dir(base_path) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if name.contains("Proton") {
                            let path_str = path.to_string_lossy().to_string();
                            if !seen_paths.contains(&path_str) {
                                seen_paths.insert(path_str.clone());
                                runners.push(Runner {
                                    id: format!("proton_{}", name),
                                    name: name,
                                    path: path_str,
                                    r#type: "proton".to_string(),
                                });
                            }
                        }
                    }
                }
            }
        }

        let runners_dir = get_app_dir(&app).join("runners");
        let _ = fs::create_dir_all(&runners_dir);
        if let Ok(entries) = fs::read_dir(&runners_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let dir_name = entry.file_name().to_string_lossy().to_string();
                    let wine_bin = path.join("bin").join("wine");
                    let proton_bin = path.join("proton");
                    if proton_bin.exists() {
                        let path_str = path.to_string_lossy().to_string();
                        if !seen_paths.contains(&path_str) {
                            seen_paths.insert(path_str.clone());
                            runners.push(Runner {
                                id: format!("downloaded_{}", dir_name),
                                name: format!("{} (downloaded)", dir_name),
                                path: path_str,
                                r#type: "proton".to_string(),
                            });
                        }
                    } else if wine_bin.exists() {
                        let path_str = wine_bin.to_string_lossy().to_string();
                        if !seen_paths.contains(&path_str) {
                            seen_paths.insert(path_str.clone());
                            runners.push(Runner {
                                id: format!("downloaded_{}", dir_name),
                                name: format!("{} (downloaded)", dir_name),
                                path: path_str,
                                r#type: "wine".to_string(),
                            });
                        }
                    }
                }
            }
        }
    }

    runners
}

#[tauri::command]
async fn download_runner(app: AppHandle, state: State<'_, DownloadState>, name: String, url: String) -> Result<String, String> {
    validate_download_url(&url)?;
    let safe_name = sanitize_path_component(&name)?;

    let runners_dir = get_app_dir(&app).join("runners");
    fs::create_dir_all(&runners_dir).map_err(|e| e.to_string())?;

    let runner_dir = runners_dir.join(&safe_name);
    if runner_dir.exists() {
        let _ = fs::remove_dir_all(&runner_dir);
    }
    fs::create_dir_all(&runner_dir).map_err(|e| e.to_string())?;

    let token = CancellationToken::new();
    let child_token = token.clone();
    {
        let mut lock = state.token.lock().await;
        if let Some(old_token) = lock.take() {
            old_token.cancel();
        }
        *lock = Some(token);
    }

    let tarball_path = runners_dir.join(format!("{}.tar.gz", safe_name));
    let response = reqwest::get(&url).await.map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        return Err(format!("Download failed: {}", response.status()));
    }

    let total_size = response.content_length().unwrap_or(0) as f64;
    let mut file = fs::File::create(&tarball_path).map_err(|e| e.to_string())?;
    let mut downloaded: u64 = 0;
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        if child_token.is_cancelled() {
            drop(file);
            let _ = fs::remove_file(&tarball_path);
            let _ = fs::remove_dir_all(&runner_dir);
            return Err("CANCELLED".into());
        }
        let chunk = chunk.map_err(|e| e.to_string())?;
        file.write_all(&chunk).map_err(|e| e.to_string())?;
        downloaded += chunk.len() as u64;
        if downloaded > MAX_DOWNLOAD_BYTES {
            drop(file);
            let _ = fs::remove_file(&tarball_path);
            let _ = fs::remove_dir_all(&runner_dir);
            return Err("Download exceeds maximum allowed size (4 GB)".into());
        }
        if total_size > 0.0 {
            let _ = app.emit("runner-download-progress", downloaded as f64 / total_size * 100.0);
        }
    }

    drop(file);
    { *state.token.lock().await = None; }

    let tarball_str = tarball_path.to_str().ok_or_else(|| "Invalid tarball path".to_string())?;
    let runner_str = runner_dir.to_str().ok_or_else(|| "Invalid runner path".to_string())?;
    let status = hidden_command("tar")
        .args(["-zxf", tarball_str, "-C", runner_str, "--strip-components=1"])
        .status()
        .map_err(|e| e.to_string())?;

    let _ = fs::remove_file(&tarball_path);

    if !status.success() {
        let _ = fs::remove_dir_all(&runner_dir);
        return Err("Extraction failed".into());
    }

    Ok(safe_name)
}

#[tauri::command]
#[allow(non_snake_case)]
fn check_game_installed(app: AppHandle, instance_id: String) -> bool {
    let Ok(safe_id) = sanitize_path_component(&instance_id) else { return false };
    get_app_dir(&app).join("instances").join(&safe_id).join("Minecraft.Client.exe").exists()
}

#[tauri::command]
#[allow(non_snake_case)]
fn save_skin_file(app: AppHandle, name: String, skinBase64: String) -> Result<(), String> {
    use base64::{Engine as _, engine::general_purpose};
    let safe_name = sanitize_path_component(&name)?;
    let skins_dir = get_app_dir(&app).join("skins");
    let _ = fs::create_dir_all(&skins_dir);
    let base64_str = skinBase64.split(',').nth(1).unwrap_or(&skinBase64);
    let bytes = general_purpose::STANDARD.decode(base64_str)
        .map_err(|_| "Invalid base64 skin data".to_string())?;
    fs::write(skins_dir.join(format!("{}.png", safe_name)), bytes)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn open_skins_folder(app: AppHandle) -> Result<(), String> {
    let skins_dir = get_app_dir(&app).join("skins");
    let _ = fs::create_dir_all(&skins_dir);
    let dir_str = skins_dir.to_str().ok_or_else(|| "Invalid directory path".to_string())?;
    let _ = app.opener().open_path(dir_str, None::<&str>);
    Ok(())
}

#[tauri::command]
#[allow(non_snake_case)]
fn open_instance_folder(app: AppHandle, instance_id: String) -> Result<(), String> {
    let safe_id = sanitize_path_component(&instance_id)?;
    let instances_dir = get_app_dir(&app).join("instances");
    let dir = instances_dir.join(&safe_id);
    if dir.exists() {
        validate_path_within(&dir, &instances_dir)?;
        let dir_str = dir.to_str().ok_or_else(|| "Invalid directory path".to_string())?;
        let _ = app.opener().open_path(dir_str, None::<&str>);
    }
    Ok(())
}

#[tauri::command]
#[allow(non_snake_case)]
fn delete_instance(app: AppHandle, instance_id: String) -> Result<(), String> {
    let safe_id = sanitize_path_component(&instance_id)?;
    let instances_dir = get_app_dir(&app).join("instances");
    let dir = instances_dir.join(&safe_id);
    if dir.exists() {
        validate_path_within(&dir, &instances_dir)?;
        let _ = fs::remove_dir_all(dir);
    }
    Ok(())
}

#[tauri::command]
async fn cancel_download(state: State<'_, DownloadState>) -> Result<(), String> {
    if let Some(token) = state.token.lock().await.take() { token.cancel(); }
    Ok(())
}

#[tauri::command]
async fn setup_macos_runtime(window: tauri::Window, app: AppHandle) -> Result<(), String> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = window;
        let _ = app;
        return Err("macOS runtime setup is only supported on macOS.".into());
    }

    #[cfg(target_os = "macos")]
    {
        #[derive(Deserialize)]
        struct GithubAsset {
            name: String,
            browser_download_url: String,
        }

        #[derive(Deserialize)]
        struct GithubRelease {
            tag_name: String,
            assets: Vec<GithubAsset>,
        }

        emit_macos_setup_progress(&window, "resolving", "Resolving macOS compatibility runtime…".into(), None);

        let client = reqwest::Client::new();
        let release_text = client
            .get("https://api.github.com/repos/Gcenx/game-porting-toolkit/releases/latest")
            .header("User-Agent", "Revelations-Launcher")
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?
            .text()
            .await
            .map_err(|e| e.to_string())?;

        let release: GithubRelease = serde_json::from_str(&release_text).map_err(|e| e.to_string())?;
        let asset = release
            .assets
            .iter()
            .find(|a| a.name.ends_with(".tar.xz") || a.name.ends_with(".tar.gz"))
            .ok_or_else(|| "No compatible runtime asset found in latest release.".to_string())?;

        let runtime_dir = get_macos_runtime_dir(&app);
        let toolkit_dir = runtime_dir.join("toolkit");
        let prefix_dir = runtime_dir.join("prefix");
        fs::create_dir_all(&runtime_dir).map_err(|e| e.to_string())?;

        if toolkit_dir.exists() {
            let _ = fs::remove_dir_all(&toolkit_dir);
        }
        fs::create_dir_all(&toolkit_dir).map_err(|e| e.to_string())?;

        emit_macos_setup_progress(
            &window,
            "downloading",
            format!("Downloading runtime ({})…", release.tag_name),
            Some(0.0),
        );

        let archive_path = runtime_dir.join(format!("gptk_{}", asset.name));
        let response = client
            .get(&asset.browser_download_url)
            .header("User-Agent", "Revelations-Launcher")
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?;

        let total_size = response.content_length().unwrap_or(0) as f64;
        let mut file = fs::File::create(&archive_path).map_err(|e| e.to_string())?;
        let mut downloaded = 0.0;
        let mut last_percent_sent: i64 = -1;
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            file.write_all(&chunk).map_err(|e| e.to_string())?;
            downloaded += chunk.len() as f64;

            if total_size > 0.0 {
                let percent = (downloaded / total_size * 100.0).clamp(0.0, 100.0);
                let rounded = percent.floor() as i64;
                if rounded != last_percent_sent {
                    last_percent_sent = rounded;
                    emit_macos_setup_progress(
                        &window,
                        "downloading",
                        format!("Downloading runtime… {}%", rounded),
                        Some(percent),
                    );
                }
            }
        }
        drop(file);

        emit_macos_setup_progress(&window, "extracting", "Extracting runtime…".into(), None);
        
        let archive_metadata = fs::metadata(&archive_path).map_err(|e| format!("Cannot read archive: {}", e))?;
        println!("Archive size: {} bytes", archive_metadata.len());
        
        if archive_metadata.len() < 100_000_000 {
            return Err(format!("Archive too small: {} bytes", archive_metadata.len()));
        }
        
        let status = hidden_command("tar")
            .args([
                "-xf",
                archive_path.to_str().ok_or_else(|| "Invalid archive path".to_string())?,
                "-C",
                toolkit_dir.to_str().ok_or_else(|| "Invalid toolkit path".to_string())?,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status()
            .map_err(|e| e.to_string())?;
        
        println!("Tar exit status: {:?}", status);
        
        let _ = fs::remove_file(&archive_path);
        if !status.success() {
            return Err(format!("Extraction failed with status: {:?}", status));
        }

        fs::create_dir_all(&prefix_dir).map_err(|e| e.to_string())?;

        let wine_binary = find_executable_recursive(&toolkit_dir, "wine64")
            .or_else(|| find_executable_recursive(&toolkit_dir, "wine"))
            .ok_or_else(|| "Unable to locate wine binary inside runtime.".to_string())?;

        let wine_bin_dir = wine_binary
            .parent()
            .map(|pp| pp.to_path_buf())
            .ok_or_else(|| "Unable to locate wine bin directory inside runtime.".to_string())?;

        emit_macos_setup_progress(&window, "initializing", "Initializing Wine prefix…".into(), None);

        let mut cmd = Command::new(&wine_binary);
        cmd.arg("wineboot");
        cmd.arg("-u");
        cmd.env("WINEPREFIX", &prefix_dir);
        cmd.env("WINEARCH", "win64");
        cmd.env("WINEDEBUG", "-all");
        cmd.env("WINEESYNC", "1");
        cmd.env("WINEDLLOVERRIDES", "winemenubuilder.exe=d;mscoree,mshtml=");
        cmd.env("MTL_HUD_ENABLED", "0");
        cmd.env(
            "PATH",
            format!(
                "{}:{}",
                wine_bin_dir.to_string_lossy(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let status = cmd.status().map_err(|e| e.to_string())?;
        if !status.success() {
            return Err("Wine prefix initialization failed".into());
        }

        emit_macos_setup_progress(&window, "done", "Setup complete.".into(), Some(100.0));
        Ok(())
    }
}

/// Build a minimal LCE skin pack (.pck) containing a single skin PNG.
fn build_skin_pck(skin_png: &[u8], display_name: &str, is_alex: bool) -> Vec<u8> {
    let mut buf = Vec::new();

    fn w32(buf: &mut Vec<u8>, v: u32) { buf.extend_from_slice(&v.to_le_bytes()); }

    fn wstr(buf: &mut Vec<u8>, s: &str) {
        let chars: Vec<u16> = s.encode_utf16().collect();
        w32(buf, chars.len() as u32);
        for c in &chars { buf.extend_from_slice(&c.to_le_bytes()); }
        buf.extend_from_slice(&[0, 0, 0, 0]);
    }

    // Build ANIM flags:
    // Bit 18 (0x40000) = MODERN_WIDE_MODEL
    // Bit 19 (0x80000) = SLIM_MODEL (Alex/3px arms)
    let mut anim_flags: u32 = 0x40000; // MODERN_WIDE_MODEL always set
    if is_alex {
        anim_flags |= 0x80000; // SLIM_MODEL
    }
    let anim_str = format!("0x{:08x}", anim_flags);

    // Version
    w32(&mut buf, 3);

    // Property type definitions
    let prop_types = ["DISPLAYNAME", "GAME_FLAGS", "FREE", "ANIM"];
    w32(&mut buf, prop_types.len() as u32);
    for (i, name) in prop_types.iter().enumerate() {
        w32(&mut buf, i as u32);
        wstr(&mut buf, name);
    }

    // File count: 1
    w32(&mut buf, 1);

    // File header
    let filename = "dlcskin00000000.png";
    w32(&mut buf, skin_png.len() as u32);
    w32(&mut buf, 0); // asset type 0 = SkinFile
    wstr(&mut buf, filename);

    // Properties (after all headers, before data)
    w32(&mut buf, 4);

    // DISPLAYNAME (index 0)
    w32(&mut buf, 0);
    wstr(&mut buf, display_name);

    // GAME_FLAGS (index 1) — always 0x18 for human skins
    w32(&mut buf, 1);
    wstr(&mut buf, "0x18");

    // FREE (index 2)
    w32(&mut buf, 2);
    wstr(&mut buf, "1");

    // ANIM (index 3) — encodes model type via flags
    w32(&mut buf, 3);
    wstr(&mut buf, &anim_str);

    // File data
    buf.extend_from_slice(skin_png);

    buf
}

fn copy_dir_all(src: impl AsRef<std::path::Path>, dst: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    fs::create_dir_all(&dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_all(entry.path(), dst.as_ref().join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
        }
    }
    Ok(())
}

#[tauri::command]
#[allow(non_snake_case)]
async fn download_and_install(app: AppHandle, state: State<'_, DownloadState>, url: String, instance_id: String) -> Result<String, String> {
    validate_download_url(&url)?;
    let safe_id = sanitize_path_component(&instance_id)?;

    let root = get_app_dir(&app);
    let instance_dir = root.join("instances").join(&safe_id);
    let token = CancellationToken::new();
    let child_token = token.clone();
    {
        let mut lock = state.token.lock().await;
        if let Some(old_token) = lock.take() {
            old_token.cancel();
        }
        *lock = Some(token);
    }

    fs::create_dir_all(&instance_dir).map_err(|e| e.to_string())?;

    let zip_path = root.join(format!("temp_{}.zip", safe_id));
    let response = reqwest::get(&url).await.map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        return Err(format!("Download failed: {}", response.status()));
    }

    let total_size = response.content_length().unwrap_or(0) as f64;
    let mut file = fs::File::create(&zip_path).map_err(|e| e.to_string())?;
    let mut downloaded: u64 = 0;
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        if child_token.is_cancelled() {
            drop(file); let _ = fs::remove_file(&zip_path);
            return Err("CANCELLED".into());
        }
        let chunk = chunk.map_err(|e| e.to_string())?;
        file.write_all(&chunk).map_err(|e| e.to_string())?;
        downloaded += chunk.len() as u64;
        if downloaded > MAX_DOWNLOAD_BYTES {
            drop(file);
            let _ = fs::remove_file(&zip_path);
            return Err("Download exceeds maximum allowed size (4 GB)".into());
        }
        if total_size > 0.0 { let _ = app.emit("download-progress", downloaded as f64 / total_size * 100.0); }
    }

    drop(file);
    { *state.token.lock().await = None; }

    // Extract to a temp staging directory
    let staging_dir = root.join(format!("temp_staging_{}", safe_id));
    if staging_dir.exists() { let _ = fs::remove_dir_all(&staging_dir); }
    fs::create_dir_all(&staging_dir).map_err(|e| e.to_string())?;

    let staging_str = staging_dir.to_str().ok_or_else(|| "Invalid staging path".to_string())?;
    let zip_str = zip_path.to_str().ok_or_else(|| "Invalid zip path".to_string())?;

    #[cfg(target_os = "linux")]
    {
        let unzip_check = hidden_command("unzip").arg("-v").status();
        if unzip_check.is_err() || !unzip_check.unwrap().success() {
            let _ = fs::remove_dir_all(&staging_dir);
            return Err("The 'unzip' command was not found. Please install it (e.g., 'sudo apt install unzip') to continue.".into());
        }

        let status = hidden_command("unzip")
            .args(["-o", "-q", zip_str, "-d", staging_str])
            .status()
            .map_err(|e| e.to_string())?;

        if !status.success() {
            let _ = fs::remove_dir_all(&staging_dir);
            return Err("Extraction failed".into());
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let status = hidden_command("tar")
            .args(["-xf", zip_str, "-C", staging_str])
            .status()
            .map_err(|e| e.to_string())?;

        if !status.success() {
            let _ = fs::remove_dir_all(&staging_dir);
            return Err("Extraction failed".into());
        }
    }

    let _ = fs::remove_file(&zip_path);

    // Unwrap single nested directory if present
    let mut source_dir = staging_dir.clone();
    if let Ok(entries) = fs::read_dir(&staging_dir) {
        let entries_vec: Vec<_> = entries.flatten().collect();
        if entries_vec.len() == 1 && entries_vec[0].path().is_dir() {
            source_dir = entries_vec[0].path();
        }
    }

    // Merge new files into instance, preserving user data
    merge_directory(&source_dir, &instance_dir).map_err(|e| format!("Merge failed: {}", e))?;
    let _ = fs::remove_dir_all(&staging_dir);

    // Write version metadata with the current Nightly release timestamp
    if let Ok(published_at) = fetch_nightly_published_at().await {
        let metadata = serde_json::json!({ "installed_at": published_at });
        let _ = fs::write(instance_dir.join("version_metadata.json"), metadata.to_string());
    }

    Ok("Success".into())
}

/// Merge files from src into dst, only overwriting files that differ in size.
fn merge_directory(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(src).map_err(|e| e.to_string())?.flatten() {
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            merge_directory(&src_path, &dst_path)?;
        } else {
            let should_copy = if dst_path.exists() {
                let src_size = fs::metadata(&src_path).map(|m| m.len()).unwrap_or(0);
                let dst_size = fs::metadata(&dst_path).map(|m| m.len()).unwrap_or(0);
                src_size != dst_size
            } else {
                true
            };
            if should_copy {
                let _ = fs::copy(&src_path, &dst_path);
            }
        }
    }
    Ok(())
}

async fn fetch_nightly_published_at() -> Result<String, String> {
    let client = reqwest::Client::new();
    let response = client
        .get("https://api.github.com/repos/itsRevela/LCE-Revelations/releases/tags/Nightly")
        .header("User-Agent", "Revelations-Launcher")
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        return Err(format!("GitHub API returned {}", response.status()));
    }

    let body = response.text().await.map_err(|e| e.to_string())?;
    let json: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    json["published_at"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "No published_at in response".into())
}

#[tauri::command]
#[allow(non_snake_case)]
async fn check_for_game_update(app: AppHandle, instance_id: String) -> Result<bool, String> {
    let safe_id = sanitize_path_component(&instance_id)?;
    let instance_dir = get_app_dir(&app).join("instances").join(&safe_id);
    let metadata_path = instance_dir.join("version_metadata.json");

    let installed_at = if let Ok(content) = fs::read_to_string(&metadata_path) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            json["installed_at"].as_str().map(|s| s.to_string())
        } else {
            None
        }
    } else {
        None
    };

    // No metadata = never tracked, treat as update available
    let installed_at = match installed_at {
        Some(ts) => ts,
        None => return Ok(true),
    };

    // Fetch remote timestamp (silently return false on failure)
    let remote_at = match fetch_nightly_published_at().await {
        Ok(ts) => ts,
        Err(_) => return Ok(false),
    };

    Ok(remote_at > installed_at)
}

fn ensure_server_list(instance_dir: &PathBuf, servers: Vec<McServer>) {
    let servers_db = instance_dir.join("servers.db");
    
    let mut all_servers = Vec::new();
    
    if let Ok(content) = fs::read(&servers_db) {
        if content.len() >= 12 && &content[0..4] == b"MCSV" {
            let count = u32::from_le_bytes(content[8..12].try_into().unwrap_or([0; 4]));
            let mut pos = 12;
            for _ in 0..count {
                if pos + 2 > content.len() { break; }
                let ip_len = u16::from_le_bytes(content[pos..pos+2].try_into().unwrap_or([0; 2])) as usize;
                pos += 2;
                if pos + ip_len > content.len() { break; }
                let ip = String::from_utf8_lossy(&content[pos..pos+ip_len]).to_string();
                pos += ip_len;
                if pos + 2 > content.len() { break; }
                let port = u16::from_le_bytes(content[pos..pos+2].try_into().unwrap_or([0; 2]));
                pos += 2;
                if pos + 2 > content.len() { break; }
                let name_len = u16::from_le_bytes(content[pos..pos+2].try_into().unwrap_or([0; 2])) as usize;
                pos += 2;
                if pos + name_len > content.len() { break; }
                let name = String::from_utf8_lossy(&content[pos..pos+name_len]).to_string();
                pos += name_len;
                
                all_servers.push(McServer { name, ip, port });
            }
        }
    }

    for s in servers {
        all_servers.push(s);
    }

    let mut unique_servers = Vec::new();
    let mut seen: std::collections::HashSet<(String, u16)> = std::collections::HashSet::new();
    for s in all_servers {
        let key = (s.ip.clone(), s.port);
        if seen.insert(key) {
            unique_servers.push(s);
        }
    }

    let mut file_content = Vec::new();
    file_content.extend_from_slice(b"MCSV");
    file_content.extend_from_slice(&1u32.to_le_bytes());
    file_content.extend_from_slice(&(unique_servers.len() as u32).to_le_bytes());
    for server in unique_servers {
        let ip_bytes = server.ip.as_bytes();
        let name_bytes = server.name.as_bytes();
        file_content.extend_from_slice(&(ip_bytes.len() as u16).to_le_bytes());
        file_content.extend_from_slice(ip_bytes);
        file_content.extend_from_slice(&server.port.to_le_bytes());
        file_content.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        file_content.extend_from_slice(name_bytes);
    }
    let _ = fs::create_dir_all(instance_dir);
    let _ = fs::write(&servers_db, file_content);
}

fn perform_dlc_sync(app: &AppHandle, instance_dir: &PathBuf) -> Result<(), String> {
    let mut dlc_src = None;
    let root = get_app_dir(app);
    
    use tauri::path::BaseDirectory;
    if let Ok(p) = app.path().resolve("resources/DLC", BaseDirectory::Resource) {
        if p.exists() {
            dlc_src = Some(p);
        } else {
            if let Ok(p2) = app.path().resolve("DLC", BaseDirectory::Resource) {
                if p2.exists() { dlc_src = Some(p2); }
            }
        }
    }
    
    if dlc_src.is_none() {
        let current = std::env::current_dir().unwrap_or_default();
        let p3 = current.join("src-tauri").join("resources").join("DLC");
        let p4 = current.join("resources").join("DLC");
        if p3.exists() { dlc_src = Some(p3); }
        else if p4.exists() { dlc_src = Some(p4); }
    }

    if dlc_src.is_none() {
        let p5 = root.join("DLC");
        if p5.exists() { dlc_src = Some(p5); }
    }
    
    match dlc_src {
        Some(src) => {
            let dlc_dest = instance_dir.join("Windows64Media").join("DLC");
            let _ = fs::create_dir_all(&dlc_dest);
            
            if let Ok(entries) = fs::read_dir(&src) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let dest_path = dlc_dest.join(&name);
                    
                    if !dest_path.exists() {
                        if let Err(e) = if entry.path().is_dir() {
                            copy_dir_all(entry.path(), &dest_path)
                        } else {
                            fs::copy(entry.path(), &dest_path).map(|_| ())
                        } {
                            eprintln!("[DLC Sync] Failed to copy {:?} to {:?}: {}", entry.path(), dest_path, e);
                        } else {
                            println!("[DLC Sync] Copied to {:?}", dest_path);
                        }
                    } else {
                        println!("[DLC Sync] Skipping {:?}: Already exists in instance", name);
                    }
                }
            }
            Ok(())
        },
        None => {
            println!("[DLC Sync] Skipping sync: No DLC source found.");
            Ok(())
        }
    }
}

#[tauri::command]
async fn sync_dlc(app: AppHandle, instance_id: String) -> Result<(), String> {
    let safe_id = sanitize_path_component(&instance_id)?;
    let root = get_app_dir(&app);
    let instance_dir = root.join("instances").join(&safe_id);
    perform_dlc_sync(&app, &instance_dir)
}

#[tauri::command]
#[allow(non_snake_case)]
async fn launch_game(app: AppHandle, state: State<'_, GameState>, instance_id: String, servers: Vec<McServer>) -> Result<(), String> {
    let safe_id = sanitize_path_component(&instance_id)?;
    let root = get_app_dir(&app);
    let instance_dir = root.join("instances").join(&safe_id);
    let config = load_config(app.clone());
    let username = config.username;
    let _ = fs::write(instance_dir.join("username.txt"), &username);
    ensure_server_list(&instance_dir, servers);
    if let Some(skin_data) = config.skin_base64 {
        use base64::{Engine as _, engine::general_purpose};
        let base64_str = skin_data.split(',').nth(1).unwrap_or(&skin_data);
        if let Ok(bytes) = general_purpose::STANDARD.decode(base64_str) {
            let display_name = &username;
            let is_alex = config.skin_model.as_deref() == Some("alex");
            let pck_data = build_skin_pck(&bytes, display_name, is_alex);
            let custom_skins_dir = instance_dir.join("Windows64Media").join("DLC").join("Custom Skins");
            let _ = fs::create_dir_all(&custom_skins_dir);
            let _ = fs::write(custom_skins_dir.join("Skins.pck"), pck_data);
        }
    }

    let _ = perform_dlc_sync(&app, &instance_dir)?;

    let game_exe = instance_dir.join("Minecraft.Client.exe");
    if !game_exe.exists() {
        return Err("Game executable not found in instance folder.".into());
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(runner_id) = config.linux_runner {
            let runners = get_available_runners(app.clone());
            if let Some(runner) = runners.into_iter().find(|r| r.id == runner_id) {
                let mut cmd = if runner.r#type == "proton" {
                    let proton_exe = PathBuf::from(&runner.path).join("proton");
                    let mut c = tokio::process::Command::new(proton_exe);
                    let compat_data = instance_dir.join("proton_prefix");
                    fs::create_dir_all(&compat_data).map_err(|e| e.to_string())?;
                    c.env("STEAM_COMPAT_CLIENT_INSTALL_PATH", "");
                    c.env("STEAM_COMPAT_DATA_PATH", compat_data.to_str().ok_or_else(|| "Invalid compat data path".to_string())?);
                    c.env("SteamAppId", "480");
                    c.arg("run");
                    c
                } else {
                    tokio::process::Command::new(runner.path)
                };

                #[cfg(unix)]
                cmd.process_group(0);

                cmd.arg(&game_exe)
                   .current_dir(&instance_dir);

                let child = cmd.spawn().map_err(|e| e.to_string())?;
                {
                    let mut lock = state.child.lock().await;
                    *lock = Some(child);
                }

                let status = loop {
                    {
                        let mut lock = state.child.lock().await;
                        if let Some(ref mut c) = *lock {
                            if let Some(s) = c.try_wait().map_err(|e| e.to_string())? {
                                break s;
                            }
                        } else {
                            return Ok(());
                        }
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                };

                {
                    let mut lock = state.child.lock().await;
                    *lock = None;
                }

                return Ok(());
            }
        }
        Err("No Linux runner selected in settings.".into())
    }

    #[cfg(not(target_os = "linux"))]
    {
        #[cfg(target_os = "macos")]
        {
            let runtime_dir = get_macos_runtime_dir(&app);
            let toolkit_dir = runtime_dir.join("toolkit");
            let prefix_dir = runtime_dir.join("prefix");

            if !toolkit_dir.exists() || !prefix_dir.exists() {
                return Err("macOS Compatibility is not set up. Open Settings and run Setup macOS Compatibility.".into());
            }

            let gptk_no_hud = find_executable_recursive(&toolkit_dir, "gameportingtoolkit-no-hud")
                .or_else(|| find_executable_recursive(&toolkit_dir, "gameportingtoolkit"));

            let wine_binary = find_executable_recursive(&toolkit_dir, "wine64")
                .or_else(|| find_executable_recursive(&toolkit_dir, "wine"))
                .ok_or_else(|| "Unable to locate wine binary inside runtime.".to_string())?;

            let wine_bin_dir = wine_binary
                .parent()
                .map(|pp| pp.to_path_buf())
                .ok_or_else(|| "Unable to locate wine bin directory inside runtime.".to_string())?;

            let mut cmd = if let Some(wrapper) = gptk_no_hud {
                let win_path = unix_path_to_wine_z_path(&game_exe);
                let mut c = tokio::process::Command::new(wrapper);
                c.arg(&prefix_dir);
                c.arg(win_path);
                c
            } else {
                let mut c = tokio::process::Command::new(&wine_binary);
                c.env("WINEPREFIX", &prefix_dir);
                c.arg(&game_exe);
                c
            };

            #[cfg(unix)]
            cmd.process_group(0);

            cmd.current_dir(&instance_dir);
            cmd.env("WINEPREFIX", &prefix_dir);
            cmd.env("WINEDEBUG", "-all");
            let perf_boost = config.apple_silicon_performance_boost.unwrap_or(false);
            if perf_boost {
                #[cfg(target_arch = "aarch64")]
                {
                    cmd.env("WINE_MSYNC", "1");
                    cmd.env("MVK_ALLOW_METAL_FENCES", "1");
                }
                #[cfg(not(target_arch = "aarch64"))]
                {
                    cmd.env("WINEESYNC", "1");
                }
            } else {
                cmd.env("WINEESYNC", "1");
            }
            cmd.env("WINEDLLOVERRIDES", "winemenubuilder.exe=d;mscoree,mshtml=");
            cmd.env("MTL_HUD_ENABLED", "0");
            cmd.env("MVK_CONFIG_RESUME_LOST_DEVICE", "1");
            cmd.env(
                "PATH",
                format!(
                    "{}:{}",
                    wine_bin_dir.to_string_lossy(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            );
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());

            let child = cmd.spawn().map_err(|e| e.to_string())?;
            {
                let mut lock = state.child.lock().await;
                *lock = Some(child);
            }

            let status = loop {
                {
                    let mut lock = state.child.lock().await;
                    if let Some(ref mut c) = *lock {
                        if let Some(s) = c.try_wait().map_err(|e| e.to_string())? {
                            break s;
                        }
                    } else {
                        return Ok(());
                    }
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            };

            {
                let mut lock = state.child.lock().await;
                *lock = None;
            }

            return Ok(());
        }

        #[cfg(all(not(target_os = "macos"), not(target_os = "linux")))]
        {
            let mut cmd = tokio::process::Command::new(&game_exe);
            #[cfg(unix)]
            cmd.process_group(0);
            cmd.current_dir(&instance_dir);
            let child = cmd.spawn().map_err(|e| e.to_string())?;
            {
                let mut lock = state.child.lock().await;
                *lock = Some(child);
            }
            let _status = loop {
                {
                    let mut lock = state.child.lock().await;
                    if let Some(ref mut c) = *lock {
                        if let Some(s) = c.try_wait().map_err(|e| e.to_string())? {
                            break s;
                        }
                    } else {
                        return Ok(());
                    }
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            };
            {
                let mut lock = state.child.lock().await;
                *lock = None;
            }
            return Ok(());
        }
    }
}

#[cfg(unix)]
fn kill_process_tree(app: &AppHandle, instance_id: &str) {
    let root = get_app_dir(&app);
    let instance_dir = root.join("instances").join(instance_id);
    let target = unix_path_to_wine_z_path(&instance_dir.join("Minecraft.Client.exe"));
    let Ok(entries) = fs::read_dir("/proc") else { return };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
        let cmdline = fs::read_to_string(format!("/proc/{}/cmdline", pid))
            .unwrap_or_default();
        if cmdline.contains(&*target) {
            unsafe { libc::kill(pid as i32, libc::SIGKILL); }
        }
    }
}

#[tauri::command]
async fn stop_game(#[allow(unused_variables)] app: AppHandle, #[allow(unused_variables)] instance_id: String, state: State<'_, GameState>) -> Result<(), String> {
    let mut lock = state.child.lock().await;
    if let Some(mut child) = lock.take() {
        #[cfg(unix)] kill_process_tree(&app, &instance_id);
        let _ = child.kill().await;
    }
    Ok(())
}

#[tauri::command]
fn update_tray_icon(app: AppHandle, visible: bool) {
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_visible(visible);
    }
}

#[tauri::command]
async fn fetch_skin(username: String) -> Result<(String, String), String> {
    let client = reqwest::Client::new();
    let mojang_url = format!("https://api.mojang.com/users/profiles/minecraft/{}", username);
    let mojang_res = client.get(&mojang_url).send().await.map_err(|e| format!("Failed request to mojang: {}", e))?;
    if !mojang_res.status().is_success() {
        return Err("Player not found".to_string());
    }
    let mojang_text = mojang_res.text().await.map_err(|e| format!("Failed to read mojang text: {}", e))?;
    let mojang_data: serde_json::Value = serde_json::from_str(&mojang_text).map_err(|e| format!("Invalid Mojang JSON: {}", e))?;
    let id = mojang_data.get("id").and_then(|v| v.as_str()).ok_or_else(|| "Invalid Moajng response format".to_string())?;
    let name_exact = mojang_data.get("name").and_then(|v| v.as_str()).unwrap_or(&username).to_string();
    
    let mc_api_url = format!("https://api.minecraftapi.net/v3/profile/{}", id);
    let mc_api_res = client.get(&mc_api_url).send().await.map_err(|e| format!("Failed request to mc api: {}", e))?;
    if !mc_api_res.status().is_success() {
        return Err("Error fetching skin data".to_string());
    }
    let mc_api_text = mc_api_res.text().await.map_err(|e| format!("Failed to read mc api text: {}", e))?;
    let mc_api_data: serde_json::Value = serde_json::from_str(&mc_api_text).map_err(|e| format!("Invalid MC API JSON: {}", e))?;
    let image_b64 = mc_api_data.get("skin")
        .and_then(|s| s.get("image"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "No skin found".to_string())?;

    // Validate the decoded skin data is a valid PNG and within size limits
    {
        use base64::{Engine as _, engine::general_purpose};
        let decoded = general_purpose::STANDARD.decode(image_b64)
            .map_err(|_| "Invalid base64 skin data".to_string())?;
        if decoded.len() > MAX_SKIN_BYTES {
            return Err(format!("Skin data too large: {} bytes (max {})", decoded.len(), MAX_SKIN_BYTES));
        }
        if decoded.len() < 8 || &decoded[0..4] != b"\x89PNG" {
            return Err("Skin data is not a valid PNG image".to_string());
        }
    }

    Ok((image_b64.to_string(), name_exact))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(DownloadState { token: Arc::new(Mutex::new(None)) })
        .manage(GameState { child: Arc::new(Mutex::new(None)) })
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // When a second instance is launched, show and focus the existing window
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_gamepad::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_drpc::init())
        .invoke_handler(tauri::generate_handler![setup_macos_runtime, launch_game, stop_game, check_game_installed, save_config, load_config, download_and_install, open_instance_folder, cancel_download, get_available_runners, get_external_palettes, import_theme, download_runner, delete_instance, update_tray_icon, sync_dlc, fetch_skin, check_for_game_update, open_skins_folder, save_skin_file])
        .setup(|app| {
            let config = load_config(app.handle().clone());
            let visible = config.enable_tray_icon.unwrap_or(true);

            let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let show_i = MenuItem::with_id(app, "show", "Show", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_i, &quit_i])?;

            let tray = TrayIconBuilder::with_id("main")
                .icon(tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png")).unwrap())
                .tooltip("Revelations Launcher")
                .menu(&menu)
                .on_menu_event(|app: &AppHandle, event: MenuEvent| {
                    match event.id.as_ref() {
                        "quit" => {
                            app.exit(0);
                        }
                        "show" => {
                            if let Some(window) = app.get_webview_window("main") {
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray: &TrayIcon, event: TrayIconEvent| {
                    if let TrayIconEvent::Click { button: tauri::tray::MouseButton::Left, .. } = event {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                })
                .build(app)?;

            let _ = tray.set_visible(visible);

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let config = load_config(window.app_handle().clone());
                if config.enable_tray_icon.unwrap_or(true) {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
