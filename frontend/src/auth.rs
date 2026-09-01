//! Authorization Code + PKCE against a real OIDC provider (Dex) - the
//! actual login flow, not a shortcut. See `pages::login`/`pages::callback`
//! for where each half of this gets used, and `config`'s own doc
//! comments for the fixed client/redirect/issuer values.

use crate::config::{CLIENT_ID, DEMO_CUSTOMER_SUB, DEMO_STAFF_LEAD_SUB, DEX_ISSUER, REDIRECT_URI};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::{Digest, Sha256};
use web_sys::{window, Storage};

const TOKEN_KEY: &str = "skilj_helpdesk_token";
const ROLE_KEY: &str = "skilj_helpdesk_role";
const VERIFIER_KEY: &str = "skilj_helpdesk_pkce_verifier";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Customer,
    StaffLead,
}

fn local_storage() -> Storage {
    window()
        .expect("running in a browser")
        .local_storage()
        .expect("localStorage is available")
        .expect("localStorage is available")
}

fn session_storage() -> Storage {
    window()
        .expect("running in a browser")
        .session_storage()
        .expect("sessionStorage is available")
        .expect("sessionStorage is available")
}

fn generate_verifier() -> String {
    let mut bytes = [0u8; 64];
    getrandom::getrandom(&mut bytes).expect("getrandom works in wasm32 via the js feature");
    URL_SAFE_NO_PAD.encode(bytes)
}

fn challenge_for(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

/// Kicks off the real flow: stores a fresh PKCE verifier, then navigates
/// the whole page to Dex's own `/auth` endpoint - Dex's own login form
/// is what the user actually sees and types credentials into next, not
/// anything this app renders.
pub fn begin_login() {
    let verifier = generate_verifier();
    session_storage()
        .set_item(VERIFIER_KEY, &verifier)
        .expect("sessionStorage.setItem");

    let url = web_sys::Url::new(&format!("{DEX_ISSUER}/auth")).expect("DEX_ISSUER is a valid URL");
    let params = url.search_params();
    params.set("client_id", CLIENT_ID);
    params.set("redirect_uri", REDIRECT_URI);
    params.set("response_type", "code");
    params.set("scope", "openid email profile");
    params.set("code_challenge", &challenge_for(&verifier));
    params.set("code_challenge_method", "S256");

    window()
        .expect("running in a browser")
        .location()
        .set_href(&url.href())
        .expect("navigating to Dex's own /auth endpoint");
}

/// The PKCE verifier `begin_login` stashed - `pages::callback`'s own
/// token exchange needs it exactly once, then it's spent (see
/// `clear_verifier`).
pub fn take_verifier() -> Option<String> {
    let storage = session_storage();
    let verifier = storage.get_item(VERIFIER_KEY).ok().flatten();
    let _ = storage.remove_item(VERIFIER_KEY);
    verifier
}

/// Decodes (not verifies - there's nothing here that needs to trust
/// this claim for security purposes, only to pick which view to render;
/// skilj-graphql is what actually verifies the token, server-side, on
/// every request) the JWT's own `sub` claim.
pub fn decode_jwt_sub(jwt: &str) -> Option<String> {
    let payload_b64 = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload_b64).ok()?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    json.get("sub")?.as_str().map(str::to_string)
}

fn role_for_sub(sub: &str) -> Option<Role> {
    match sub {
        s if s == DEMO_CUSTOMER_SUB => Some(Role::Customer),
        s if s == DEMO_STAFF_LEAD_SUB => Some(Role::StaffLead),
        _ => None,
    }
}

/// Stores the real ID token (what every GraphQL request authenticates
/// with) plus the view role decoded from its own `sub` - called once,
/// right after a successful token exchange.
pub fn store_session(id_token: &str) -> Result<Role, String> {
    let sub = decode_jwt_sub(id_token).ok_or("the token has no decodable sub claim")?;
    let role = role_for_sub(&sub).ok_or_else(|| {
        format!("{sub:?} isn't one of this showcase's two seeded demo identities")
    })?;
    let storage = local_storage();
    storage
        .set_item(TOKEN_KEY, id_token)
        .map_err(|_| "localStorage.setItem failed".to_string())?;
    storage
        .set_item(ROLE_KEY, if role == Role::Customer { "customer" } else { "staff" })
        .map_err(|_| "localStorage.setItem failed".to_string())?;
    Ok(role)
}

pub fn current_session() -> Option<(String, Role)> {
    let storage = local_storage();
    let token = storage.get_item(TOKEN_KEY).ok().flatten()?;
    let role = match storage.get_item(ROLE_KEY).ok().flatten()?.as_str() {
        "customer" => Role::Customer,
        _ => Role::StaffLead,
    };
    Some((token, role))
}

pub fn log_out() {
    let storage = local_storage();
    let _ = storage.remove_item(TOKEN_KEY);
    let _ = storage.remove_item(ROLE_KEY);
}
