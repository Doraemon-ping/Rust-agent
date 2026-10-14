//! The bridge's JSON Lines wire format, seen from the host.
//!
//! Only the host -> runtime direction is constructed here; the runtime ->
//! host direction is parsed. Both are strict: an unexpected shape is an error
//! the caller turns into a log line or a failed request, never a panic.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state::{HostError, PROTOCOL_VERSION};

/// An error object as the bridge reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteError {
    pub code: String,
    pub message: String,
}

/// One message read from the runtime's stdout.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Inbound {
    /// The answer to a request, matched by `request_id`.
    Response {
        #[serde(default)]
        protocol_version: Option<u32>,
        request_id: String,
        ok: bool,
        #[serde(default)]
        result: Option<Value>,
        #[serde(default)]
        error: Option<RemoteError>,
    },
    /// An asynchronous notification. Never matches a request.
    Event {
        #[serde(default)]
        protocol_version: Option<u32>,
        event: String,
        #[serde(default)]
        ts: Option<String>,
        #[serde(default)]
        data: Value,
    },
    /// A failure the runtime could not attribute to a request.
    Error {
        #[serde(default)]
        protocol_version: Option<u32>,
        #[serde(default)]
        ts: Option<String>,
        error: RemoteError,
    },
}

impl Inbound {
    /// The protocol revision the runtime reported, when it reported one.
    pub fn protocol_version(&self) -> Option<u32> {
        match self {
            Inbound::Response {
                protocol_version, ..
            }
            | Inbound::Event {
                protocol_version, ..
            }
            | Inbound::Error {
                protocol_version, ..
            } => *protocol_version,
        }
    }


}

/// Parse one stdout line.
pub fn parse_inbound(line: &str) -> Result<Inbound, HostError> {
    serde_json::from_str::<Inbound>(line).map_err(|error| {
        HostError::new(
            "invalid_runtime_message",
            format!("the runtime sent a message we do not understand: {error}"),
        )
    })
}


#[derive(Debug, Serialize)]
struct RequestMessage<'a> {
    protocol_version: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    request_id: &'a str,
    method: &'a str,
    params: Value,
}

/// Encode one request as a single line, without the trailing newline.
pub fn encode_request(request_id: &str, method: &str, params: Value) -> Result<String, HostError> {
    serde_json::to_string(&RequestMessage {
        protocol_version: PROTOCOL_VERSION,
        kind: "request",
        request_id,
        method,
        params,
    })
    .map_err(|error| {
        HostError::new(
            "encode_failed",
            format!("could not encode `{method}`: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_request_line_has_the_documented_shape() {
        let line = encode_request("r-1", "runtime.ping", json!({})).unwrap();
        assert!(!line.contains('\n'));
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["protocol_version"], json!(PROTOCOL_VERSION));
        assert_eq!(value["type"], json!("request"));
        assert_eq!(value["request_id"], json!("r-1"));
        assert_eq!(value["method"], json!("runtime.ping"));
        assert_eq!(value["params"], json!({}));
    }

    #[test]
    fn a_response_is_matched_by_request_id() {
        let line = r#"{"protocol_version":1,"type":"response","request_id":"r-4","ok":true,"result":{"pong":true}}"#;
        match parse_inbound(line) {
            Ok(Inbound::Response {
                request_id,
                ok,
                result,
                ..
            }) => {
                assert_eq!(request_id, "r-4");
                assert!(ok);
                assert_eq!(result.unwrap()["pong"], json!(true));
            }
            other => panic!("expected a response, got {other:?}"),
        }
    }

    #[test]
    fn an_event_never_looks_like_a_response() {
        let line = r#"{"protocol_version":1,"type":"event","event":"runtime.ready","ts":"2026-01-01T00:00:00Z","data":{"pid":7}}"#;
        let parsed = parse_inbound(line).unwrap();
        assert!(matches!(parsed, Inbound::Event { .. }));
        assert!(!matches!(parsed, Inbound::Response { .. }));
        assert_eq!(parsed.protocol_version(), Some(PROTOCOL_VERSION));
    }

    #[test]
    fn an_unsolicited_error_parses() {
        let line = r#"{"protocol_version":1,"type":"error","error":{"code":"invalid_json","message":"nope"}}"#;
        match parse_inbound(line) {
            Ok(Inbound::Error { error, .. }) => assert_eq!(error.code, "invalid_json"),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn a_failed_response_carries_its_error() {
        let line = r#"{"protocol_version":1,"type":"response","request_id":"r-9","ok":false,"error":{"code":"unknown_method","message":"no"}}"#;
        match parse_inbound(line) {
            Ok(Inbound::Response { ok, error, .. }) => {
                assert!(!ok);
                assert_eq!(error.unwrap().code, "unknown_method");
            }
            other => panic!("expected a response, got {other:?}"),
        }
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        for line in ["{not json", "[1,2]", "", r#"{"type":"shutdown"}"#] {
            assert!(parse_inbound(line).is_err(), "accepted {line:?}");
        }
    }
}
