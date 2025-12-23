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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
        let (entries, _etag) = self.list_dir_with_etag(path, None).await?;
        Ok(entries)
    }
    
    pub async fn list_dir_with_etag(&self, path: &str, if_none_match: Option<&str>) -> Result<(Vec<DavEntry>, Option<String>)> {
        self.list_dir_with_etag_depth(path, if_none_match, "1").await
    }
    
    pub async fn list_dir_recursive(&self, path: &str, max_depth: usize) -> Result<std::collections::HashMap<String, Vec<DavEntry>>> {
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
            .header("Depth", "infinity")
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
            .context("Failed to list directory recursively")?;

        if !response.status().is_success() {
            anyhow::bail!("Failed to list directory recursively: {} (url: {})", response.status(), url);
        }

        let body = response.text().await?;
        self.parse_propfind_recursive(&body, path, max_depth)
    }
    
    async fn list_dir_with_etag_depth(&self, path: &str, if_none_match: Option<&str>, depth: &str) -> Result<(Vec<DavEntry>, Option<String>)> {
        // For root or empty path, use base_url directly
        let url = if path.is_empty() || path == "/" {
            self.base_url.clone()
        } else {
            self.base_url.join(path.trim_start_matches('/'))?
        };

        let mut request = self
            .client
            .request(reqwest::Method::from_bytes(b"PROPFIND")?, url.clone())
            .basic_auth(&self.username, Some(&self.password))
            .header("Depth", depth)
            .header("Content-Type", "application/xml");
        
        // Add If-None-Match header for conditional request
        if let Some(etag) = if_none_match {
            tracing::info!("Making conditional request for {} with If-None-Match: {}", path, etag);
            request = request.header("If-None-Match", etag);
        }
        
        let response = request
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

        // Extract ETag from response headers
        let etag = response.headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        
        // Handle HTTP 304 Not Modified
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            tracing::info!("Got HTTP 304 Not Modified for {}, content unchanged", path);
            // Return empty vec - caller should use cached data
            return Ok((Vec::new(), etag));
        }

        if !response.status().is_success() {
            anyhow::bail!("Failed to list directory: {} (url: {})", response.status(), url);
        }
        
        tracing::info!("Got HTTP 200 for {} with etag: {:?}", path, etag);

        let body = response.text().await?;
        let entries = self.parse_propfind_response(&body)?;
        Ok((entries, etag))
    }

    fn parse_propfind_response(&self, xml: &str) -> Result<Vec<DavEntry>> {
        // Simple XML parsing - in production use a proper XML parser like quick-xml
        let mut entries = Vec::new();

        // Split into response blocks
        let responses: Vec<&str> = xml.split("<d:response>").collect();
        
        for response in responses.iter().skip(1) { // Skip first empty part
            let mut name = String::new();
            let mut is_dir = false;
            let mut size: u64 = 0;
            let mut modified: Option<chrono::DateTime<chrono::Utc>> = None;
            
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
                
                // Extract file size
                if line.contains("<d:getcontentlength>") {
                    if let Some(size_str) = extract_tag_content(line, "d:getcontentlength") {
                        size = size_str.parse().unwrap_or(0);
                    }
                }
                
                // Extract last modified date
                if line.contains("<d:getlastmodified>") {
                    if let Some(date_str) = extract_tag_content(line, "d:getlastmodified") {
                        // Parse RFC 2822 format (HTTP date format)
                        if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(&date_str) {
                            modified = Some(dt.with_timezone(&chrono::Utc));
                        }
                    }
                }
            }
            
            // Add entry if we have a name and it's not the parent directory
            if !name.is_empty() && name != "." && !name.contains("..") {
                entries.push(DavEntry {
                    name,
                    is_dir,
                    size,
                    modified,
                });
            }
        }

        Ok(entries)
    }
    
    fn parse_propfind_recursive(&self, xml: &str, base_path: &str, max_depth: usize) -> Result<std::collections::HashMap<String, Vec<DavEntry>>> {
        use std::collections::HashMap;
        
        let mut directories: HashMap<String, Vec<DavEntry>> = HashMap::new();
        let base_url_str = self.base_url.as_str().trim_end_matches('/');
        
        // Split into response blocks
        let responses: Vec<&str> = xml.split("<d:response>").collect();
        
        for response in responses.iter().skip(1) {
            let mut href = String::new();
            let mut is_dir = false;
            let mut size: u64 = 0;
            let mut modified: Option<chrono::DateTime<chrono::Utc>> = None;
            
            // Extract href and properties
            for line in response.lines() {
                if line.contains("<d:href>") {
                    if let Some(h) = extract_tag_content(line, "d:href") {
                        href = h;
                    }
                }
                
                if line.contains("<d:collection") || line.contains("<d:collection/>") {
                    is_dir = true;
                }
                
                if line.contains("<d:getcontentlength>") {
                    if let Some(size_str) = extract_tag_content(line, "d:getcontentlength") {
                        size = size_str.parse().unwrap_or(0);
                    }
                }
                
                if line.contains("<d:getlastmodified>") {
                    if let Some(date_str) = extract_tag_content(line, "d:getlastmodified") {
                        if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(&date_str) {
                            modified = Some(dt.with_timezone(&chrono::Utc));
                        }
                    }
                }
            }
            
            if href.is_empty() {
                continue;
            }
            
            // Remove base URL prefix and decode
            let path = href.strip_prefix(base_url_str).unwrap_or(&href);
            let path = urlencoding::decode(path).unwrap_or_default().to_string();
            let path = path.trim_start_matches('/').trim_end_matches('/');
            
            // Skip if beyond max depth
            let depth = if path.is_empty() { 0 } else { path.matches('/').count() + 1 };
            if depth > max_depth {
                continue;
            }
            
            // Determine parent directory
            let (parent_dir, name) = if path.is_empty() || path == base_path.trim_matches('/') {
                // This is the root of our query, skip it
                continue;
            } else if let Some(idx) = path.rfind('/') {
                let parent = &path[..idx];
                let name = &path[idx+1..];
                (format!("/{}", parent), name.to_string())
            } else {
                ("/".to_string(), path.to_string())
            };
            
            // Add entry to its parent directory
            directories.entry(parent_dir).or_insert_with(Vec::new).push(DavEntry {
                name,
                is_dir,
                size,
                modified,
            });
        }
        
        Ok(directories)
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
