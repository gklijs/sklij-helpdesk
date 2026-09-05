mod api;
mod app;
mod auth;
mod config;
mod model;
mod pages;
mod theme;

use app::App;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
