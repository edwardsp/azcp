use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use bytes::Bytes;
use chrono::Utc;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_LENGTH, CONTENT_TYPE};
use reqwest::{Client, StatusCode};
use url::Url;

use crate::auth::{sas, shared_key, Credential};
use crate::config::API_VERSION;
use crate::error::{AzcpError, Result};

use super::models::*;

#[derive(Clone)]
pub struct BlobClient {
    http: Client,
    credential: Credential,
}

impl BlobClient {
    pub fn new(credential: Credential) -> Result<Self> {
        let http = Client::builder()
            .pool_max_idle_per_host(100)
            .build()
            .map_err(AzcpError::Http)?;
        Ok(Self { http, credential })
    }

    pub async fn list_containers(&self, account: &str) -> Result<Vec<ContainerItem>> {
        let mut all = Vec::new();
        let mut marker: Option<String> = None;

        loop {
            let mut url_str =
                format!("https://{account}.blob.core.windows.net/?comp=list");
            if let Some(ref m) = marker {
                url_str.push_str(&format!("&marker={m}"));
            }

            let mut url =
                Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;
            let headers = self.build_headers(&url, "GET", None, None)?;
            self.apply_sas(&mut url);

            let resp = self.http.get(url).headers(headers).send().await?;
            let status = resp.status();
            let body = resp.text().await?;

            if !status.is_success() {
                return Err(parse_error(status, &body));
            }

            let parsed: ContainerListResponse = quick_xml::de::from_str(&body)
                .map_err(|e| AzcpError::Xml(e.to_string()))?;

            if let Some(containers) = parsed.containers {
                all.extend(containers.items);
            }

            match parsed.next_marker {
                Some(ref m) if !m.is_empty() => marker = Some(m.clone()),
                _ => break,
            }
        }

        Ok(all)
    }

    pub async fn list_blobs(
        &self,
        account: &str,
        container: &str,
        prefix: Option<&str>,
        recursive: bool,
    ) -> Result<Vec<BlobItem>> {
        let mut all = Vec::new();
        let mut marker: Option<String> = None;

        loop {
            let mut url_str = format!(
                "https://{account}.blob.core.windows.net/{container}?restype=container&comp=list"
            );
            if let Some(p) = prefix {
                let encoded = percent_encoding::utf8_percent_encode(
                    p,
                    percent_encoding::NON_ALPHANUMERIC,
                );
                url_str.push_str(&format!("&prefix={encoded}"));
            }
            if !recursive {
                url_str.push_str("&delimiter=/");
            }
            if let Some(ref m) = marker {
                url_str.push_str(&format!("&marker={m}"));
            }

            let mut url =
                Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;
            let headers = self.build_headers(&url, "GET", None, None)?;
            self.apply_sas(&mut url);

            let resp = self.http.get(url).headers(headers).send().await?;
            let status = resp.status();
            let body = resp.text().await?;

            if !status.is_success() {
                return Err(parse_error(status, &body));
            }

            let parsed: BlobListResponse = quick_xml::de::from_str(&body)
                .map_err(|e| AzcpError::Xml(e.to_string()))?;

            if let Some(blobs) = parsed.blobs {
                for entry in blobs.entries {
                    if let crate::storage::blob::models::BlobOrPrefix::Blob(b) = entry {
                        all.push(b);
                    }
                }
            }

            match parsed.next_marker {
                Some(ref m) if !m.is_empty() => marker = Some(m.clone()),
                _ => break,
            }
        }

        Ok(all)
    }

    pub async fn list_entries(
        &self,
        account: &str,
        container: &str,
        prefix: Option<&str>,
        recursive: bool,
    ) -> Result<(Vec<BlobItem>, Vec<String>)> {
        let mut blobs = Vec::new();
        let mut prefixes = Vec::new();
        let mut marker: Option<String> = None;

        loop {
            let mut url_str = format!(
                "https://{account}.blob.core.windows.net/{container}?restype=container&comp=list"
            );
            if let Some(p) = prefix {
                let encoded = percent_encoding::utf8_percent_encode(
                    p,
                    percent_encoding::NON_ALPHANUMERIC,
                );
                url_str.push_str(&format!("&prefix={encoded}"));
            }
            if !recursive {
                url_str.push_str("&delimiter=/");
            }
            if let Some(ref m) = marker {
                url_str.push_str(&format!("&marker={m}"));
            }

            let mut url =
                Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;
            let headers = self.build_headers(&url, "GET", None, None)?;
            self.apply_sas(&mut url);

            let resp = self.http.get(url).headers(headers).send().await?;
            let status = resp.status();
            let body = resp.text().await?;

            if !status.is_success() {
                return Err(parse_error(status, &body));
            }

            let parsed: BlobListResponse = quick_xml::de::from_str(&body)
                .map_err(|e| AzcpError::Xml(e.to_string()))?;

            if let Some(list) = parsed.blobs {
                for entry in list.entries {
                    match entry {
                        crate::storage::blob::models::BlobOrPrefix::Blob(b) => blobs.push(b),
                        crate::storage::blob::models::BlobOrPrefix::BlobPrefix(p) => {
                            prefixes.push(p.name)
                        }
                    }
                }
            }

            match parsed.next_marker {
                Some(ref m) if !m.is_empty() => marker = Some(m.clone()),
                _ => break,
            }
        }

        Ok((blobs, prefixes))
    }

    pub async fn create_container(
        &self,
        account: &str,
        container: &str,
    ) -> Result<()> {
        let url_str = format!(
            "https://{account}.blob.core.windows.net/{container}?restype=container"
        );
        let mut url =
            Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;
        let headers = self.build_headers(&url, "PUT", None, Some(0))?;
        self.apply_sas(&mut url);

        let resp = self.http.put(url).headers(headers).send().await?;
        let status = resp.status();

        if status == StatusCode::CREATED || status == StatusCode::CONFLICT {
            Ok(())
        } else {
            let body = resp.text().await?;
            Err(parse_error(status, &body))
        }
    }

    pub async fn delete_container(
        &self,
        account: &str,
        container: &str,
    ) -> Result<()> {
        let url_str = format!(
            "https://{account}.blob.core.windows.net/{container}?restype=container"
        );
        let mut url =
            Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;
        let headers = self.build_headers(&url, "DELETE", None, None)?;
        self.apply_sas(&mut url);

        let resp = self.http.delete(url).headers(headers).send().await?;
        let status = resp.status();

        if status.is_success() || status == StatusCode::ACCEPTED {
            Ok(())
        } else {
            let body = resp.text().await?;
            Err(parse_error(status, &body))
        }
    }

    pub async fn put_blob(
        &self,
        account: &str,
        container: &str,
        blob_path: &str,
        data: Bytes,
        content_type: Option<&str>,
        options: &UploadOptions,
    ) -> Result<()> {
        let url_str = format!(
            "https://{account}.blob.core.windows.net/{container}/{blob_path}"
        );
        let mut url =
            Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;

        let ct = content_type.unwrap_or("application/octet-stream");
        let len = data.len() as u64;
        let extra = build_upload_headers(options);
        let headers = self.build_headers_with(&url, "PUT", Some(ct), Some(len), &extra)?;
        self.apply_sas(&mut url);

        let resp = self.http.put(url).headers(headers).body(data).send().await?;
        let status = resp.status();

        if status.is_success() {
            Ok(())
        } else if is_already_exists(status) {
            Err(AzcpError::AlreadyExists(blob_path.to_string()))
        } else {
            let body = resp.text().await?;
            Err(parse_error(status, &body))
        }
    }

    pub async fn put_block(
        &self,
        account: &str,
        container: &str,
        blob_path: &str,
        block_id: &str,
        data: Bytes,
    ) -> Result<()> {
        let encoded_param = percent_encoding::utf8_percent_encode(
            block_id,
            percent_encoding::NON_ALPHANUMERIC,
        );
        let url_str = format!(
            "https://{account}.blob.core.windows.net/{container}/{blob_path}?comp=block&blockid={encoded_param}"
        );
        let mut url =
            Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;

        let len = data.len() as u64;
        let headers = self.build_headers(&url, "PUT", None, Some(len))?;
        self.apply_sas(&mut url);

        let resp = self.http.put(url).headers(headers).body(data).send().await?;
        let status = resp.status();

        if status.is_success() {
            Ok(())
        } else {
            let body = resp.text().await?;
            Err(parse_error(status, &body))
        }
    }

    pub async fn put_block_list(
        &self,
        account: &str,
        container: &str,
        blob_path: &str,
        blocks: &[BlockListEntry],
        content_type: Option<&str>,
        options: &UploadOptions,
    ) -> Result<()> {
        let url_str = format!(
            "https://{account}.blob.core.windows.net/{container}/{blob_path}?comp=blocklist"
        );
        let mut url =
            Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;

        let xml = build_block_list_xml(blocks);
        let len = xml.len() as u64;
        let mut extra: Vec<(&'static str, HeaderValue)> = build_upload_headers(options)
            .into_iter()
            .filter_map(|(name, value)| match name {
                "x-ms-blob-type" => None,
                // On commit the request body is the BlockList XML, not the
                // blob content. Send the blob's MD5 via the metadata header
                // instead, otherwise Azure validates it against the XML.
                "content-md5" => Some(("x-ms-blob-content-md5", value)),
                other => Some((other, value)),
            })
            .collect();
        if let Some(ct) = content_type {
            if let Ok(val) = HeaderValue::from_str(ct) {
                extra.push(("x-ms-blob-content-type", val));
            }
        }
        let headers = self.build_headers_with(
            &url,
            "PUT",
            Some("application/xml"),
            Some(len),
            &extra,
        )?;
        self.apply_sas(&mut url);

        let resp = self.http.put(url).headers(headers).body(xml).send().await?;
        let status = resp.status();

        if status.is_success() {
            Ok(())
        } else if is_already_exists(status) {
            Err(AzcpError::AlreadyExists(blob_path.to_string()))
        } else {
            let body = resp.text().await?;
            Err(parse_error(status, &body))
        }
    }

    pub async fn get_blob(
        &self,
        account: &str,
        container: &str,
        blob_path: &str,
    ) -> Result<Bytes> {
        let url_str = format!(
            "https://{account}.blob.core.windows.net/{container}/{blob_path}"
        );
        let mut url =
            Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;
        let headers = self.build_headers(&url, "GET", None, None)?;
        self.apply_sas(&mut url);

        let resp = self.http.get(url).headers(headers).send().await?;
        let status = resp.status();

        if !status.is_success() {
            let body = resp.text().await?;
            return Err(parse_error(status, &body));
        }

        Ok(resp.bytes().await?)
    }

    pub async fn get_blob_range(
        &self,
        account: &str,
        container: &str,
        blob_path: &str,
        offset: u64,
        length: u64,
    ) -> Result<Bytes> {
        let url_str = format!(
            "https://{account}.blob.core.windows.net/{container}/{blob_path}"
        );
        let mut url =
            Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;
        let range_val = format!("bytes={}-{}", offset, offset + length - 1);
        let extra = vec![(
            "x-ms-range",
            HeaderValue::from_str(&range_val).unwrap(),
        )];
        let headers = self.build_headers_with(&url, "GET", None, None, &extra)?;
        self.apply_sas(&mut url);

        let resp = self.http.get(url).headers(headers).send().await?;
        let status = resp.status();

        if !status.is_success() {
            let body = resp.text().await?;
            return Err(parse_error(status, &body));
        }

        Ok(resp.bytes().await?)
    }

    pub async fn get_blob_properties(
        &self,
        account: &str,
        container: &str,
        blob_path: &str,
    ) -> Result<BlobInfo> {
        let url_str = format!(
            "https://{account}.blob.core.windows.net/{container}/{blob_path}"
        );
        let mut url =
            Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;
        let headers = self.build_headers(&url, "HEAD", None, None)?;
        self.apply_sas(&mut url);

        let resp = self.http.head(url).headers(headers).send().await?;
        let status = resp.status();

        if !status.is_success() {
            return Err(AzcpError::Storage {
                status: status.as_u16(),
                message: format!("HEAD {blob_path} failed"),
            });
        }

        let h = resp.headers();
        Ok(BlobInfo {
            name: blob_path.to_string(),
            content_length: h
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            content_type: h
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_string(),
            last_modified: h
                .get("last-modified")
                .and_then(|v| v.to_str().ok())
                .map(String::from),
            etag: h.get("etag").and_then(|v| v.to_str().ok()).map(String::from),
            content_md5: h
                .get("content-md5")
                .and_then(|v| v.to_str().ok())
                .map(String::from),
        })
    }

    pub async fn delete_blob(
        &self,
        account: &str,
        container: &str,
        blob_path: &str,
    ) -> Result<()> {
        let url_str = format!(
            "https://{account}.blob.core.windows.net/{container}/{blob_path}"
        );
        let mut url =
            Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;
        let headers = self.build_headers_with(
            &url,
            "DELETE",
            None,
            None,
            &[("x-ms-delete-snapshots", HeaderValue::from_static("include"))],
        )?;
        self.apply_sas(&mut url);

        let resp = self.http.delete(url).headers(headers).send().await?;
        let status = resp.status();

        if status.is_success() || status == StatusCode::ACCEPTED {
            Ok(())
        } else {
            let body = resp.text().await?;
            Err(parse_error(status, &body))
        }
    }

    fn build_headers(
        &self,
        url: &Url,
        method: &str,
        content_type: Option<&str>,
        content_length: Option<u64>,
    ) -> Result<HeaderMap> {
        self.build_headers_with(url, method, content_type, content_length, &[])
    }

    fn build_headers_with(
        &self,
        url: &Url,
        method: &str,
        content_type: Option<&str>,
        content_length: Option<u64>,
        extra_headers: &[(&'static str, HeaderValue)],
    ) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        let date = Utc::now()
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();

        headers.insert("x-ms-date", HeaderValue::from_str(&date).unwrap());
        headers.insert("x-ms-version", HeaderValue::from_static(API_VERSION));

        if let Some(ct) = content_type {
            if let Ok(val) = HeaderValue::from_str(ct) {
                headers.insert(CONTENT_TYPE, val);
            }
        }
        if let Some(len) = content_length {
            headers.insert(
                CONTENT_LENGTH,
                HeaderValue::from_str(&len.to_string()).unwrap(),
            );
        }

        // Insert extra x-ms-* headers BEFORE signing so they are included
        // in the canonicalized headers used to compute the SharedKey signature.
        for (name, value) in extra_headers {
            headers.insert(*name, value.clone());
        }

        match &self.credential {
            Credential::SharedKey { account, key } => {
                let auth =
                    shared_key::sign_request(account, key, method, url, &headers, content_length);
                headers.insert(
                    "Authorization",
                    HeaderValue::from_str(&auth).unwrap(),
                );
            }
            Credential::Bearer { token } => {
                let val = format!("Bearer {token}");
                headers.insert(
                    "Authorization",
                    HeaderValue::from_str(&val).unwrap(),
                );
            }
            Credential::Sas { .. } | Credential::Anonymous => {}
        }

        Ok(headers)
    }

    fn apply_sas(&self, url: &mut Url) {
        if let Credential::Sas { token } = &self.credential {
            sas::append_sas_token(url, token);
        }
    }
}

fn parse_error(status: StatusCode, body: &str) -> AzcpError {
    if let Ok(err) = quick_xml::de::from_str::<StorageError>(body) {
        AzcpError::Storage {
            status: status.as_u16(),
            message: format!("{}: {}", err.code, err.message),
        }
    } else {
        AzcpError::Storage {
            status: status.as_u16(),
            message: body.chars().take(200).collect(),
        }
    }
}

fn is_already_exists(status: StatusCode) -> bool {
    // 409 Conflict (BlobAlreadyExists) is what Azure returns when the
    // request body would create a blob that already exists. 412 Precondition
    // Failed is returned when If-None-Match: * is set and the blob exists.
    status == StatusCode::CONFLICT || status == StatusCode::PRECONDITION_FAILED
}

fn build_upload_headers(options: &UploadOptions) -> Vec<(&'static str, HeaderValue)> {
    let mut h: Vec<(&'static str, HeaderValue)> = vec![(
        "x-ms-blob-type",
        HeaderValue::from_static("BlockBlob"),
    )];
    if !options.overwrite {
        h.push(("if-none-match", HeaderValue::from_static("*")));
    }
    if let Some(md5) = &options.content_md5 {
        let encoded = B64.encode(md5);
        if let Ok(val) = HeaderValue::from_str(&encoded) {
            h.push(("content-md5", val));
        }
    }
    h
}
