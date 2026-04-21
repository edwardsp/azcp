use base64::{engine::general_purpose::STANDARD, Engine as _};
use hmac::{Hmac, Mac};
use reqwest::header::HeaderMap;
use sha2::Sha256;
use url::Url;

type HmacSha256 = Hmac<Sha256>;

pub fn sign_request(
    account: &str,
    key: &str,
    method: &str,
    url: &Url,
    headers: &HeaderMap,
    content_length: Option<u64>,
) -> String {
    let canonicalized_headers = build_canonicalized_headers(headers);
    let canonicalized_resource = build_canonicalized_resource(account, url);

    let content_length_str = match content_length {
        Some(0) | None => String::new(),
        Some(len) => len.to_string(),
    };

    let string_to_sign = format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        method,
        get_header(headers, "content-encoding"),
        get_header(headers, "content-language"),
        content_length_str,
        get_header(headers, "content-md5"),
        get_header(headers, "content-type"),
        get_header(headers, "date"),
        get_header(headers, "if-modified-since"),
        get_header(headers, "if-match"),
        get_header(headers, "if-none-match"),
        get_header(headers, "if-unmodified-since"),
        get_header(headers, "range"),
        canonicalized_headers,
        canonicalized_resource,
    );

    let decoded_key = STANDARD.decode(key).expect("invalid base64 storage key");
    let mut mac = HmacSha256::new_from_slice(&decoded_key).expect("HMAC init failed");
    mac.update(string_to_sign.as_bytes());
    let signature = STANDARD.encode(mac.finalize().into_bytes());

    format!("SharedKey {account}:{signature}")
}

fn get_header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

fn build_canonicalized_headers(headers: &HeaderMap) -> String {
    let mut ms_headers: Vec<(String, String)> = headers
        .iter()
        .filter_map(|(k, v)| {
            let name = k.as_str().to_lowercase();
            if name.starts_with("x-ms-") {
                Some((name, v.to_str().unwrap_or("").to_string()))
            } else {
                None
            }
        })
        .collect();

    ms_headers.sort_by(|a, b| a.0.cmp(&b.0));

    ms_headers
        .iter()
        .map(|(k, v)| format!("{k}:{v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_canonicalized_resource(account: &str, url: &Url) -> String {
    let path = url.path();
    let mut resource = format!("/{account}{path}");

    let mut params: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.to_lowercase(), v.to_string()))
        .collect();
    params.sort_by(|a, b| a.0.cmp(&b.0));

    for (key, value) in &params {
        resource.push_str(&format!("\n{key}:{value}"));
    }

    resource
}
