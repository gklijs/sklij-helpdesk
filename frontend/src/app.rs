use crate::pages::{callback::Callback, dashboard::Dashboard, login::Login};
use leptos::prelude::*;
use leptos_router::components::{Route, Router, Routes};
use leptos_router::path;

#[component]
pub fn App() -> impl IntoView {
    view! {
        <Router>
            <Routes fallback=|| view! { <p>"Not found."</p> }>
                <Route path=path!("/") view=Dashboard />
                <Route path=path!("/login") view=Login />
                <Route path=path!("/callback") view=Callback />
            </Routes>
        </Router>
    }
}
