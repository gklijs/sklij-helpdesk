//! Manual dark/light override, on top of the system default
//! `index.html`'s own `light-dark()`/`color-scheme: light dark` already
//! gives every page (including the very first paint, before this WASM
//! binary is even loaded). `<ThemeToggle/>` only exists to let a caller
//! override that default and have it stick - both this module and
//! `index.html`'s own inline bootstrap `<script>` read/write the
//! identical `localStorage` key, so a stored preference survives a full
//! reload, not just client-side route changes within this SPA.

use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::window;

const THEME_KEY: &str = "skilj_helpdesk_theme";

fn document_element() -> web_sys::Element {
    window()
        .expect("running in a browser")
        .document()
        .expect("document is available")
        .document_element()
        .expect("<html> is always present")
}

fn stored_theme() -> Option<String> {
    window()?
        .local_storage()
        .ok()
        .flatten()?
        .get_item(THEME_KEY)
        .ok()
        .flatten()
}

fn system_prefers_dark() -> bool {
    window()
        .and_then(|w| w.match_media("(prefers-color-scheme: dark)").ok().flatten())
        .is_some_and(|m| m.matches())
}

/// `"dark"` or `"light"` - never `"system"`: what's actually rendering
/// right now, resolving an absent override against the OS preference
/// exactly the way `index.html`'s own `light-dark()` does, so the
/// toggle's first click always flips relative to what the caller
/// actually sees, not an assumed default.
fn effective_theme() -> &'static str {
    match stored_theme().as_deref() {
        Some("dark") => "dark",
        Some("light") => "light",
        _ => {
            if system_prefers_dark() {
                "dark"
            } else {
                "light"
            }
        }
    }
}

fn apply(theme: &str) {
    let _ = document_element().set_attribute("data-theme", theme);
}

#[component]
pub fn ThemeToggle() -> impl IntoView {
    let (theme, set_theme) = signal(effective_theme().to_string());

    // Keeps this button's own label in sync with a live OS
    // color-scheme change while no explicit override is stored -
    // `index.html`'s own `light-dark()` CSS already re-resolves every
    // color on this event with no JS involved at all; this listener
    // exists only so *this* component's own displayed label doesn't go
    // stale relative to it (found in review: `theme` was previously
    // computed once at mount and never revisited, so a caller who
    // never touched the toggle and had their OS flip scheme mid-session
    // saw a label naming the mode they were already in, not the one a
    // click would switch to - one wasted click to reach the theme they
    // actually wanted, not a wrong theme, but wrong all the same).
    if let Some(mql) = window().and_then(|w| w.match_media("(prefers-color-scheme: dark)").ok().flatten()) {
        let on_change = Closure::<dyn Fn()>::new(move || {
            set_theme.set(effective_theme().to_string());
        });
        mql.set_onchange(Some(on_change.as_ref().unchecked_ref()));
        // Leaked deliberately: this listener needs to live exactly as
        // long as the app does - `<ThemeToggle/>` never unmounts in
        // this single-page app - and `Closure::forget()` is the
        // documented way to give a JS-held callback a `'static`
        // lifetime without an unmount hook whose only job would be
        // dropping it.
        on_change.forget();
    }

    let toggle = move |_| {
        let next = if theme.get() == "dark" {
            "light"
        } else {
            "dark"
        };
        apply(next);
        if let Some(storage) = window().and_then(|w| w.local_storage().ok().flatten()) {
            let _ = storage.set_item(THEME_KEY, next);
        }
        set_theme.set(next.to_string());
    };

    view! {
        <button
            type="button"
            class="theme-toggle"
            on:click=toggle
            title="Toggle dark/light mode"
        >
            // Text, not an emoji glyph - a plain button label needs no
            // color-emoji font, which not every Linux browser (this
            // showcase's own headless-Chromium screenshot pipeline
            // included) ships by default.
            {move || if theme.get() == "dark" { "Light mode" } else { "Dark mode" }}
        </button>
    }
}
