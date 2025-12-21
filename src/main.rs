use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod config;
mod filesystem;
mod secrets;
mod webdav;
mod cache;

use config::MountConfig;
use filesystem::DavFS;

#[derive(Parser)]
#[command(name = "davfs-sync")]
#[command(about = "WebDAV FUSE filesystem with offline support", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
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

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "davfs_sync=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Setup {
            name,
            url,
            username,
            mount_point,
        } => {
            setup_mount(name, url, username, mount_point).await?;
        }
        Commands::Mount { name } => {
            mount_filesystem(name).await?;
        }
        Commands::List => {
            list_mounts().await?;
        }
        Commands::SetupFromNextcloud {
            name,
            remote_path,
            mount_point,
        } => {
            setup_from_nextcloud(name, remote_path, mount_point).await?;
        }
    }

    Ok(())
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
                    anyhow::bail!("Failed to unmount existing mount at {}. Please run: fusermount3 -uz {}", 
                        config.mount_point.display(), config.mount_point.display());
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
    let webdav = webdav::WebDavClient::new(
        config.url.clone(),
        config.username.clone(),
        password,
    )?;

    // Test connection
    println!("Testing connection...");
    match webdav.test_connection().await {
        Ok(_) => println!("✓ Connected successfully!"),
        Err(e) => {
            eprintln!("✗ Connection failed: {}", e);
            eprintln!("The filesystem will mount, but operations will fail until connection is available.");
        }
    }

    // Create filesystem
    let fs = DavFS::new(webdav);

    println!("\nMounting filesystem at {}...", config.mount_point.display());
    println!("Press Ctrl+C to unmount\n");

    // Mount options - minimal set to avoid permission issues
    let options = vec![
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
    let mount_result = tokio::task::spawn_blocking(move || {
        fuser::mount2(fs, &mount_point, &options)
    }).await?;

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
        format!("{}/.var/app/com.nextcloud.desktopclient.nextcloud/config/Nextcloud/nextcloud.cfg", home),
    ];
    
    let mut config_content = None;
    let mut config_path_used = None;
    
    for path in &config_paths {
        if let Ok(content) = std::fs::read_to_string(path) {
            println!("✓ Found config at: {}", path);
            config_content = Some(content);
            config_path_used = Some(path.clone());
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
        } else if line.starts_with("http://") || line.starts_with("https://") {
            if server_url.is_none() {
                server_url = Some(line.to_string());
                println!("  Found URL in line: {}", line);
            }
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
        } else if line.starts_with("webflow_user=") || line.contains("\\webflow_user=") {
            if username.is_none() {
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
    }
    
    if server_url.is_none() || username.is_none() {
        println!("\n⚠ Could not parse all required fields. Config file content:");
        println!("---");
        for (i, line) in config_content.lines().take(100).enumerate() {
            println!("{:3}: {}", i+1, line);
        }
        println!("---\n");
    }

    let server_url = server_url.ok_or_else(|| anyhow::anyhow!("Server URL not found in Nextcloud config"))?;
    let username = username.ok_or_else(|| anyhow::anyhow!("Username not found in Nextcloud config"))?;

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
    let password = match try_get_nextcloud_password(&secret_store, &username, base_url).await {
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

async fn try_get_nextcloud_password(
    _secret_store: &secrets::SecretStore,
    username: &str,
    _server_url: &str,
) -> Result<String> {
    use std::collections::HashMap;
    use secret_service::{SecretService, EncryptionType};
    
    // Try to find Nextcloud password in keyring
    // Nextcloud Desktop stores passwords with various attribute combinations
    let service = SecretService::connect(EncryptionType::Dh).await?;
    let collection = service.get_default_collection().await?;
    
    // Try different search patterns
    let search_patterns = vec![
        vec![
            ("application", "Nextcloud"),
            ("user", username),
        ],
        vec![
            ("application", "org.kde.kwalletd5"),
            ("folder", "Nextcloud"),
        ],
        vec![
            ("application", "Nextcloud Desktop"),
        ],
    ];
    
    for pattern in search_patterns {
        let mut attrs = HashMap::new();
        for (key, value) in pattern {
            attrs.insert(key, value);
        }
        
        if let Ok(items) = collection.search_items(attrs).await {
            if let Some(item) = items.first() {
                if let Ok(secret) = item.get_secret().await {
                    if let Ok(password) = String::from_utf8(secret) {
                        return Ok(password);
                    }
                }
            }
        }
    }
    
    anyhow::bail!("Password not found in keyring")
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
