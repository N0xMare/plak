//! Shared pure OAuth workflow helpers for subscription dialects.
//!
//! Credential-free math and string construction only: form encoding, authorize
//! URL building, device poll schedules, callback query parsing, access-token
//! timing, and HTTP/1.1 request/response framing. Shared async loopback accept
//! and JWKS cache ladders live in [`crate::oauth_orchestrator`]. Native auth
//! managers retain dialect HTTP, key crypto, catalog, and durable store
//! ownership.
//!
//! Pin tables live in [`crate::oauth_pins`]. Managers consume both as thin
//! table-driven dialects of one workflow.

mod authorize;
mod callback;
mod device_poll;
mod error;
mod form;
mod http11_get;
mod http11_response;

pub use authorize::{AuthorizeUrlRequest, build_authorization_url};
pub use callback::{
    CallbackQueryPolicy, OAuthCallbackOutcome, loopback_redirect_authority,
    parse_absolute_redirect_query, parse_loopback_callback_target, parse_oauth_callback_query,
    parse_remote_authorization_input,
};
pub use device_poll::DevicePollSchedule;
pub use error::OAuthWorkflowError;
pub use form::{
    access_needs_refresh, authorization_code_token_form, capped_deadline, decode_json_bounded,
    refresh_token_form, remaining_until, unsigned_access_token_exp, unverified_jwt_timing,
    urlencoded_form, urlencoded_form_bytes,
};
pub use http11_get::{Http11GetRequest, is_http_token_byte, parse_http11_get_request};
pub use http11_response::{
    LoopbackCallbackOutcome, LoopbackCallbackPage, format_http11_text_response,
};
