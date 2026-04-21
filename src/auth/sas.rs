use url::Url;

pub fn append_sas_token(url: &mut Url, token: &str) {
    let token = token.strip_prefix('?').unwrap_or(token);
    let existing = url.query().unwrap_or("");
    if existing.is_empty() {
        url.set_query(Some(token));
    } else {
        url.set_query(Some(&format!("{existing}&{token}")));
    }
}

pub fn extract_sas_token(url: &Url) -> Option<String> {
    let query = url.query()?;
    if query.contains("sig=") {
        Some(query.to_string())
    } else {
        None
    }
}
