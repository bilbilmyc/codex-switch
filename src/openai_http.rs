use std::io::Read;
use std::time::Duration;

use reqwest::blocking::{Client, Response};
use reqwest::redirect::Policy;
use url::Url;

pub(crate) const MAX_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct InvalidBaseUrl;

#[derive(Debug)]
pub(crate) enum ResponseReadError {
    Io,
    TooLarge,
}

pub(crate) fn client(timeout: Duration) -> Result<Client, reqwest::Error> {
    Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(timeout)
        .redirect(Policy::none())
        .build()
}

pub(crate) fn endpoint(base_url: &str, resource: &str) -> Result<Url, InvalidBaseUrl> {
    let mut url = Url::parse(base_url).map_err(|_| InvalidBaseUrl)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none()
    {
        return Err(InvalidBaseUrl);
    }

    {
        let mut segments = url.path_segments_mut().map_err(|_| InvalidBaseUrl)?;
        segments.pop_if_empty();
        segments.push(resource);
    }
    Ok(url)
}

pub(crate) fn read_limited(
    response: Response,
    max_bytes: u64,
) -> Result<Vec<u8>, ResponseReadError> {
    let mut limited = response.take(max_bytes + 1);
    let mut bytes = Vec::new();
    limited
        .read_to_end(&mut bytes)
        .map_err(|_| ResponseReadError::Io)?;
    if bytes.len() as u64 > max_bytes {
        return Err(ResponseReadError::TooLarge);
    }
    Ok(bytes)
}
