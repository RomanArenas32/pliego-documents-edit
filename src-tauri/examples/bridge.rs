//! Test bench: `pnpm build && cargo run --example bridge --features bridge`
//! and open http://127.0.0.1:1430 in a browser.

fn main() {
    let dist = concat!(env!("CARGO_MANIFEST_DIR"), "/../dist");
    let port = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(1430);
    pliego_lib::bridge::run(dist.as_ref(), port);
}
