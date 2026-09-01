use crate::{auth, config};
use leptos::prelude::*;
use leptos::task::spawn_local;
use web_sys::window;

#[component]
pub fn Callback() -> impl IntoView {
    let (error, set_error) = signal(None::<String>);

    Effect::new(move |_| {
        spawn_local(async move {
            match exchange().await {
                Ok(()) => {
                    let _ = window().expect("browser").location().set_href("/");
                }
                Err(e) => set_error.set(Some(e)),
            }
        });
    });

    view! {
        <p>"Signing you in..."</p>
        {move || {
            error
                .get()
                .map(|e| view! { <p class="error">{format!("Sign-in failed: {e}")}</p> })
        }}
        <p><a href="/login">"Back to login"</a></p>
    }
}

/// The other half of `auth::begin_login` - exchanges the `code` Dex's
/// own redirect carried for a real ID token, over `/token`, with the
/// PKCE verifier proving this is the same browser session that started
/// the flow.
async fn exchange() -> Result<(), String> {
    let location = window().expect("browser").location();
    let search = location.search().map_err(|_| "no query string on this URL".to_string())?;
    let params = web_sys::UrlSearchParams::new_with_str(&search)
        .map_err(|_| "malformed query string".to_string())?;
    let code = params
        .get("code")
        .ok_or("no ?code= parameter - Dex's own login/consent flow didn't complete")?;
    let verifier = auth::take_verifier()
        .ok_or("no PKCE verifier stored for this session - did you navigate here directly?")?;

    let form = web_sys::UrlSearchParams::new().map_err(|_| "couldn't build a form body".to_string())?;
    form.append("grant_type", "authorization_code");
    form.append("code", &code);
    form.append("redirect_uri", config::REDIRECT_URI);
    form.append("client_id", config::CLIENT_ID);
    form.append("code_verifier", &verifier);

    let response = gloo_net::http::Request::post(&format!("{}/token", config::DEX_ISSUER))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(String::from(form.to_string()))
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| format!("couldn't reach Dex's own /token endpoint: {e}"))?;

    let json: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("Dex's /token response wasn't valid JSON: {e}"))?;
    let id_token = json["id_token"]
        .as_str()
        .ok_or_else(|| format!("no id_token in Dex's response: {json}"))?;

    auth::store_session(id_token)?;
    Ok(())
}
