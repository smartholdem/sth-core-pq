//! Startup relay ranking: probe every relay's `/ping` in parallel, start the endpoint on the reachable ones (iroh
//! then picks the lowest-latency one as home relay) and re-add the dead ones once they answer again — so a down
//! relay no longer costs a 30 s connect timeout at boot.

use iroh::{Endpoint, RelayMap, RelayUrl};
use std::sync::Arc;
use std::time::{Duration, Instant};

const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const RETRY_EVERY: Duration = Duration::from_secs(60);

async fn ping(client: &reqwest::Client, url: &RelayUrl) -> Option<Duration> {
    let probe = format!("{}/ping", url.as_str().trim_end_matches('/'));
    let started = Instant::now();
    match client.get(&probe).send().await {
        Ok(r) if r.status().is_success() => Some(started.elapsed()),
        _ => None,
    }
}

/// Latency of every relay, fastest first, unreachable (`None`) last.
pub async fn rank(urls: &[RelayUrl]) -> Vec<(RelayUrl, Option<Duration>)> {
    let Ok(client) = reqwest::Client::builder().timeout(PROBE_TIMEOUT).build() else {
        return urls.iter().map(|u| (u.clone(), None)).collect();
    };
    let mut out: Vec<(RelayUrl, Option<Duration>)> = futures::future::join_all(urls.iter().map(|u| {
        let client = client.clone();
        async move { (u.clone(), ping(&client, u).await) }
    }))
    .await;
    out.sort_by_key(|(_, d)| d.unwrap_or(Duration::MAX));
    out
}

/// Logs the ranking and removes the unreachable relays from `map` (only if at least one answered); returns them.
pub async fn prune_dead(map: &RelayMap) -> Vec<RelayUrl> {
    let urls: Vec<RelayUrl> = map.urls();
    if urls.len() < 2 {
        return Vec::new();
    }
    let ranked = rank(&urls).await;
    let text: Vec<String> = ranked.iter().map(|(u, d)| match d { Some(d) => format!("{u} {} ms", d.as_millis()), None => format!("{u} unreachable") }).collect();
    let dead: Vec<RelayUrl> = ranked.iter().filter(|(_, d)| d.is_none()).map(|(u, _)| u.clone()).collect();
    if dead.len() == urls.len() {
        tracing::warn!(relays = text.join(", "), "no relay answered the startup probe — keeping all of them");
        return Vec::new();
    }
    tracing::info!(relays = text.join(", "), fastest = %ranked[0].0, "relay ranking at startup");
    for u in &dead {
        map.remove(u);
    }
    dead
}

/// Puts the relays removed at startup back as soon as they answer `/ping` again.
pub fn readd_when_alive(endpoint: Endpoint, mut dead: Vec<RelayUrl>) {
    if dead.is_empty() {
        return;
    }
    tokio::spawn(async move {
        while !dead.is_empty() && !endpoint.is_closed() {
            tokio::time::sleep(RETRY_EVERY).await;
            let ranked = rank(&dead).await;
            for (u, d) in ranked {
                if let Some(d) = d {
                    endpoint.insert_relay(u.clone(), Arc::new(u.clone().into())).await;
                    tracing::info!(relay = %u, ms = d.as_millis() as u64, "relay is back — re-added to the relay map");
                    dead.retain(|x| *x != u);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dead_relays_are_ranked_last_and_pruned() {
        let app = axum::Router::new().route("/ping", axum::routing::get(|| async { "pong" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let alive: RelayUrl = format!("http://127.0.0.1:{port}").parse().unwrap();
        let dead: RelayUrl = "http://127.0.0.1:9".parse().unwrap();
        let ranked = rank(&[dead.clone(), alive.clone()]).await;
        assert_eq!(ranked[0].0, alive);
        assert!(ranked[0].1.is_some() && ranked[1].1.is_none());
        let map: RelayMap = [dead.clone(), alive.clone()].into_iter().collect();
        assert_eq!(prune_dead(&map).await, vec![dead.clone()]);
        assert_eq!(map.urls::<Vec<RelayUrl>>(), vec![alive]);
        let all_dead: RelayMap = [dead.clone(), "http://127.0.0.1:7".parse().unwrap()].into_iter().collect();
        assert!(prune_dead(&all_dead).await.is_empty(), "nothing answers: keep every relay");
        assert_eq!(all_dead.urls::<Vec<RelayUrl>>().len(), 2);
    }
}
