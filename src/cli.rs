//! Command-line interface.
//!
//! Moved out of `main.rs` verbatim so the binary is a thin entry point and this
//! logic is reachable from integration tests.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::config::MountConfig;
use crate::filesystem::DavFS;
use crate::secrets;
use crate::store::MetaStore;
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

    /// Mount filesystem (stays in foreground)
    Mount {
        /// Name of the mount to use
        name: String,
    },

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
        Commands::Mount { name } => mount_filesystem(name).await,
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
    let config = MountConfig {
        name: name.clone(),
        url,
        username,
        mount_point: mount_point.into(),
    };

    // Store config and password in Secret Service
    let secret_store = secrets::SecretStore::new().await?;
    secret_store.store_mount_config(&name, &config).await?;
    secret_store.store_password(&name, &password).await?;

    println!("\n✓ Mount '{}' configured successfully!", name);
    println!("\nTo mount:");
    println!("  davfs-sync mount {}", name);

    Ok(())
}

/// Populate the cache before the filesystem is exposed.
///
/// A breadth-first crawl of `Depth: 1` listings, not one `Depth: infinity`
/// request. Nextcloud and SabreDAV reject `Depth: infinity` outright with
/// `403 Propfind-Finite-Depth`, so the PoC's "one recursive PROPFIND" silently
/// failed against the exact servers it was written for, and the mount fell back
/// to fetching every directory synchronously on first access.
///
/// Each level is fetched concurrently, bounded by `max_concurrent`.
async fn initial_sync(
    webdav: &webdav::WebDavClient,
    cache: &MetaStore,
    max_depth: usize,
    max_concurrent: usize,
) -> Result<usize> {
    use futures::stream::{self, StreamExt};

    let mut frontier = vec![String::from("/")];
    let mut cached = 0usize;

    for depth in 0..max_depth {
        if frontier.is_empty() {
            break;
        }
        tracing::info!("initial sync: level {} ({} dirs)", depth, frontier.len());

        let results: Vec<(String, Option<Vec<webdav::DavEntry>>, Option<String>)> =
            stream::iter(frontier.iter().cloned())
                .map(|path| async move {
                    let dav_path = path.trim_start_matches('/');
                    match webdav.list_dir(dav_path, None).await {
                        Ok(webdav::ListResult::Modified { entries, etag }) => {
                            (path, Some(entries), etag)
                        }
                        // No validator was sent, so this cannot happen; treat it
                        // as "nothing to store" rather than asserting.
                        Ok(webdav::ListResult::NotModified) => (path, None, None),
                        Err(e) => {
                            tracing::warn!("initial sync: {} failed: {}", path, e);
                            (path, None, None)
                        }
                    }
                })
                .buffer_unordered(max_concurrent)
                .collect()
                .await;

        let mut next = Vec::new();
        for (path, entries, etag) in results {
            let Some(entries) = entries else { continue };

            for e in entries.iter().filter(|e| e.is_dir) {
                next.push(if path == "/" {
                    format!("/{}", e.name)
                } else {
                    format!("{}/{}", path, e.name)
                });
            }

            // The validator is stored, so the first background revalidation of
            // each directory is conditional. The PoC passed `None` here, which
            // meant every directory was re-fetched in full at least once.
            cache.insert(path, entries, etag).await?;
            cached += 1;
        }
        frontier = next;
    }

    Ok(cached)
}

async fn mount_filesystem(name: String) -> Result<()> {
    println!("Loading mount configuration for '{}'...", name);

    // Load config from Secret Service
    let secret_store = secrets::SecretStore::new().await?;
    let config = secret_store.load_mount_config(&name).await?;
    let password = secret_store.load_password(&name).await?;

    println!("Connecting to: {}", config.url);
    println!("Mount point: {}", config.mount_point.display());

    // Check if mount point is already mounted and try to unmount it
    println!("Checking for existing mounts...");

    // First, check if it's mounted using mountpoint command
    let check_mounted = std::process::Command::new("mountpoint")
        .arg("-q")
        .arg(&config.mount_point)
        .status();

    let is_mounted = check_mounted.map(|s| s.success()).unwrap_or(false);

    if is_mounted {
        println!("Found existing mount, attempting to unmount...");
        let unmount_result = std::process::Command::new("fusermount3")
            .arg("-u")
            .arg(&config.mount_point)
            .status();

        match unmount_result {
            Ok(status) if status.success() => {
                println!("✓ Successfully unmounted existing mount");
                // Give it a moment to fully unmount
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            Ok(_) => {
                // Try lazy unmount as fallback
                println!("Regular unmount failed, trying lazy unmount...");
                let lazy_result = std::process::Command::new("fusermount3")
                    .arg("-uz")
                    .arg(&config.mount_point)
                    .status();

                if lazy_result.map(|s| s.success()).unwrap_or(false) {
                    println!("✓ Lazy unmount succeeded");
                    std::thread::sleep(std::time::Duration::from_millis(500));
                } else {
                    anyhow::bail!(
                        "Failed to unmount existing mount at {}. Please run: fusermount3 -uz {}",
                        config.mount_point.display(),
                        config.mount_point.display()
                    );
                }
            }
            Err(e) => {
                eprintln!("Warning: Could not unmount: {}", e);
            }
        }
    } else {
        // Even if not mounted, might be a stale directory - try unmounting anyway
        let _ = std::process::Command::new("fusermount3")
            .arg("-uz")
            .arg(&config.mount_point)
            .output();
    }

    // Create mount point if it doesn't exist
    std::fs::create_dir_all(&config.mount_point)?;

    // Create WebDAV client
    let webdav = webdav::WebDavClient::new(config.url.clone(), config.username.clone(), password)?;

    // Test connection
    println!("Testing connection...");
    match webdav.test_connection().await {
        Ok(_) => println!("✓ Connected successfully!"),
        Err(e) => {
            eprintln!("✗ Connection failed: {}", e);
            eprintln!("The filesystem will mount, but operations will fail until connection is available.");
        }
    }

    // Get cache database path - use XDG data directory
    let cache_dir = dirs::data_local_dir()
        .context("Failed to get local data directory")?
        .join("davfs-sync");

    // Ensure cache directory exists
    std::fs::create_dir_all(&cache_dir)?;

    let cache_db_file = cache_dir.join(format!("{}.db", name));
    let cache_db_path = format!("sqlite://{}", cache_db_file.display());
    println!("Using cache database: {}", cache_db_path);

    // Initialize cache (must happen in async context)
    println!("Initializing cache...");
    let cache = MetaStore::new(&cache_db_path, std::time::Duration::from_secs(5)).await?;
    println!("✓ Cache initialized");

    // Perform initial sync before creating filesystem
    println!("Starting initial sync...");
    match initial_sync(&webdav, &cache, 3, 4).await {
        Ok(dir_count) => {
            println!("✓ Initial sync complete! Cached {} directories", dir_count);
        }
        Err(e) => {
            eprintln!("⚠ Initial sync failed: {}", e);
            eprintln!("Will mount with stale cache (if available)");
        }
    }

    // Create filesystem with populated cache
    let fs = DavFS::new(
        tokio::runtime::Handle::current(),
        webdav,
        cache,
        crate::fuse::OwnerIds::current(),
    );

    println!(
        "\nMounting filesystem at {}...",
        config.mount_point.display()
    );
    println!("Press Ctrl+C to unmount\n");

    // fuser 0.18 takes a Config rather than a bare Vec<MountOption>.
    // n_threads/clone_fd are left at their defaults here; Phase 6 turns them up
    // once the handlers no longer block the event loop.
    // Config is #[non_exhaustive], so build it by mutating the default.
    let mut options = fuser::Config::default();
    options.mount_options = vec![
        fuser::MountOption::FSName("davfs-sync".to_string()),
        fuser::MountOption::RO, // Read-only for PoC
    ];

    // Setup signal handler for clean unmount
    let mount_point_for_signal = config.mount_point.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        println!("\n\nReceived Ctrl+C, unmounting...");
        // Try to unmount
        let _ = std::process::Command::new("fusermount3")
            .arg("-u")
            .arg(&mount_point_for_signal)
            .output();
        std::process::exit(0);
    });

    // Spawn blocking mount operation in a separate thread to avoid runtime conflicts
    let mount_point = config.mount_point.clone();
    let mount_result =
        tokio::task::spawn_blocking(move || fuser::mount(fs, &mount_point, &options)).await?;

    mount_result?;

    println!("\nFilesystem unmounted.");
    Ok(())
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
    let config = MountConfig {
        name: name.clone(),
        url: webdav_url,
        username: username.clone(),
        mount_point: mount_point.into(),
    };

    // Store config and password
    secret_store.store_mount_config(&name, &config).await?;
    secret_store.store_password(&name, &password).await?;

    println!("\n✓ Mount '{}' configured successfully!", name);
    println!("\nTo mount:");
    println!("  davfs-sync mount {}", name);

    Ok(())
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
