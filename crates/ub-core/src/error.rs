use thiserror::Error;

/// Status codes matching FR-ERR requirements.
pub type UbStatus = u32;

pub const UB_OK: UbStatus = 0;
pub const UB_ERR_ADDR_INVALID: UbStatus = 1;
pub const UB_ERR_PERM_DENIED: UbStatus = 2;
pub const UB_ERR_ALIGNMENT: UbStatus = 3;
pub const UB_ERR_LINK_DOWN: UbStatus = 4;
pub const UB_ERR_NO_RESOURCES: UbStatus = 5;
pub const UB_ERR_TIMEOUT: UbStatus = 6;
pub const UB_ERR_PAYLOAD_TOO_LARGE: UbStatus = 7;
pub const UB_ERR_FLUSHED: UbStatus = 8;
pub const UB_ERR_INTERNAL: UbStatus = 9;

/// Convert status code to human-readable string.
pub fn status_to_string(status: UbStatus) -> &'static str {
    match status {
        UB_OK => "OK",
        UB_ERR_ADDR_INVALID => "ADDR_INVALID",
        UB_ERR_PERM_DENIED => "PERM_DENIED",
        UB_ERR_ALIGNMENT => "ALIGNMENT",
        UB_ERR_LINK_DOWN => "LINK_DOWN",
        UB_ERR_NO_RESOURCES => "NO_RESOURCES",
        UB_ERR_TIMEOUT => "TIMEOUT",
        UB_ERR_PAYLOAD_TOO_LARGE => "PAYLOAD_TOO_LARGE",
        UB_ERR_FLUSHED => "FLUSHED",
        UB_ERR_INTERNAL => "INTERNAL",
        _ => "UNKNOWN",
    }
}

#[derive(Error, Debug)]
pub enum UbError {
    #[error("address invalid")]
    AddrInvalid,

    #[error("permission denied")]
    PermDenied,

    #[error("alignment error: address must be 8-byte aligned for atomic operations")]
    Alignment,

    #[error("link down")]
    LinkDown,

    #[error("no resources")]
    NoResources,

    #[error("timeout")]
    Timeout,

    #[error("payload too large")]
    PayloadTooLarge,

    #[error("flushed")]
    Flushed,

    #[error("internal error: {0}")]
    Internal(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("config error: {0}")]
    Config(String),
}

impl UbError {
    pub fn status_code(&self) -> UbStatus {
        match self {
            UbError::AddrInvalid => UB_ERR_ADDR_INVALID,
            UbError::PermDenied => UB_ERR_PERM_DENIED,
            UbError::Alignment => UB_ERR_ALIGNMENT,
            UbError::LinkDown => UB_ERR_LINK_DOWN,
            UbError::NoResources => UB_ERR_NO_RESOURCES,
            UbError::Timeout => UB_ERR_TIMEOUT,
            UbError::PayloadTooLarge => UB_ERR_PAYLOAD_TOO_LARGE,
            UbError::Flushed => UB_ERR_FLUSHED,
            UbError::Internal(_) => UB_ERR_INTERNAL,
            UbError::Io(_) => UB_ERR_INTERNAL,
            UbError::Config(_) => UB_ERR_INTERNAL,
        }
    }
}
