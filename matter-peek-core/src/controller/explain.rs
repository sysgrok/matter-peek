//! Errors in plain words: what a device's (or our stack's) error code means for
//! the one who asked, people and agents alike, with the code itself kept for
//! the experts.

use alloc::format;
use alloc::string::String;

use rs_matter::error::ErrorCode;

/// `code` in plain words, followed by the code itself, e.g. "it did not answer
/// in time (RxTimeout)".
pub fn describe(code: ErrorCode) -> String {
    match explain(code) {
        Some(words) => format!("{words} ({code:?})"),
        None => format!("{code:?}"),
    }
}

/// What `code` means when asking a device, if it is one worth explaining.
pub fn explain(code: ErrorCode) -> Option<&'static str> {
    Some(match code {
        ErrorCode::NotFound | ErrorCode::NodeNotFound => {
            "it could not be found on the network; it may be off or out of reach"
        }
        ErrorCode::TxTimeout | ErrorCode::RxTimeout => "it did not answer in time",
        ErrorCode::NoSession => "no secure session could be set up with it",
        ErrorCode::NoSpaceSessions => "it has no free sessions; try again shortly",
        ErrorCode::Busy => "it is busy; try again shortly",
        ErrorCode::ConstraintError => "it rejected a value as not allowed",
        ErrorCode::DynamicConstraintError => {
            "it rejected a value as not allowed in its current state"
        }
        ErrorCode::InvalidCommand | ErrorCode::InvalidDataType => {
            "it rejected the request as malformed"
        }
        ErrorCode::InvalidAction => "it refused the request",
        ErrorCode::CommandNotFound => "it does not support this command",
        ErrorCode::AttributeNotFound => "it does not have this attribute",
        ErrorCode::ClusterNotFound | ErrorCode::EndpointNotFound => {
            "it does not support this feature"
        }
        ErrorCode::UnsupportedAccess => "our fabric is not allowed to do this on it",
        ErrorCode::FailSafeRequired => "it needs an armed fail-safe for this",
        ErrorCode::NeedsTimedInteraction => "it needs a timed request for this",
        ErrorCode::ResourceExhausted | ErrorCode::NoSpace => "it has no room left for this",
        ErrorCode::NocFabricTableFull => "its fabric table is full",
        _ => return None,
    })
}
