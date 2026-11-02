//! The one JSON-RPC error-code table and the one WebSocket close-code table
//! of every client listener (spec: workspace-architecture, JSON-RPC framing
//! and error codes). Application errors map from the error taxonomy, so a
//! kind always travels as the same code.

use pmt_core::error::ErrorKind;
use serde::Serialize;

/// An unparseable frame.
pub const PARSE_ERROR: i64 = -32_700;
/// A frame that is not a request object.
pub const INVALID_REQUEST: i64 = -32_600;
/// A method the catalog does not declare.
pub const METHOD_NOT_FOUND: i64 = -32_601;
/// Params that violate the method's contract.
pub const INVALID_PARAMS: i64 = -32_602;

/// The JSON-RPC code of an application error of `kind`.
#[must_use]
pub const fn for_kind(kind: ErrorKind) -> i64 {
    match kind {
        ErrorKind::Auth => -32_001,
        ErrorKind::PermissionDenied => -32_002,
        ErrorKind::NotFound => -32_003,
        ErrorKind::Conflict => -32_004,
        ErrorKind::BudgetExceeded => -32_005,
        ErrorKind::Unavailable => -32_006,
        ErrorKind::Cancelled => -32_007,
        ErrorKind::Protocol => -32_008,
        ErrorKind::InvalidInput | ErrorKind::Usage => -32_010,
        ErrorKind::Provider => -32_011,
        ErrorKind::Tool => -32_012,
        ErrorKind::Sandbox => -32_013,
        ErrorKind::Config => -32_014,
        ErrorKind::Io | ErrorKind::Internal => -32_099,
    }
}

/// One row of the published error table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CodeRow {
    /// The JSON-RPC `error.code`.
    pub code: i64,
    /// What it means (the error kind for application codes).
    pub meaning: &'static str,
}

/// Every error code, in the order the spec lists them.
pub const ERROR_TABLE: &[CodeRow] = &[
    row(PARSE_ERROR, "parse_error"),
    row(INVALID_REQUEST, "invalid_request"),
    row(METHOD_NOT_FOUND, "unknown_method"),
    row(INVALID_PARAMS, "invalid_params"),
    row(-32_001, "auth"),
    row(-32_002, "permission_denied"),
    row(-32_003, "not_found"),
    row(-32_004, "conflict"),
    row(-32_005, "budget_exceeded"),
    row(-32_006, "unavailable"),
    row(-32_007, "cancelled"),
    row(-32_008, "protocol"),
    row(-32_010, "invalid_input"),
    row(-32_011, "provider"),
    row(-32_012, "tool"),
    row(-32_013, "sandbox"),
    row(-32_014, "config"),
    row(-32_099, "internal"),
];

const fn row(code: i64, meaning: &'static str) -> CodeRow {
    CodeRow { code, meaning }
}

/// The WebSocket close codes every client listener uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloseCode {
    /// 1000: normal closure.
    Normal,
    /// 1001: the daemon is shutting down.
    ShuttingDown,
    /// 1011: internal error.
    Internal,
    /// 4401: bad credential (ticket).
    BadCredential,
    /// 4403: not allowed (Host, Origin, peer or profile).
    NotAllowed,
    /// 4404: endpoint disabled on this surface.
    EndpointDisabled,
    /// 4408: heartbeat timeout.
    HeartbeatTimeout,
    /// 4410: the session ended.
    SessionEnded,
    /// 4413: frame too large.
    FrameTooLarge,
    /// 4426: protocol version unsupported.
    ProtocolUnsupported,
}

impl CloseCode {
    /// Every close code, in table order.
    pub const ALL: [Self; 10] = [
        Self::Normal,
        Self::ShuttingDown,
        Self::Internal,
        Self::BadCredential,
        Self::NotAllowed,
        Self::EndpointDisabled,
        Self::HeartbeatTimeout,
        Self::SessionEnded,
        Self::FrameTooLarge,
        Self::ProtocolUnsupported,
    ];

    /// The numeric code.
    #[must_use]
    pub const fn code(self) -> u16 {
        match self {
            Self::Normal => 1000,
            Self::ShuttingDown => 1001,
            Self::Internal => 1011,
            Self::BadCredential => 4401,
            Self::NotAllowed => 4403,
            Self::EndpointDisabled => 4404,
            Self::HeartbeatTimeout => 4408,
            Self::SessionEnded => 4410,
            Self::FrameTooLarge => 4413,
            Self::ProtocolUnsupported => 4426,
        }
    }

    /// Its name in `schema/rpc.json`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::ShuttingDown => "shutting_down",
            Self::Internal => "internal",
            Self::BadCredential => "bad_credential",
            Self::NotAllowed => "not_allowed",
            Self::EndpointDisabled => "endpoint_disabled",
            Self::HeartbeatTimeout => "heartbeat_timeout",
            Self::SessionEnded => "session_ended",
            Self::FrameTooLarge => "frame_too_large",
            Self::ProtocolUnsupported => "protocol_unsupported",
        }
    }

    /// The close reason sent with the code.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::ShuttingDown => "shutting down",
            Self::Internal => "internal error",
            Self::BadCredential => "bad credential",
            Self::NotAllowed => "not allowed",
            Self::EndpointDisabled => "endpoint disabled",
            Self::HeartbeatTimeout => "heartbeat timeout",
            Self::SessionEnded => "session ended",
            Self::FrameTooLarge => "frame too large",
            Self::ProtocolUnsupported => "protocol version unsupported",
        }
    }

    /// The code a received close frame carries, when it is in the table.
    #[must_use]
    pub fn from_code(code: u16) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.code() == code)
    }
}

#[cfg(test)]
mod tests {
    use pmt_core::error::ErrorKind;

    use super::{CloseCode, ERROR_TABLE, for_kind};

    #[test]
    fn every_kind_maps_into_the_published_table() {
        for kind in ErrorKind::ALL {
            let code = for_kind(kind);
            assert!(
                ERROR_TABLE.iter().any(|row| row.code == code),
                "{kind}: {code}"
            );
        }
        assert_eq!(for_kind(ErrorKind::BudgetExceeded), -32_005);
        assert_eq!(for_kind(ErrorKind::Protocol), -32_008);
    }

    #[test]
    fn close_codes_are_the_shared_table() {
        let codes: Vec<u16> = CloseCode::ALL.iter().map(|c| c.code()).collect();
        assert_eq!(
            codes,
            [1000, 1001, 1011, 4401, 4403, 4404, 4408, 4410, 4413, 4426]
        );
        assert_eq!(
            CloseCode::from_code(4426),
            Some(CloseCode::ProtocolUnsupported)
        );
        assert_eq!(CloseCode::from_code(4999), None);
    }
}
