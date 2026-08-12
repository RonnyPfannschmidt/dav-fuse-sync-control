//! Command-line interface.
//!
//! Moved out of `main.rs` verbatim so the binary is a thin entry point and this
//! logic is reachable from integration tests.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::sync::Arc;

use crate::config::MountConfig;
use crate::creds::{CredentialSource, ProcessEnv};
use crate::daemon;
use crate::dropin;
use crate::remote::{RemoteStorage, WebDavBackend};
use crate::secrets;
use crate::webdav;

#[derive(Parser)]
#[command(name = "davfs-sync")]
#[command(about = "WebDAV FUSE filesystem with offline support", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Configure a new mount
    Setup {
        /// Name of the mount
        name: String,

        /// WebDAV URL
        #[arg(long)]
        url: String,

        /// Username
        #[arg(long)]
        username: String,

        /// Mount point path
        #[arg(long)]
        mount_point: String,
    },

    /// Mount in the foreground. Ctrl+C unmounts.
    Mount {
        /// Name of the mount to use
        name: String,
        /// Force-unmount anything already at the mount point first.
        ///
        /// Never done automatically, and never in `daemon` mode: under
        /// `Restart=` a restarting instance could otherwise tear down a
        /// different live mount.
        #[arg(long)]
        force: bool,
    },

    /// Mount under systemd. Configuration comes from $DAVFS_* only.
    Daemon,

    /// Start a mount's systemd unit.
    Start { name: String },
    /// Stop a mount's systemd unit.
    Stop { name: String },
    /// Show a mount's systemd unit status.
    Status { name: String },

    /// List configured mounts
    List,

    /// Setup mount using Nextcloud Desktop credentials
    SetupFromNextcloud {
        /// Name for this mount
        name: String,

        /// Remote path on WebDAV server (e.g., /Photos or /Documents)
        #[arg(long, default_value = "/")]
        remote_path: String,

        /// Mount point path
        #[arg(long)]
        mount_point: String,
    },
}

pub async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Setup {
            name,
            url,
            username,
            mount_point,
        } => setup_mount(name, url, username, mount_point).await,
        Commands::Mount { name, force } => mount_foreground(name, force).await,
        Commands::Daemon => run_daemon().await,
        Commands::Start { name } => systemctl(&["start", &unit(&name)]),
        Commands::Stop { name } => systemctl(&["stop", &unit(&name)]),
        Commands::Status { name } => systemctl(&["status", &unit(&name)]),
        Commands::List => list_mounts().await,
        Commands::SetupFromNextcloud {
            name,
            remote_path,
            mount_point,
        } => setup_from_nextcloud(name, remote_path, mount_point).await,
    }
}

async fn setup_mount(
    name: String,
    url: String,
    username: String,
    mount_point: String,
) -> Result<()> {
    use rpassword::read_password;
    use std::io::Write;

    println!("Setting up mount: {}", name);
    println!("URL: {}", url);
    println!("Username: {}", username);
    println!("Mount point: {}", mount_point);

    // Prompt for password
    print!("Password: ");
    std::io::stdout().flush()?;
    let password = read_password()?;

    // Create config
    let config = MountConfig::new(name.clone(), url, username, mount_point.into());

    // Store config and password in Secret Service
    let secret_store = secrets::SecretStore::new().await?;
    secret_store.store_mount_config(&name, &config).await?;
    secret_store.store_password(&name, &password).await?;

    finish_setup(&name, &config)
}

/// Write the systemd drop-in and tell the user what to do next.
///
/// The drop-in carries the same values the foreground path reads from the
/// keyring, so `mount` and `daemon` see an identical configuration.
fn finish_setup(name: &str, config: &MountConfig) -> Result<()> {
    dropin::validate_name(name)?;
    println!("\n\u{2713} Mount '{}' configured.", name);

    match dropin::write(name, &config.to_env()) {
        Ok(path) => {
            println!("  systemd drop-in: {}", path.display());
            // Without this systemd will not see the new drop-in.
            let reloaded = std::process::Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !reloaded {
                println!("  (run `systemctl --user daemon-reload` by hand)");
            }
            println!("\nRun in the foreground:");
            println!("  davfs-sync mount {name}");
            println!("\nOr as a service (needs the unit files from systemd/ installed):");
            println!("  systemctl --user enable --now davfs-sync@{name}.service");
        }
        Err(e) => {
            // Not fatal: the foreground path needs no drop-in at all.
            println!("  (could not write the systemd drop-in: {e})");
            println!("\nTo mount:");
            println!("  davfs-sync mount {name}");
        }
    }
    Ok(())
}

fn unit(name: &str) -> String {
    format!("davfs-sync@{name}.service")
}

/// Thin wrapper over `systemctl --user`.
fn systemctl(args: &[&str]) -> Result<()> {
    let status = std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("running systemctl")?;
    if !status.success() {
        anyhow::bail!("systemctl --user {} failed", args.join(" "));
    }
    Ok(())
}

/// Build the backend for a configured mount.
async fn backend_for(config: &MountConfig, password: String) -> Result<Arc<dyn RemoteStorage>> {
    match &config.backend {
        crate::config::Backend::WebDav => {
            let client = webdav::WebDavClient::with_timeout(
                config.url.clone(),
                config.username.clone(),
                password,
                std::time::Duration::from_secs(config.timeout_sec),
            )?;
            Ok(Arc::new(WebDavBackend::new(client)))
        }
        crate::config::Backend::GoogleDrive { .. } => {
            anyhow::bail!("the Google Drive backend is not implemented yet")
        }
    }
}

fn params_for(config: &MountConfig) -> Result<daemon::MountParams> {
    Ok(daemon::MountParams {
        name: config.name.clone(),
        mount_point: config.mount_point.clone(),
        data_dir: config.data_dir()?,
        cache_dir: config.cache_dir()?,
        backend_tag: config.backend.tag().to_string(),
        vfs: config.vfs_config(),
        sync: config.sync_config(),
        allow_other: config.allow_other,
        fuse_threads: config.fuse_threads,
        initial_sync_depth: config.initial_sync_depth,
    })
}

/// `davfs-sync mount <name>`: configuration from the keyring, overlaid with any
/// `DAVFS_*` in the environment, so this and `daemon` agree.
async fn mount_foreground(name: String, force: bool) -> Result<()> {
    let store = secrets::SecretStore::new().await?;
    let mut config = store.load_mount_config(&name).await?;
    config.name = name.clone();
    config.apply_env(&ProcessEnv);

    if force {
        force_unmount(&config.mount_point);
    } else if daemon::is_fuse_mount(&config.mount_point) {
        anyhow::bail!(
            "{} is already mounted; unmount it or pass --force",
            config.mount_point.display()
        );
    }

    let password = store.load_password(&name).await?;
    let remote = backend_for(&config, password).await?;

    println!(
        "Mounting {} at {}",
        config.url,
        config.mount_point.display()
    );
    println!("Press Ctrl+C to unmount\n");
    daemon::run(remote, params_for(&config)?).await
}

/// `davfs-sync daemon`: the systemd entry point. Everything comes from the
/// environment, and credentials are resolved through the same source chain the
/// unit's drop-in selects.
async fn run_daemon() -> Result<()> {
    let config = MountConfig::from_env(&ProcessEnv)?;
    let source = CredentialSource::detect(&ProcessEnv)?;
    tracing::info!("credential source: {:?}", source);

    let password = source.password(&config.name, &ProcessEnv).await?;
    let remote = backend_for(&config, password).await?;
    daemon::run(remote, params_for(&config)?).await
}

/// Only ever from `mount --force`, never from `daemon`.
fn force_unmount(mount_point: &std::path::Path) {
    if !daemon::is_fuse_mount(mount_point) {
        return;
    }
    eprintln!("Unmounting existing mount at {}", mount_point.display());
    for args in [["-u", ""], ["-uz", ""]] {
        let mut cmd = std::process::Command::new("fusermount3");
        cmd.arg(args[0]).arg(mount_point);
        if cmd.status().map(|s| s.success()).unwrap_or(false) {
            std::thread::sleep(std::time::Duration::from_millis(300));
            if !daemon::is_fuse_mount(mount_point) {
                return;
            }
        }
    }
    eprintln!("Warning: could not unmount {}", mount_point.display());
}

async fn setup_from_nextcloud(
    name: String,
    remote_path: String,
    mount_point: String,
) -> Result<()> {
    println!("Looking for Nextcloud Desktop credentials...\n");

    // Try to read Nextcloud config from multiple locations
    let home = std::env::var("HOME")?;
    let config_paths = vec![
        // Regular installation
        format!("{}/.config/Nextcloud/nextcloud.cfg", home),
        // Flatpak installation
        format!(
            "{}/.var/app/com.nextcloud.desktopclient.nextcloud/config/Nextcloud/nextcloud.cfg",
            home
        ),
    ];

    let mut config_content = None;

    for path in &config_paths {
        if let Ok(content) = std::fs::read_to_string(path) {
            println!("✓ Found config at: {}", path);
            config_content = Some(content);
            break;
        }
    }

    let config_content = match config_content {
        Some(c) => c,
        None => {
            eprintln!("Error: Could not find Nextcloud Desktop config.");
            eprintln!("\nSearched locations:");
            for path in &config_paths {
                eprintln!("  - {}", path);
            }
            eprintln!("\nMake sure Nextcloud Desktop is installed and configured.");
            anyhow::bail!("Nextcloud Desktop config not found");
        }
    };

    // Parse config to extract account info
    let mut server_url = None;
    let mut username = None;

    println!("\nParsing config file...");

    for line in config_content.lines() {
        let line = line.trim();

        // Try different URL formats (with or without number prefix)
        if line.starts_with("url=") || line.contains("\\url=") {
            let url = if let Some(stripped) = line.strip_prefix("url=") {
                stripped
            } else if let Some(pos) = line.find("\\url=") {
                &line[pos + 5..]
            } else {
                continue;
            };
            server_url = Some(url.to_string());
            println!("  Found url={}", url);
        } else if (line.starts_with("http://") || line.starts_with("https://"))
            && server_url.is_none()
        {
            server_url = Some(line.to_string());
            println!("  Found URL in line: {}", line);
        }

        // Try different username formats (with or without number prefix)
        if line.starts_with("user=") || line.contains("\\user=") {
            let user = if let Some(stripped) = line.strip_prefix("user=") {
                stripped
            } else if let Some(pos) = line.find("\\user=") {
                &line[pos + 6..]
            } else {
                continue;
            };
            username = Some(user.to_string());
            println!("  Found user={}", user);
        } else if line.starts_with("davUser=") || line.contains("\\davUser=") {
            let user = if let Some(stripped) = line.strip_prefix("davUser=") {
                stripped
            } else if let Some(pos) = line.find("\\davUser=") {
                &line[pos + 9..]
            } else {
                continue;
            };
            username = Some(user.to_string());
            println!("  Found davUser={}", user);
        } else if (line.starts_with("webflow_user=") || line.contains("\\webflow_user="))
            && username.is_none()
        {
            let user = if let Some(stripped) = line.strip_prefix("webflow_user=") {
                stripped
            } else if let Some(pos) = line.find("\\webflow_user=") {
                &line[pos + 14..]
            } else {
                continue;
            };
            username = Some(user.to_string());
            println!("  Found webflow_user={}", user);
        }
    }

    if server_url.is_none() || username.is_none() {
        println!("\n⚠ Could not parse all required fields. Config file content:");
        println!("---");
        for (i, line) in config_content.lines().take(100).enumerate() {
            println!("{:3}: {}", i + 1, line);
        }
        println!("---\n");
    }

    let server_url =
        server_url.ok_or_else(|| anyhow::anyhow!("Server URL not found in Nextcloud config"))?;
    let username =
        username.ok_or_else(|| anyhow::anyhow!("Username not found in Nextcloud config"))?;

    // Construct WebDAV URL
    // Nextcloud WebDAV is at: <server>/remote.php/dav/files/<username>/<path>
    let base_url = server_url.trim_end_matches('/');
    let webdav_url = format!(
        "{}/remote.php/dav/files/{}/{}",
        base_url,
        username,
        remote_path.trim_start_matches('/')
    );

    println!("Found Nextcloud account:");
    println!("  Server:   {}", base_url);
    println!("  Username: {}", username);
    println!("  WebDAV:   {}", webdav_url);
    println!();

    // Try to get password from Secret Service
    let secret_store = secrets::SecretStore::new().await?;

    println!("Attempting to retrieve password from keyring...");

    // Try common Nextcloud keyring entries
    let password = match secret_store.find_nextcloud_password(&username).await {
        Ok(pw) => {
            println!("✓ Password retrieved from keyring!");
            pw
        }
        Err(e) => {
            println!("✗ Could not retrieve password from keyring: {}", e);
            println!("\nPlease enter password manually:");

            use rpassword::read_password;
            use std::io::Write;

            print!("Password: ");
            std::io::stdout().flush()?;
            read_password()?
        }
    };

    // Create config
    let config = MountConfig::new(
        name.clone(),
        webdav_url,
        username.clone(),
        mount_point.into(),
    );

    // Store config and password
    secret_store.store_mount_config(&name, &config).await?;
    secret_store.store_password(&name, &password).await?;

    finish_setup(&name, &config)
}

async fn list_mounts() -> Result<()> {
    let secret_store = secrets::SecretStore::new().await?;
    let mounts = secret_store.list_mounts().await?;

    if mounts.is_empty() {
        println!("No mounts configured.");
        println!("\nTo add a mount:");
        println!("  davfs-sync setup <name> --url <url> --username <user> --mount-point <path>");
        return Ok(());
    }

    println!("Configured mounts:\n");
    for name in mounts {
        if let Ok(config) = secret_store.load_mount_config(&name).await {
            println!("  {} ", name);
            println!("    URL:         {}", config.url);
            println!("    Username:    {}", config.username);
            println!("    Mount point: {}", config.mount_point.display());
            println!();
        }
    }

    Ok(())
}
