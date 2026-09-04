use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::{Deserialize, Serialize};

use crate::domain::Profile;
use crate::openai_http::{self, ResponseReadError};

const PROBE_INPUT: &str = "ping";
const PROBE_MAX_OUTPUT_TOKENS: u16 = 16;
const PROBE_MAX_RESPONSE_BYTES: u64 = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeErrorCategory {
    MissingApiKey,
    InvalidBaseUrl,
    Unauthorized,
    RateLimited,
    UpstreamError,
    RequestTimeout,
    NetworkError,
    ResponseTooLarge,
    InvalidResponse,
    RequestRejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeOutcome {
    pub request_duration_ms: u64,
    pub checked_at_unix_ms: u64,
    pub error: Option<ProbeErrorCategory>,
    pub usage: Option<ProbeUsage>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

#[derive(Serialize)]
struct ProbeRequest<'a> {
    model: &'a str,
    input: &'static str,
    max_output_tokens: u16,
    store: bool,
    stream: bool,
}

#[derive(Deserialize)]
struct ResponseEnvelope {
    id: String,
    object: String,
    #[serde(default)]
    status: Option<ResponseStatus>,
    #[serde(default)]
    usage: Option<serde_json::Value>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ResponseStatus {
    Completed,
    Incomplete,
    Failed,
    Queued,
    InProgress,
    Cancelled,
    #[serde(other)]
    Unknown,
}

pub fn probe(profile: &Profile) -> ProbeOutcome {
    probe_with_timeout(profile, Duration::from_secs(30))
}

fn probe_with_timeout(profile: &Profile, timeout: Duration) -> ProbeOutcome {
    let started = Instant::now();
    let Some(api_key) = profile.api_key.as_ref() else {
        return finish(started, Some(ProbeErrorCategory::MissingApiKey), None);
    };
    let endpoint = match openai_http::endpoint(&profile.base_url, "responses") {
        Ok(endpoint) => endpoint,
        Err(_) => return finish(started, Some(ProbeErrorCategory::InvalidBaseUrl), None),
    };
    let client = match openai_http::client(timeout) {
        Ok(client) => client,
        Err(_) => return finish(started, Some(ProbeErrorCategory::NetworkError), None),
    };
    let request = ProbeRequest {
        model: &profile.model,
        input: PROBE_INPUT,
        max_output_tokens: PROBE_MAX_OUTPUT_TOKENS,
        store: false,
        stream: false,
    };
    let request_body = match serde_json::to_vec(&request) {
        Ok(body) => body,
        Err(_) => return finish(started, Some(ProbeErrorCategory::RequestRejected), None),
    };
    let response = match client
        .post(endpoint)
        .bearer_auth(api_key.expose_secret())
        .header(ACCEPT, "application/json")
        .header(CONTENT_TYPE, "application/json")
        .body(request_body)
        .send()
    {
        Ok(response) => response,
        Err(error) if error.is_timeout() => {
            return finish(started, Some(ProbeErrorCategory::RequestTimeout), None);
        }
        Err(_) => return finish(started, Some(ProbeErrorCategory::NetworkError), None),
    };

    let status = response.status().as_u16();
    let (error, usage) = match status {
        200..=299 => match openai_http::read_limited(response, PROBE_MAX_RESPONSE_BYTES) {
            Ok(bytes) => match parse_response_envelope(&bytes) {
                Ok(usage) => (None, usage),
                Err(error) => (Some(error), None),
            },
            Err(ResponseReadError::TooLarge) => (Some(ProbeErrorCategory::ResponseTooLarge), None),
            Err(ResponseReadError::Io) => (Some(ProbeErrorCategory::NetworkError), None),
        },
        401 | 403 => (Some(ProbeErrorCategory::Unauthorized), None),
        408 => (Some(ProbeErrorCategory::RequestTimeout), None),
        429 => (Some(ProbeErrorCategory::RateLimited), None),
        500..=599 => (Some(ProbeErrorCategory::UpstreamError), None),
        _ => (Some(ProbeErrorCategory::RequestRejected), None),
    };
    finish(started, error, usage)
}

fn parse_response_envelope(bytes: &[u8]) -> Result<Option<ProbeUsage>, ProbeErrorCategory> {
    let envelope: ResponseEnvelope =
        serde_json::from_slice(bytes).map_err(|_| ProbeErrorCategory::InvalidResponse)?;
    if envelope.object != "response" || envelope.id.trim().is_empty() {
        return Err(ProbeErrorCategory::InvalidResponse);
    }
    match envelope.status {
        Some(ResponseStatus::Completed | ResponseStatus::Incomplete) => {
            Ok(probe_usage(envelope.usage.as_ref()))
        }
        Some(
            ResponseStatus::Failed
            | ResponseStatus::Queued
            | ResponseStatus::InProgress
            | ResponseStatus::Cancelled,
        ) => Err(ProbeErrorCategory::RequestRejected),
        Some(ResponseStatus::Unknown) | None => Err(ProbeErrorCategory::InvalidResponse),
    }
}

fn probe_usage(value: Option<&serde_json::Value>) -> Option<ProbeUsage> {
    let object = value?.as_object()?;
    let usage = ProbeUsage {
        input_tokens: object
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64),
        output_tokens: object
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64),
        total_tokens: object
            .get("total_tokens")
            .and_then(serde_json::Value::as_u64),
    };
    (usage.input_tokens.is_some() || usage.output_tokens.is_some() || usage.total_tokens.is_some())
        .then_some(usage)
}

fn finish(
    started: Instant,
    error: Option<ProbeErrorCategory>,
    usage: Option<ProbeUsage>,
) -> ProbeOutcome {
    ProbeOutcome {
        request_duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        checked_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX),
        error,
        usage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ApiKey;

    fn profile(address: impl std::fmt::Display) -> Profile {
        Profile::new(
            "Local",
            format!("http://{address}/v1/"),
            ApiKey::new("sk-probe-secret").unwrap(),
            "gpt-probe-model",
            None,
        )
        .unwrap()
    }

    #[test]
    fn sends_a_minimal_non_streaming_responses_request() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let handle = std::thread::spawn(move || {
            let mut request = server.recv().unwrap();
            assert_eq!(request.method(), &tiny_http::Method::Post);
            assert_eq!(request.url(), "/v1/responses");
            let authorization = request
                .headers()
                .iter()
                .find(|header| header.field.equiv("Authorization"))
                .map(|header| header.value.as_str());
            let accept = request
                .headers()
                .iter()
                .find(|header| header.field.equiv("Accept"))
                .map(|header| header.value.as_str());
            let content_type = request
                .headers()
                .iter()
                .find(|header| header.field.equiv("Content-Type"))
                .map(|header| header.value.as_str());
            assert_eq!(authorization, Some("Bearer sk-probe-secret"));
            assert_eq!(accept, Some("application/json"));
            assert_eq!(content_type, Some("application/json"));

            let mut body = String::new();
            request.as_reader().read_to_string(&mut body).unwrap();
            assert!(!body.contains("sk-probe-secret"));
            let body: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["model"], "gpt-probe-model");
            assert_eq!(body["input"], "ping");
            assert_eq!(body["max_output_tokens"], 16);
            assert_eq!(body["store"], false);
            assert_eq!(body["stream"], false);
            request
                .respond(
                    tiny_http::Response::from_string(
                        r#"{"id":"resp_probe","object":"response","status":"incomplete","output":[{"secret":"generated-output-sentinel"}],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}"#,
                    )
                    .with_header(
                        tiny_http::Header::from_bytes(b"Content-Type", b"application/json")
                            .unwrap(),
                    ),
                )
                .unwrap();
        });

        let outcome = probe(&profile(address));

        handle.join().unwrap();
        assert_eq!(outcome.error, None);
        assert_eq!(
            outcome.usage,
            Some(ProbeUsage {
                input_tokens: Some(4),
                output_tokens: Some(1),
                total_tokens: Some(5),
            })
        );
        assert!(!format!("{outcome:?}").contains("generated-output-sentinel"));
        assert!(outcome.checked_at_unix_ms > 0);
    }

    #[test]
    fn accepts_only_completed_or_incomplete_response_statuses() {
        for status in ["completed", "incomplete"] {
            let response = format!(
                r#"{{"id":"resp_ok","object":"response","status":"{status}","output":[{{"secret":"ignored"}}]}}"#
            );

            assert_eq!(parse_response_envelope(response.as_bytes()), Ok(None));
        }

        for status in ["failed", "queued", "in_progress", "cancelled"] {
            let response =
                format!(r#"{{"id":"resp_pending","object":"response","status":"{status}"}}"#);

            assert_eq!(
                parse_response_envelope(response.as_bytes()),
                Err(ProbeErrorCategory::RequestRejected)
            );
        }

        for response in [
            r#"{"id":"resp_missing","object":"response"}"#,
            r#"{"id":"resp_unknown","object":"response","status":"future_status"}"#,
        ] {
            assert_eq!(
                parse_response_envelope(response.as_bytes()),
                Err(ProbeErrorCategory::InvalidResponse)
            );
        }
    }

    #[test]
    fn categorizes_safe_http_failures() {
        for (status, expected) in [
            (401, ProbeErrorCategory::Unauthorized),
            (408, ProbeErrorCategory::RequestTimeout),
            (429, ProbeErrorCategory::RateLimited),
            (503, ProbeErrorCategory::UpstreamError),
            (400, ProbeErrorCategory::RequestRejected),
        ] {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let address = server.server_addr().to_ip().unwrap();
            let handle = std::thread::spawn(move || {
                let request = server.recv().unwrap();
                request
                    .respond(
                        tiny_http::Response::from_string("secret upstream body")
                            .with_status_code(status),
                    )
                    .unwrap();
            });

            let outcome = probe(&profile(address));

            handle.join().unwrap();
            assert_eq!(outcome.error, Some(expected));
            assert!(!format!("{outcome:?}").contains("secret upstream body"));
        }
    }

    #[test]
    fn rejects_invalid_and_oversized_success_responses() {
        let invalid_server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let invalid_address = invalid_server.server_addr().to_ip().unwrap();
        let invalid_handle = std::thread::spawn(move || {
            let request = invalid_server.recv().unwrap();
            request
                .respond(tiny_http::Response::from_string(
                    r#"{"id":"","object":"not-a-response","secret":"body-sentinel"}"#,
                ))
                .unwrap();
        });
        let invalid = probe(&profile(invalid_address));
        invalid_handle.join().unwrap();
        assert_eq!(invalid.error, Some(ProbeErrorCategory::InvalidResponse));
        assert!(!format!("{invalid:?}").contains("body-sentinel"));

        let large_server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let large_address = large_server.server_addr().to_ip().unwrap();
        let large_handle = std::thread::spawn(move || {
            let request = large_server.recv().unwrap();
            request
                .respond(tiny_http::Response::from_data(vec![
                    b'x';
                    PROBE_MAX_RESPONSE_BYTES
                        as usize
                        + 1
                ]))
                .unwrap();
        });
        let large = probe(&profile(large_address));
        large_handle.join().unwrap();
        assert_eq!(large.error, Some(ProbeErrorCategory::ResponseTooLarge));
    }

    #[test]
    fn does_not_follow_redirects() {
        let destination = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let destination_address = destination.server_addr().to_ip().unwrap();
        let source = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let source_address = source.server_addr().to_ip().unwrap();
        let handle = std::thread::spawn(move || {
            let request = source.recv().unwrap();
            request
                .respond(
                    tiny_http::Response::empty(302).with_header(
                        tiny_http::Header::from_bytes(
                            b"Location",
                            format!("http://{destination_address}/captured").as_bytes(),
                        )
                        .unwrap(),
                    ),
                )
                .unwrap();
        });

        let outcome = probe(&profile(source_address));

        handle.join().unwrap();
        assert_eq!(outcome.error, Some(ProbeErrorCategory::RequestRejected));
        assert!(
            destination
                .recv_timeout(Duration::from_millis(150))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn maps_a_real_request_timeout_without_waiting_for_the_production_limit() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let handle = std::thread::spawn(move || {
            let request = server.recv().unwrap();
            std::thread::sleep(Duration::from_millis(200));
            let _ = request.respond(tiny_http::Response::from_string(
                r#"{"id":"too-late","object":"response"}"#,
            ));
        });

        let outcome = probe_with_timeout(&profile(address), Duration::from_millis(40));

        handle.join().unwrap();
        assert_eq!(outcome.error, Some(ProbeErrorCategory::RequestTimeout));
        assert!(outcome.request_duration_ms < 200);
    }

    #[test]
    fn local_validation_failures_do_not_expose_credentials_or_urls() {
        let missing_key = Profile::without_api_key(
            "No key",
            "https://relay.example/v1",
            "gpt-probe-model",
            None,
        )
        .unwrap();
        let missing = probe(&missing_key);
        assert_eq!(missing.error, Some(ProbeErrorCategory::MissingApiKey));

        let mut invalid_url = profile("127.0.0.1:1");
        invalid_url.base_url = "https://user:secret-url@example.test/v1".to_owned();
        let invalid = probe(&invalid_url);
        assert_eq!(invalid.error, Some(ProbeErrorCategory::InvalidBaseUrl));
        let debug = format!("{invalid:?}");
        assert!(!debug.contains("sk-probe-secret"));
        assert!(!debug.contains("secret-url"));
    }
}
