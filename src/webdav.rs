use anyhow::{Context, Result};
use reqwest::Client;
use url::Url;

#[derive(Clone)]
pub struct WebDavClient {
    client: Client,
    base_url: Url,
    username: String,
    password: String,
}

#[derive(Debug, Clone)]
pub struct DavEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<chrono::DateTime<chrono::Utc>>,
}

impl WebDavClient {
    pub fn new(base_url: String, username: String, password: String) -> Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        let base_url = Url::parse(&base_url)?;

        Ok(Self {
            client,
            base_url,
            username,
            password,
        })
    }

    pub async fn test_connection(&self) -> Result<()> {
        let response = self
            .client
            .request(
                reqwest::Method::from_bytes(b"PROPFIND")?,
                self.base_url.clone(),
            )
            .basic_auth(&self.username, Some(&self.password))
            .header("Depth", "0")
            .send()
            .await
            .context("Failed to connect to WebDAV server")?;

        if !response.status().is_success() {
            anyhow::bail!("Server returned status: {}", response.status());
        }

        Ok(())
    }

    pub async fn list_dir(&self, path: &str) -> Result<Vec<DavEntry>> {
        // For root or empty path, use base_url directly
        let url = if path.is_empty() || path == "/" {
            self.base_url.clone()
        } else {
            self.base_url.join(path.trim_start_matches('/'))?
        };

        let response = self
            .client
            .request(reqwest::Method::from_bytes(b"PROPFIND")?, url.clone())
            .basic_auth(&self.username, Some(&self.password))
            .header("Depth", "1")
            .header("Content-Type", "application/xml")
            .body(
                r#"<?xml version="1.0"?>
                <d:propfind xmlns:d="DAV:">
                  <d:prop>
                    <d:displayname/>
                    <d:getcontentlength/>
                    <d:getlastmodified/>
                    <d:resourcetype/>
                  </d:prop>
                </d:propfind>"#,
            )
            .send()
            .await
            .context("Failed to list directory")?;

        if !response.status().is_success() {
            anyhow::bail!("Failed to list directory: {} (url: {})", response.status(), url);
        }

        let body = response.text().await?;
        self.parse_propfind_response(&body)
    }

    fn parse_propfind_response(&self, xml: &str) -> Result<Vec<DavEntry>> {
        // Simple XML parsing - in production use a proper XML parser like quick-xml
        let mut entries = Vec::new();

        // Split into response blocks
        let responses: Vec<&str> = xml.split("<d:response>").collect();
        
        for response in responses.iter().skip(1) { // Skip first empty part
            let mut name = String::new();
            let mut is_dir = false;
            
            // Extract displayname or href
            for line in response.lines() {
                if line.contains("<d:displayname>") {
                    if let Some(n) = extract_tag_content(line, "d:displayname") {
                        name = n;
                    }
                } else if name.is_empty() && line.contains("<d:href>") {
                    // Fallback to href if no displayname
                    if let Some(href) = extract_tag_content(line, "d:href") {
                        // Extract last path component
                        let path = href.trim_end_matches('/');
                        if let Some(last) = path.split('/').last() {
                            name = last.to_string();
                        }
                    }
                }
                
                // Check if it's a collection (directory)
                if line.contains("<d:collection") || line.contains("<d:collection/>") {
                    is_dir = true;
                }
            }
            
            // Add entry if we have a name and it's not the parent directory
            if !name.is_empty() && name != "." && !name.contains("..") {
                entries.push(DavEntry {
                    name,
                    is_dir,
                    size: 0,
                    modified: None,
                });
            }
        }

        Ok(entries)
    }

    pub async fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        let url = self.base_url.join(path)?;

        let response = self
            .client
            .get(url)
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
            .context("Failed to download file")?;

        if !response.status().is_success() {
            anyhow::bail!("Failed to read file: {}", response.status());
        }

        Ok(response.bytes().await?.to_vec())
    }
}

fn extract_tag_content(line: &str, tag: &str) -> Option<String> {
    let start_tag = format!("<{}>", tag);
    let end_tag = format!("</{}>", tag);

    if let Some(start) = line.find(&start_tag) {
        if let Some(end) = line.find(&end_tag) {
            let content_start = start + start_tag.len();
            if content_start < end {
                return Some(line[content_start..end].trim().to_string());
            }
        }
    }
    None
}
