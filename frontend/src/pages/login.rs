use crate::auth::begin_login;
use leptos::prelude::*;

#[component]
pub fn Login() -> impl IntoView {
    view! {
        <h1>"SkilJ Helpdesk"</h1>
        <p>"Sign in with the real, self-hosted OIDC provider (Dex) this showcase runs."</p>
        <button on:click=move |_| begin_login()>"Log in"</button>
        <p>
            "Demo accounts (Dex's own login form asks for these next):"
        </p>
        <ul class="demo-accounts">
            <li>"Customer view: " <code>"customer@acme.example"</code> " / " <code>"customer-demo-pw"</code></li>
            <li>"Staff view: " <code>"lead@acme.example"</code> " / " <code>"staff-demo-pw"</code></li>
        </ul>
    }
}
