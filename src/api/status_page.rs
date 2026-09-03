//! Author: TechnoL0g
//!
//! `GET /status` — tiny self-contained HTML page (no assets) that polls the JSON API every 5 s:
//! height, sync state, peers, delegate slots and forging history. Local operators only (127.0.0.1:4003).

use axum::response::Html;

const PAGE: &str = include_str!("status.html");

pub async fn page() -> Html<&'static str> {
    Html(PAGE)
}
