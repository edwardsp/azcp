use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use bytes::Bytes;
use chrono::Utc;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_LENGTH, CONTENT_TYPE};
use reqwest::{Client, Method, Response, StatusCode};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use url::Url;

use crate::auth::{sas, shared_key, Credential};
use crate::config::API_VERSION;
use crate::error::{AzcpError, Result};

use super::models::*;

#[derive(Default, Debug)]
pub struct RetryStats {
    pub throttle_503: AtomicU64,
    pub throttle_429: AtomicU64,
    pub server_5xx: AtomicU64,
    pub transport_err: AtomicU64,
}

impl RetryStats {
    pub fn total(&self) -> u64 {
        self.throttle_503.load(Ordering::Relaxed)
            + self.throttle_429.load(Ordering::Relaxed)
            + self.server_5xx.load(Ordering::Relaxed)
            + self.transport_err.load(Ordering::Relaxed)
    }
}

// Power-of-2 ms histogram buckets; index i covers (2^(i-1), 2^i] ms,
// bucket 0 covers [0, 1] ms, bucket 15 covers (16384 ms, +inf).
#[derive(Debug)]
pub struct LatencyStats {
    pub count: AtomicU64,
    pub sum_us: AtomicU64,
    pub min_us: AtomicU64,
    pub max_us: AtomicU64,
    pub bytes: AtomicU64,
    pub buckets: [AtomicU64; 16],
}

impl Default for LatencyStats {
    fn default() -> Self {
        Self {
            count: AtomicU64::new(0),
            sum_us: AtomicU64::new(0),
            min_us: AtomicU64::new(u64::MAX),
            max_us: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl LatencyStats {
    pub fn record(&self, dur_us: u64, bytes: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(dur_us, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        let dur_ms = dur_us / 1000;
        let bucket = if dur_ms == 0 {
            0usize
        } else {
            ((64 - dur_ms.leading_zeros()) as usize).min(15)
        };
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
        let mut cur = self.min_us.load(Ordering::Relaxed);
        while dur_us < cur {
            match self.min_us.compare_exchange_weak(
                cur,
                dur_us,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(c) => cur = c,
            }
        }
        let mut cur = self.max_us.load(Ordering::Relaxed);
        while dur_us > cur {
            match self.max_us.compare_exchange_weak(
                cur,
                dur_us,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(c) => cur = c,
            }
        }
    }

    pub fn percentile_ms(&self, p: f64) -> u64 {
        let counts: [u64; 16] = std::array::from_fn(|i| self.buckets[i].load(Ordering::Relaxed));
        let total: u64 = counts.iter().sum();
        if total == 0 {
            return 0;
        }
        let target = ((total as f64) * p).ceil() as u64;
        let mut acc = 0u64;
        for (i, c) in counts.iter().enumerate() {
            acc += c;
            if acc >= target {
                return if i == 0 { 1 } else { 1u64 << i };
            }
        }
        u64::MAX
    }
}

#[derive(Clone)]
pub struct BlobClient {
    http: Client,
    credential: Credential,
    max_retries: u32,
    retry_stats: Arc<RetryStats>,
    latency_stats: Arc<LatencyStats>,
}

impl BlobClient {
    pub fn new(credential: Credential) -> Result<Self> {
        Self::with_max_retries(credential, 5)
    }

    pub fn with_max_retries(credential: Credential, max_retries: u32) -> Result<Self> {
        Self::build(credential, max_retries, None, None)
    }

    pub fn with_shared_stats(
        credential: Credential,
        max_retries: u32,
        retry_stats: Arc<RetryStats>,
        latency_stats: Arc<LatencyStats>,
    ) -> Result<Self> {
        Self::build(credential, max_retries, Some(retry_stats), Some(latency_stats))
    }

    fn build(
        credential: Credential,
        max_retries: u32,
        retry_stats: Option<Arc<RetryStats>>,
        latency_stats: Option<Arc<LatencyStats>>,
    ) -> Result<Self> {
        // HTTP/2 multiplexes all requests onto a single TCP connection, which
        // caps per-connection receive bandwidth on Azure Blob (~25 Gbps
        // observed). HTTP/1.1 lets the pool spread traffic across many
        // independent TCP connections.
        let http = Client::builder()
            .pool_max_idle_per_host(512)
            .http1_only()
            .build()
            .map_err(AzcpError::Http)?;
        Ok(Self {
            http,
            credential,
            max_retries,
            retry_stats: retry_stats.unwrap_or_else(|| Arc::new(RetryStats::default())),
            latency_stats: latency_stats.unwrap_or_else(|| Arc::new(LatencyStats::default())),
        })
    }

    pub fn retry_stats(&self) -> Arc<RetryStats> {
        self.retry_stats.clone()
    }

    pub fn latency_stats(&self) -> Arc<LatencyStats> {
        self.latency_stats.clone()
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
            let url =
                Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;

            let resp = self.send(Method::GET, url, &[], None, None).await?;
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
            let url =
                Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;

            let resp = self.send(Method::GET, url, &[], None, None).await?;
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
            let url =
                Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;

            let resp = self.send(Method::GET, url, &[], None, None).await?;
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
        let url =
            Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;

        let resp = self
            .send(Method::PUT, url, &[], None, Some(Bytes::new()))
            .await?;
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
        let url =
            Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;

        let resp = self.send(Method::DELETE, url, &[], None, None).await?;
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
        let url =
            Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;

        let ct = content_type.unwrap_or("application/octet-stream");
        let extra = build_upload_headers(options);

        let resp = self
            .send(Method::PUT, url, &extra, Some(ct), Some(data))
            .await?;
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
        let url =
            Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;

        let resp = self
            .send(Method::PUT, url, &[], None, Some(data))
            .await?;
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
        let url =
            Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;

        let xml = build_block_list_xml(blocks);
        let body = Bytes::from(xml);
        let mut extra: Vec<(&'static str, HeaderValue)> = build_upload_headers(options)
            .into_iter()
            .filter_map(|(name, value)| match name {
                "x-ms-blob-type" => None,
                // On commit the request body is the BlockList XML, not the blob
                // content. Send the blob's MD5 via x-ms-blob-content-md5 instead,
                // otherwise Azure validates content-md5 against the XML body.
                "content-md5" => Some(("x-ms-blob-content-md5", value)),
                other => Some((other, value)),
            })
            .collect();
        if let Some(ct) = content_type {
            if let Ok(val) = HeaderValue::from_str(ct) {
                extra.push(("x-ms-blob-content-type", val));
            }
        }

        let resp = self
            .send(Method::PUT, url, &extra, Some("application/xml"), Some(body))
            .await?;
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
        let url =
            Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;

        let started = std::time::Instant::now();
        let resp = self.send(Method::GET, url, &[], None, None).await?;
        let status = resp.status();

        if !status.is_success() {
            let body = resp.text().await?;
            return Err(parse_error(status, &body));
        }

        let bytes = resp.bytes().await?;
        let elapsed_us = started.elapsed().as_micros() as u64;
        self.latency_stats.record(elapsed_us, bytes.len() as u64);
        Ok(bytes)
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
        let url =
            Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;
        let range_val = format!("bytes={}-{}", offset, offset + length - 1);
        let extra = vec![(
            "x-ms-range",
            HeaderValue::from_str(&range_val).unwrap(),
        )];

        let started = std::time::Instant::now();
        let resp = self.send(Method::GET, url, &extra, None, None).await?;
        let status = resp.status();

        if !status.is_success() {
            let body = resp.text().await?;
            return Err(parse_error(status, &body));
        }

        let bytes = resp.bytes().await?;
        let elapsed_us = started.elapsed().as_micros() as u64;
        self.latency_stats.record(elapsed_us, bytes.len() as u64);
        Ok(bytes)
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
        let url =
            Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;

        let resp = self.send(Method::HEAD, url, &[], None, None).await?;
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
        let url =
            Url::parse(&url_str).map_err(|e| AzcpError::InvalidUrl(e.to_string()))?;
        let extra = [("x-ms-delete-snapshots", HeaderValue::from_static("include"))];

        let resp = self.send(Method::DELETE, url, &extra, None, None).await?;
        let status = resp.status();

        if status.is_success() || status == StatusCode::ACCEPTED {
            Ok(())
        } else {
            let body = resp.text().await?;
            Err(parse_error(status, &body))
        }
    }

    // send issues an HTTP request with retry/backoff on 429/503/5xx. Headers
    // (including x-ms-date and SharedKey signature) are re-computed per attempt
    // so signatures don't expire on long retries. The body must be Bytes
    // (clonable) so retries can resend it.
    async fn send(
        &self,
        method: Method,
        url: Url,
        extra_headers: &[(&'static str, HeaderValue)],
        content_type: Option<&str>,
        body: Option<Bytes>,
    ) -> Result<Response> {
        let max_attempts: u32 = self.max_retries.saturating_add(1).max(1);
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let content_length = body.as_ref().map(|b| b.len() as u64);
            let headers = self.build_headers_with(
                &url,
                method.as_str(),
                content_type,
                content_length,
                extra_headers,
            )?;
            let mut req_url = url.clone();
            self.apply_sas(&mut req_url);

            let mut req = self.http.request(method.clone(), req_url).headers(headers);
            if let Some(b) = body.clone() {
                req = req.body(b);
            }

            let send_result = req.send().await;
            match send_result {
                Ok(resp) => {
                    let status = resp.status();
                    if is_retryable_status(status) && attempt < max_attempts {
                        self.record_retry_status(status);
                        let delay = retry_delay_ms(&resp, attempt);
                        tracing::warn!(
                            "retry {attempt}/{max_attempts} status={} sleep={}ms",
                            status.as_u16(),
                            delay
                        );
                        drop(resp);
                        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                        continue;
                    }
                    return Ok(resp);
                }
                Err(e) if attempt < max_attempts && is_retryable_transport_err(&e) => {
                    self.retry_stats.transport_err.fetch_add(1, Ordering::Relaxed);
                    let delay = backoff_ms(attempt);
                    tracing::warn!(
                        "retry {attempt}/{max_attempts} transport error={e} sleep={delay}ms"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    continue;
                }
                Err(e) => return Err(AzcpError::Http(e)),
            }
        }
    }

    fn record_retry_status(&self, status: StatusCode) {
        match status {
            StatusCode::SERVICE_UNAVAILABLE => {
                self.retry_stats.throttle_503.fetch_add(1, Ordering::Relaxed);
            }
            StatusCode::TOO_MANY_REQUESTS => {
                self.retry_stats.throttle_429.fetch_add(1, Ordering::Relaxed);
            }
            _ => {
                self.retry_stats.server_5xx.fetch_add(1, Ordering::Relaxed);
            }
        }
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

        // Insert extra x-ms-* headers BEFORE signing so they are included in
        // the canonicalized headers used to compute the SharedKey signature.
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
    // 409 Conflict (BlobAlreadyExists) — blob already exists on create.
    // 412 Precondition Failed — If-None-Match: * set and blob exists.
    status == StatusCode::CONFLICT || status == StatusCode::PRECONDITION_FAILED
}

fn is_retryable_status(status: StatusCode) -> bool {
    // Retry Azure throttling (503 ServerBusy, 429 TooManyRequests) and transient
    // upstream failures (500/502/504). Do NOT retry 4xx auth/config errors.
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

fn is_retryable_transport_err(e: &reqwest::Error) -> bool {
    // Network-level failures worth retrying: connect/timeout/body-streaming.
    // Build errors (bad URL, invalid headers) are not retryable.
    e.is_timeout() || e.is_connect() || e.is_request() || e.is_body()
}

fn retry_delay_ms(resp: &Response, attempt: u32) -> u64 {
    // Honor server-provided Retry-After header (seconds, per RFC 7231) when
    // present; otherwise fall back to exponential backoff with jitter.
    if let Some(secs) = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        return (secs.saturating_mul(1000)).min(60_000);
    }
    backoff_ms(attempt)
}

fn backoff_ms(attempt: u32) -> u64 {
    // Exponential backoff 500ms * 2^(attempt-1) capped at 30s, with ±25% jitter
    // sourced from system-time nanos (good enough to de-synchronize parallel
    // retries; no crypto need here).
    let base = 500u64;
    let cap = 30_000u64;
    let exp = base.saturating_mul(1u64 << (attempt - 1).min(10));
    let capped = exp.min(cap);
    let jitter_range = capped / 4;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let jitter = nanos % (jitter_range * 2 + 1);
    capped.saturating_sub(jitter_range).saturating_add(jitter)
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
