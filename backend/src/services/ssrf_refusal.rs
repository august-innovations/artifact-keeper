//! The vocabulary of outbound SSRF refusals.
//!
//! Two producers refuse an outbound request under the SSRF policy:
//!
//! - the connect-time DNS guard ([`crate::services::ssrf_dns`]), which drops
//!   every address a host resolves to and fails the request with "all
//!   resolved addresses blocked by SSRF policy";
//! - the per-hop redirect policy ([`crate::services::http_client`]), which
//!   re-checks every redirect target and refuses the hop with "redirect
//!   target rejected by SSRF policy".
//!
//! Neither refusal is a typed error: the DNS guard's failure travels as an
//! `io::Error` inside a `reqwest` (or `git2`) error chain, so the message is
//! the only signal a caller gets. Consumers therefore classify a refusal by
//! its text, and BOTH phrases have to be recognized. A redirect refusal that
//! matched only the DNS guard's phrase fell through to the generic error
//! branch: the plugin install path answered 500 where the very same refusal
//! at connect time answers 400 (#4869).
//!
//! The phrases live here so a producer cannot drift away from the
//! classifier, and consumers classify through [`is_ssrf_refusal`] /
//! [`is_ssrf_refusal_error`] instead of matching a phrase inline.

/// Phrase the connect-time DNS guard's refusal carries.
pub const CONNECT_REFUSAL_PHRASE: &str = "blocked by SSRF policy";

/// Phrase the per-hop redirect refusal carries.
pub const REDIRECT_REFUSAL_PHRASE: &str = "rejected by SSRF policy";

/// True when `message` reports an outbound refusal by the SSRF policy —
/// either the connect-time DNS guard or a redirect hop.
pub fn is_ssrf_refusal(message: &str) -> bool {
    message.contains(CONNECT_REFUSAL_PHRASE) || message.contains(REDIRECT_REFUSAL_PHRASE)
}

/// True when `err` — or any error in its `source()` chain — reports an
/// outbound refusal by the SSRF policy.
///
/// The chain is walked because the refusals arrive wrapped: the DNS
/// guard's `io::Error` sits below `reqwest`'s connect error, and the
/// redirect refusal sits below `reqwest`'s redirect error.
pub fn is_ssrf_refusal_error(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if is_ssrf_refusal(&e.to_string()) {
            return true;
        }
        cur = e.source();
    }
    false
}

#[cfg(ak_test_shard = "services-2")]
#[cfg(test)]
mod tests {
    use super::{is_ssrf_refusal, is_ssrf_refusal_error};

    /// Both producers' phrases must be recognized: a refusal that only one
    /// of them produces must not read as an ordinary failure (#4869).
    #[test]
    fn both_refusal_phrases_are_recognized() {
        assert!(
            is_ssrf_refusal("all resolved addresses blocked by SSRF policy"),
            "the connect-time refusal must be recognized"
        );
        assert!(
            is_ssrf_refusal("redirect target rejected by SSRF policy"),
            "the redirect refusal must be recognized"
        );
        assert!(
            is_ssrf_refusal("git HTTP redirect refused: redirect target rejected by SSRF policy"),
            "the phrase must be recognized inside a longer message"
        );
    }

    /// An ordinary transport failure must not be mistaken for a refusal.
    #[test]
    fn an_ordinary_error_is_not_a_refusal() {
        assert!(!is_ssrf_refusal("connection refused"));
        assert!(!is_ssrf_refusal("error following redirect"));
    }

    /// The refusals arrive wrapped, so the chain is what carries them.
    #[test]
    fn a_refusal_nested_in_the_source_chain_is_recognized() {
        let err = std::io::Error::other("redirect target rejected by SSRF policy");
        assert!(is_ssrf_refusal_error(&err));

        let plain = std::io::Error::other("connection reset by peer");
        assert!(!is_ssrf_refusal_error(&plain));
    }
}
