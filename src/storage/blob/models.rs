use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename = "EnumerationResults")]
pub struct ContainerListResponse {
    #[serde(rename = "Containers")]
    pub containers: Option<ContainerList>,
    #[serde(rename = "NextMarker")]
    pub next_marker: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ContainerList {
    #[serde(rename = "Container", default)]
    pub items: Vec<ContainerItem>,
}

#[derive(Debug, Deserialize)]
pub struct ContainerItem {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Properties")]
    pub properties: Option<ContainerProperties>,
}

#[derive(Debug, Deserialize)]
pub struct ContainerProperties {
    #[serde(rename = "Last-Modified")]
    pub last_modified: Option<String>,
    #[serde(rename = "Etag")]
    pub etag: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename = "EnumerationResults")]
pub struct BlobListResponse {
    #[serde(rename = "Blobs")]
    pub blobs: Option<BlobList>,
    #[serde(rename = "NextMarker")]
    pub next_marker: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BlobList {
    // Azure interleaves <Blob> and <BlobPrefix> when delimiter is set; capture
    // both via $value so quick-xml accepts arbitrary ordering.
    #[serde(rename = "$value", default)]
    pub entries: Vec<BlobOrPrefix>,
}

#[derive(Debug, Deserialize)]
pub enum BlobOrPrefix {
    Blob(BlobItem),
    BlobPrefix(BlobPrefix),
}

#[derive(Debug, Deserialize)]
pub struct BlobPrefix {
    #[serde(rename = "Name")]
    pub name: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct BlobItem {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Properties")]
    pub properties: Option<BlobProperties>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct BlobProperties {
    #[serde(rename = "Last-Modified")]
    pub last_modified: Option<String>,
    #[serde(rename = "Content-Length")]
    pub content_length: Option<u64>,
    #[serde(rename = "Content-Type")]
    pub content_type: Option<String>,
    #[serde(rename = "Content-MD5")]
    pub content_md5: Option<String>,
    #[serde(rename = "Etag")]
    pub etag: Option<String>,
    #[serde(rename = "BlobType")]
    pub blob_type: Option<String>,
    #[serde(rename = "AccessTier")]
    pub access_tier: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename = "Error")]
pub struct StorageError {
    #[serde(rename = "Code")]
    pub code: String,
    #[serde(rename = "Message")]
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct BlobInfo {
    pub name: String,
    pub content_length: u64,
    pub content_type: String,
    pub last_modified: Option<String>,
    pub etag: Option<String>,
    pub content_md5: Option<String>,
}

pub struct BlockListEntry {
    pub id: String,
}

#[derive(Default, Clone)]
pub struct UploadOptions {
    pub overwrite: bool,
    pub content_md5: Option<[u8; 16]>,
}

impl UploadOptions {
    pub fn overwrite() -> Self {
        Self {
            overwrite: true,
            content_md5: None,
        }
    }
}

pub fn build_block_list_xml(blocks: &[BlockListEntry]) -> String {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<BlockList>\n");
    for block in blocks {
        xml.push_str(&format!("  <Latest>{}</Latest>\n", block.id));
    }
    xml.push_str("</BlockList>");
    xml
}
