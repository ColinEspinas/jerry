//! JSON-RPC 2.0 messages and the frame that carries them: a big-endian `u32` byte length, then
//! the JSON. Requests carry an `id`; notifications do not; a response answers one `id`.
//! Transport-level failures are JSON-RPC errors; a Command's own outcome is always a `result`.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::io::{self, BufRead, Read, Write};

/// Bumped only for a framing or envelope change. The registry descriptor carries it and
/// `Client::connect_to` refuses a host whose version differs.
pub const PROTOCOL_VERSION: u32 = 1;

/// Hook payloads can embed file contents; anything past this is a bug or an attack, not data.
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

const JSONRPC: &str = "2.0";

/// JSON-RPC error codes. The negative-32000 block is this protocol's own.
pub mod rpc_code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    /// The descriptor's protocol version is not this binary's.
    pub const UNSUPPORTED_VERSION: i64 = -32000;
    pub const SHUTTING_DOWN: i64 = -32001;
    pub const TIMED_OUT: i64 = -32002;
    /// The caller may not ask for this at all (`Invocability`), distinct from a `denied` Report.
    pub const FORBIDDEN: i64 = -32003;
    /// An agent named a cwd outside its own worktree.
    pub const CONFINED: i64 = -32004;
    /// A Session-locality request reached a process with no session table.
    pub const NEEDS_HOST: i64 = -32005;
}

/// `Null` is only legal on a response to a request whose id could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    Text(String),
    Null,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(into = "Raw", try_from = "Raw")]
pub enum Message {
    Request {
        id: RequestId,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: RequestId,
        result: Result<Value, RpcError>,
    },
}

/// The envelope as it appears on the wire; `Message` is the classified view of it.
#[derive(Serialize, Deserialize)]
struct Raw {
    jsonrpc: String,
    // An explicit `"id": null` and an explicit `"result": null` both carry meaning; a plain
    // `Option` would erase them.
    #[serde(
        default,
        deserialize_with = "present_id",
        skip_serializing_if = "Option::is_none"
    )]
    id: Option<RequestId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

fn present_id<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<RequestId>, D::Error> {
    RequestId::deserialize(deserializer).map(Some)
}

/// JSON-RPC requires `params` to be structured when present, so a null payload is omitted.
fn structured(params: Value) -> Option<Value> {
    if params.is_null() {
        None
    } else {
        Some(params)
    }
}

impl From<Message> for Raw {
    fn from(message: Message) -> Raw {
        let jsonrpc = JSONRPC.to_owned();
        match message {
            Message::Request { id, method, params } => Raw {
                jsonrpc,
                id: Some(id),
                method: Some(method),
                params: structured(params),
                result: None,
                error: None,
            },
            Message::Notification { method, params } => Raw {
                jsonrpc,
                id: None,
                method: Some(method),
                params: structured(params),
                result: None,
                error: None,
            },
            Message::Response { id, result } => {
                let (result, error) = match result {
                    Ok(value) => (Some(value), None),
                    Err(error) => (None, Some(error)),
                };
                Raw {
                    jsonrpc,
                    id: Some(id),
                    method: None,
                    params: None,
                    result,
                    error,
                }
            }
        }
    }
}

impl TryFrom<Raw> for Message {
    type Error = String;

    fn try_from(raw: Raw) -> Result<Message, String> {
        if raw.jsonrpc != JSONRPC {
            return Err(format!("jsonrpc must be \"2.0\", got {:?}", raw.jsonrpc));
        }
        match (raw.id, raw.method, raw.result, raw.error) {
            (Some(RequestId::Null), Some(_), _, _) => Err("a request id must not be null".into()),
            (Some(id), Some(method), None, None) => Ok(Message::Request {
                id,
                method,
                params: raw.params.unwrap_or(Value::Null),
            }),
            (None, Some(method), None, None) => Ok(Message::Notification {
                method,
                params: raw.params.unwrap_or(Value::Null),
            }),
            (Some(id), None, Some(result), None) => Ok(Message::Response {
                id,
                result: Ok(result),
            }),
            (Some(id), None, None, Some(error)) => Ok(Message::Response {
                id,
                result: Err(error),
            }),
            (_, None, Some(_), Some(_)) => {
                Err("a response carries either result or error, not both".into())
            }
            (None, None, _, _) => Err("a response needs an id".into()),
            (_, Some(_), _, _) => Err("a request carries no result or error".into()),
            (Some(_), None, None, None) => Err("neither a request nor a response".into()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("the peer closed the connection")]
    Closed,
    #[error("frame of {0} bytes exceeds the {MAX_FRAME_BYTES}-byte limit")]
    TooLarge(u32),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("frame is not a JSON-RPC 2.0 message: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn write_frame<W: Write>(writer: &mut W, message: &Message) -> Result<(), FrameError> {
    let body = serde_json::to_vec(message)?;
    let len = u32::try_from(body.len()).map_err(|_| FrameError::TooLarge(u32::MAX))?;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

/// `Closed` on a clean EOF before any length byte; a partial length or body is an I/O error.
pub fn read_frame<R: Read>(reader: &mut R) -> Result<Message, FrameError> {
    let mut len = [0u8; 4];
    let mut filled = 0;
    while filled < len.len() {
        let n = reader.read(&mut len[filled..])?;
        if n == 0 {
            if filled == 0 {
                return Err(FrameError::Closed);
            }
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
        }
        filled += n;
    }
    let len = u32::from_be_bytes(len);
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

/// Newline-delimited framing: one JSON-RPC message per line, no length prefix - MCP's stdio
/// transport (`docs/architecture/decisions.md` §22), next to this module's length-prefixed frame
/// the host socket uses. `Message` already models JSON-RPC 2.0 generically, so `jerry mcp` reuses
/// it rather than a second codec.
pub fn write_line_frame<W: Write + ?Sized>(
    writer: &mut W,
    message: &Message,
) -> Result<(), FrameError> {
    let mut body = serde_json::to_vec(message)?;
    body.push(b'\n');
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

/// Reads one line and parses it as a `Message`. `Closed` on a clean EOF before any byte of a new
/// line, matching [`read_frame`]'s own EOF contract; a line present but unparseable is
/// `FrameError::Json`, letting the caller answer a JSON-RPC parse error and keep reading rather
/// than tearing down the connection.
pub fn read_line_frame<R: BufRead + ?Sized>(reader: &mut R) -> Result<Message, FrameError> {
    let mut line = String::new();
    let read = reader.read_line(&mut line)?;
    if read == 0 {
        return Err(FrameError::Closed);
    }
    Ok(serde_json::from_str(line.trim_end_matches(['\n', '\r']))?)
}

#[cfg(test)]
mod frame_codec_tests {
    use super::{read_frame, write_frame, FrameError, Message, RequestId, RpcError};
    use std::io::Cursor;

    fn all_shapes() -> Vec<Message> {
        vec![
            Message::Request {
                id: RequestId::Number(7),
                method: "query/status".into(),
                params: serde_json::json!({ "cwd": "/repo" }),
            },
            Message::Notification {
                method: "event/session-exited".into(),
                params: serde_json::json!({ "session": "s1" }),
            },
            Message::Response {
                id: RequestId::Text("abc".into()),
                result: Ok(serde_json::Value::Null),
            },
            Message::Response {
                id: RequestId::Number(7),
                result: Err(RpcError::new(
                    super::rpc_code::METHOD_NOT_FOUND,
                    "no such method",
                )),
            },
            Message::Response {
                id: RequestId::Null,
                result: Err(RpcError::new(super::rpc_code::PARSE_ERROR, "unreadable")),
            },
        ]
    }

    #[test]
    fn every_message_shape_survives_a_frame_round_trip() {
        let mut buffer = Vec::new();
        for message in all_shapes() {
            write_frame(&mut buffer, &message).expect("encodes");
        }
        let mut cursor = Cursor::new(buffer);
        for expected in all_shapes() {
            let decoded = read_frame(&mut cursor).expect("decodes");
            assert_eq!(decoded, expected);
        }
        assert!(matches!(read_frame(&mut cursor), Err(FrameError::Closed)));
    }

    #[test]
    fn a_null_result_is_a_result_and_a_null_id_answers_an_unreadable_request() {
        let message: Message = serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"result":null}"#)
            .expect("a null result is valid");
        assert_eq!(
            message,
            Message::Response {
                id: RequestId::Number(1),
                result: Ok(serde_json::Value::Null)
            }
        );

        let message: Message = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"bad"}}"#,
        )
        .expect("a null-id error response is the spec's reply to an unparseable request");
        assert!(matches!(
            message,
            Message::Response {
                id: RequestId::Null,
                result: Err(_)
            }
        ));
    }

    #[test]
    fn a_null_payload_is_omitted_and_a_null_request_id_is_refused() {
        let json = serde_json::to_string(&Message::Notification {
            method: "event/x".into(),
            params: serde_json::Value::Null,
        })
        .expect("serializes");
        assert!(!json.contains("params"), "{json}");

        assert!(
            serde_json::from_str::<Message>(r#"{"jsonrpc":"2.0","id":null,"method":"hook"}"#)
                .is_err()
        );
    }

    #[test]
    fn malformed_envelopes_are_rejected_with_a_reason() {
        for json in [
            r#"{"jsonrpc":"1.0","id":1,"method":"hook"}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":1,"error":{"code":1,"message":"x"}}"#,
            r#"{"jsonrpc":"2.0","result":1}"#,
            r#"{"jsonrpc":"2.0","id":1}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"hook","result":1}"#,
        ] {
            assert!(
                serde_json::from_str::<Message>(json).is_err(),
                "{json} must be rejected"
            );
        }
    }

    #[test]
    fn an_oversized_length_prefix_is_refused_before_allocating() {
        let mut bytes = (super::MAX_FRAME_BYTES + 1).to_be_bytes().to_vec();
        bytes.extend_from_slice(b"{}");
        let err = read_frame(&mut Cursor::new(bytes)).expect_err("too large");
        assert!(matches!(err, FrameError::TooLarge(_)), "{err}");
    }

    #[test]
    fn a_truncated_body_is_an_io_error_not_a_hang() {
        let mut bytes = 10u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"{}");
        let err = read_frame(&mut Cursor::new(bytes)).expect_err("truncated");
        assert!(matches!(err, FrameError::Io(_)), "{err}");
    }
}

#[cfg(test)]
mod line_frame_codec_tests {
    use super::{read_line_frame, write_line_frame, FrameError, Message, RequestId};
    use std::io::{BufReader, Cursor};

    #[test]
    fn every_message_shape_survives_a_line_frame_round_trip() {
        let messages = [
            Message::Request {
                id: RequestId::Number(1),
                method: "initialize".into(),
                params: serde_json::json!({ "protocolVersion": "2025-11-25" }),
            },
            Message::Notification {
                method: "notifications/initialized".into(),
                params: serde_json::Value::Null,
            },
            Message::Response {
                id: RequestId::Number(1),
                result: Ok(serde_json::json!({ "tools": [] })),
            },
        ];
        let mut buffer = Vec::new();
        for message in &messages {
            write_line_frame(&mut buffer, message).expect("encodes");
        }
        // One line per message, newline-delimited rather than length-prefixed.
        assert_eq!(
            String::from_utf8(buffer.clone())
                .expect("utf8")
                .lines()
                .count(),
            messages.len()
        );
        let mut reader = BufReader::new(Cursor::new(buffer));
        for expected in messages {
            assert_eq!(read_line_frame(&mut reader).expect("decodes"), expected);
        }
        assert!(matches!(
            read_line_frame(&mut reader),
            Err(FrameError::Closed)
        ));
    }

    #[test]
    fn a_malformed_line_is_a_json_error_not_closed() {
        let mut reader = BufReader::new(Cursor::new(b"not json at all\n".to_vec()));
        let err = read_line_frame(&mut reader).expect_err("malformed");
        assert!(matches!(err, FrameError::Json(_)), "{err}");
    }

    #[test]
    fn a_trailing_carriage_return_is_stripped() {
        let mut reader = BufReader::new(Cursor::new(
            b"{\"jsonrpc\":\"2.0\",\"method\":\"ping\"}\r\n".to_vec(),
        ));
        let message = read_line_frame(&mut reader).expect("decodes despite CRLF");
        assert_eq!(
            message,
            Message::Notification {
                method: "ping".into(),
                params: serde_json::Value::Null,
            }
        );
    }
}
