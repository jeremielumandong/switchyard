//! Readable RDP errors. IronRDP's errors print as `[context @ file:line] kind`, with
//! the cause (sspi's error, the server's NTSTATUS) only in the source chain, so the
//! raw text names a source path and hides why the sign-in failed.

use ironrdp_connector::sspi::{self, credssp::NStatusCode};
use ironrdp_connector::{ConnectorError, ConnectorErrorKind};

use super::RdpError;

/// The [`RdpError`] for a failed connect or sign-in step.
pub fn connector_err(e: ConnectorError) -> RdpError {
    tracing::debug!(error = %e.report(), "rdp connector error");
    match e.kind() {
        ConnectorErrorKind::Credssp(s) => RdpError::Auth(credssp_message(s)),
        ConnectorErrorKind::AccessDenied => {
            RdpError::Auth("the server refused the user name or password".into())
        }
        ConnectorErrorKind::Reason(r) => RdpError::Protocol(r.clone()),
        _ => RdpError::Protocol(chain(&e)),
    }
}

/// The [`RdpError`] for a failure while the session runs.
pub fn session_err<E: std::error::Error>(e: E) -> RdpError {
    RdpError::Protocol(chain(&e))
}

/// `e` and its causes, joined with `: `, without IronRDP's `[… @ file:line]` prefixes
/// and its placeholder kinds ("custom error", "general error").
fn chain(e: &dyn std::error::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut next = Some(e);
    while let Some(err) = next {
        let text = strip_location(&err.to_string());
        if !text.is_empty() && !parts.iter().any(|p| p.ends_with(&text)) {
            parts.push(text);
        }
        next = err.source();
    }
    if parts.is_empty() {
        "the connection failed".into()
    } else {
        parts.join(": ")
    }
}

/// `[context @ file:line] kind` as `context: kind` (or just `context` when the kind
/// says nothing); any other text unchanged.
fn strip_location(text: &str) -> String {
    let Some(rest) = text.strip_prefix('[') else {
        return text.trim().to_owned();
    };
    let Some((head, kind)) = rest.split_once("] ") else {
        return text.trim().to_owned();
    };
    let Some((context, _location)) = head.split_once(" @ ") else {
        return text.trim().to_owned();
    };
    let context = context.trim();
    let kind = kind.trim();
    let placeholder = matches!(kind, "custom error" | "general error" | "CredSSP" | "");
    match (context.is_empty(), placeholder) {
        (true, _) => kind.to_owned(),
        (false, true) => context.to_owned(),
        (false, false) => format!("{context}: {kind}"),
    }
}

/// What a CredSSP failure means for the user: the server's status when it sent one,
/// else sspi's own description.
fn credssp_message(e: &sspi::Error) -> String {
    let Some(status) = e.nstatus else {
        return match e.error_type {
            sspi::ErrorKind::MessageAltered => format!(
                "the server's identity check failed ({}). Something between this \
                 computer and the server may be intercepting the connection",
                e.description
            ),
            _ => e.description.clone(),
        };
    };
    let reason = match status {
        NStatusCode::LOGON_FAILURE | NStatusCode::WRONG_PASSWORD | NStatusCode::NO_SUCH_USER => {
            "the user name or password is wrong. For a domain account, put the domain's short \
             name in Domain (or type DOMAIN\\user), or sign in as user@domain"
        }
        NStatusCode::INVALID_ACCOUNT_NAME => "the server does not recognize that user name",
        NStatusCode::ACCOUNT_DISABLED => "the account is disabled",
        NStatusCode::ACCOUNT_LOCKED_OUT => "the account is locked out",
        ACCOUNT_EXPIRED => "the account has expired",
        NStatusCode::PASSWORD_EXPIRED | NStatusCode::PASSWORD_MUST_CHANGE => {
            "the password has expired and must be changed before signing in here"
        }
        NStatusCode::ACCOUNT_RESTRICTION
        | NStatusCode::INVALID_LOGON_HOURS
        | NStatusCode::INVALID_WORKSTATION => {
            "the account is not allowed to sign in from here (an account restriction)"
        }
        NStatusCode::LOGON_TYPE_NOT_GRANTED | NStatusCode::LOGON_NOT_GRANTED => {
            "the account is not allowed to sign in over Remote Desktop on this server"
        }
        NStatusCode::NO_LOGON_SERVERS => "the server could not reach a domain controller",
        NStatusCode::TIME_DIFFERENCE_AT_DC => {
            "the clocks of this computer, the server or its domain controller are too far apart"
        }
        NStatusCode::SMARTCARD_LOGON_REQUIRED => "the account must sign in with a smart card",
        NTLM_BLOCKED => {
            "the server's domain blocks NTLM sign-in, which the in-app client uses; \
             open the host in Remote Desktop instead"
        }
        NStatusCode::CTX_LOGON_DISABLED => "Remote Desktop sign-in is disabled on this server",
        _ => return format!("the server refused the sign-in ({status})"),
    };
    format!("{reason} ({})", status_name(status))
}

/// The status's name without the `STATUS_` prefix, or its hex code.
fn status_name(status: NStatusCode) -> String {
    match status.name() {
        Some(name) => name.trim_start_matches("STATUS_").to_owned(),
        None => format!("{:#010x}", status.0),
    }
}

/// `STATUS_ACCOUNT_EXPIRED` (sspi has no constant for it).
const ACCOUNT_EXPIRED: NStatusCode = NStatusCode(0xc000_0193);
/// `STATUS_NTLM_BLOCKED` (sspi has no constant for it).
const NTLM_BLOCKED: NStatusCode = NStatusCode(0xc000_0418);

#[cfg(test)]
mod tests {
    use super::*;

    fn server_status(code: NStatusCode) -> ConnectorError {
        let e = sspi::Error::new_with_nstatus(
            sspi::ErrorKind::InvalidToken,
            "CredSSP server returned an error status",
            code,
        );
        ConnectorError::new("CredSSP", ConnectorErrorKind::Credssp(e))
    }

    #[test]
    fn a_refused_password_reads_as_one() {
        let msg = connector_err(server_status(NStatusCode::LOGON_FAILURE)).to_string();
        assert!(msg.starts_with("sign-in failed: the user name or password is wrong"));
        assert!(msg.ends_with("(LOGON_FAILURE)"), "{msg}");
        assert!(!msg.contains(".rs"), "{msg}");
    }

    #[test]
    fn server_statuses_name_the_cause() {
        let msg = connector_err(server_status(NStatusCode::LOGON_TYPE_NOT_GRANTED)).to_string();
        assert!(
            msg.contains("not allowed to sign in over Remote Desktop"),
            "{msg}"
        );
        let msg = connector_err(server_status(NStatusCode(0xc000_0418))).to_string();
        assert!(msg.contains("blocks NTLM"), "{msg}");
        assert!(msg.contains("0xc0000418"), "{msg}");
        let msg = connector_err(server_status(NStatusCode(0xc0de_0001))).to_string();
        assert_eq!(
            msg,
            "sign-in failed: the server refused the sign-in (NSTATUS code 0xc0de0001)"
        );
    }

    #[test]
    fn client_side_credssp_errors_show_sspis_description() {
        let e = sspi::Error::new(
            sspi::ErrorKind::MessageAltered,
            "could not verify a public key echo",
        );
        let msg = connector_err(ConnectorError::new(
            "CredSSP",
            ConnectorErrorKind::Credssp(e),
        ))
        .to_string();
        assert!(msg.contains("identity check failed"), "{msg}");
        assert!(msg.contains("could not verify a public key echo"), "{msg}");
    }

    #[test]
    fn other_errors_drop_the_source_location() {
        let e = ironrdp_connector::general_err!("invalid connector state for CredSSP sequence");
        let msg = connector_err(e).to_string();
        assert_eq!(msg, "invalid connector state for CredSSP sequence");
        let e = ironrdp_connector::reason_err!("Negotiation", "server said no");
        assert_eq!(connector_err(e).to_string(), "server said no");
    }

    #[test]
    fn locations_are_stripped() {
        assert_eq!(
            strip_location(r"[write all @ C:\Users\x\src\connector.rs:159] custom error"),
            "write all"
        );
        assert_eq!(
            strip_location("[TsRequest @ a.rs:1] decode"),
            "TsRequest: decode"
        );
        assert_eq!(strip_location("plain text"), "plain text");
    }
}
