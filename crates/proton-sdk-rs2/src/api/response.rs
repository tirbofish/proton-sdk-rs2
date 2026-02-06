use crate::{EventId, SessionId, UserId, api::ApiResponse};

pub struct SesisonInitiationResponse {
    version: i32,
    modulus: String,
    server_ephemeral: Vec<u8>,
    salt: Vec<u8>,
    srp_session_id: String,
    response: ApiResponse,
}

pub struct AuthenticationResponse {
    session_id: SessionId,
    user_id: UserId,
    event_id: Option<EventId>,
}

pub struct RefreshSessionResponse {
    pub access_token: String,
    pub refresh_token: String,
}