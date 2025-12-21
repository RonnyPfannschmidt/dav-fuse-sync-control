use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountConfig {
    pub name: String,
    pub url: String,
    pub username: String,
    pub mount_point: PathBuf,
}
