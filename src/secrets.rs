use anyhow::{Context, Result};
use secret_service::SecretService;
use secret_service::EncryptionType;
use std::collections::HashMap;

use crate::config::MountConfig;

pub struct SecretStore {
    service: SecretService<'static>,
}

impl SecretStore {
    pub async fn new() -> Result<Self> {
        let service = SecretService::connect(EncryptionType::Dh)
            .await
            .context("Failed to connect to Secret Service")?;

        Ok(Self { service })
    }

    pub async fn store_mount_config(&self, name: &str, config: &MountConfig) -> Result<()> {
        let config_json = serde_json::to_string(config)?;
        
        let collection = self.service
            .get_default_collection()
            .await
            .context("Failed to get default collection")?;

        let mut attributes = HashMap::new();
        attributes.insert("application", "davfs-sync");
        attributes.insert("type", "config");
        attributes.insert("mount", name);

        collection
            .create_item(
                &format!("davfs-sync config: {}", name),
                attributes,
                config_json.as_bytes(),
                true, // replace existing
                "text/plain",
            )
            .await
            .context("Failed to store config")?;

        Ok(())
    }

    pub async fn load_mount_config(&self, name: &str) -> Result<MountConfig> {
        let collection = self.service
            .get_default_collection()
            .await
            .context("Failed to get default collection")?;

        let mut attributes = HashMap::new();
        attributes.insert("application", "davfs-sync");
        attributes.insert("type", "config");
        attributes.insert("mount", name);

        let items = collection
            .search_items(attributes)
            .await
            .context("Failed to search for config")?;

        let item = items
            .first()
            .context("Mount config not found")?;

        let secret = item.get_secret()
            .await
            .context("Failed to get secret")?;
        
        let config: MountConfig = serde_json::from_str(&String::from_utf8(secret)?)?;
        Ok(config)
    }

    pub async fn store_password(&self, name: &str, password: &str) -> Result<()> {
        let collection = self.service
            .get_default_collection()
            .await
            .context("Failed to get default collection")?;

        let mut attributes = HashMap::new();
        attributes.insert("application", "davfs-sync");
        attributes.insert("type", "password");
        attributes.insert("mount", name);

        collection
            .create_item(
                &format!("davfs-sync password: {}", name),
                attributes,
                password.as_bytes(),
                true, // replace existing
                "text/plain",
            )
            .await
            .context("Failed to store password")?;

        Ok(())
    }

    pub async fn load_password(&self, name: &str) -> Result<String> {
        let collection = self.service
            .get_default_collection()
            .await
            .context("Failed to get default collection")?;

        let mut attributes = HashMap::new();
        attributes.insert("application", "davfs-sync");
        attributes.insert("type", "password");
        attributes.insert("mount", name);

        let items = collection
            .search_items(attributes)
            .await
            .context("Failed to search for password")?;

        let item = items
            .first()
            .context("Password not found")?;

        let secret = item.get_secret()
            .await
            .context("Failed to get secret")?;
        
        Ok(String::from_utf8(secret)?)
    }

    pub async fn list_mounts(&self) -> Result<Vec<String>> {
        let collection = self.service
            .get_default_collection()
            .await
            .context("Failed to get default collection")?;

        let mut attributes = HashMap::new();
        attributes.insert("application", "davfs-sync");
        attributes.insert("type", "config");

        let items = collection
            .search_items(attributes)
            .await
            .context("Failed to search for mounts")?;

        let mut mount_names = Vec::new();
        for item in items {
            let attrs = item.get_attributes()
                .await
                .context("Failed to get attributes")?;
            if let Some(name) = attrs.get("mount") {
                mount_names.push(name.clone());
            }
        }

        Ok(mount_names)
    }
}
