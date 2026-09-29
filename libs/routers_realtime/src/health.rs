//! HTTP probes backed by the process readiness state.

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;

use crate::lifecycle::{ReadinessWatcher, ReadyState};

#[derive(Clone)]
struct HealthState {
    readiness: ReadinessWatcher,
}

async fn live() -> StatusCode {
    StatusCode::OK
}

async fn ready(State(state): State<HealthState>) -> (StatusCode, &'static str) {
    let readiness = state.readiness.current();
    let status = if readiness == ReadyState::Ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, readiness.as_label())
}

/// Build liveness and readiness routes for a process's state watcher.
pub fn health_router(readiness: ReadinessWatcher) -> Router {
    Router::new()
        .route("/live", get(live))
        .route("/ready", get(ready))
        .with_state(HealthState { readiness })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::Readiness;

    #[tokio::test]
    async fn readiness_tracks_the_process_state() {
        let (setter, watcher) = Readiness::new();
        let state = State(HealthState { readiness: watcher });

        assert_eq!(
            ready(state.clone()).await,
            (StatusCode::SERVICE_UNAVAILABLE, "starting")
        );
        setter.set(ReadyState::Ready);
        assert_eq!(ready(state.clone()).await, (StatusCode::OK, "ready"));
        setter.set(ReadyState::Draining);
        assert_eq!(
            ready(state).await,
            (StatusCode::SERVICE_UNAVAILABLE, "draining")
        );
    }
}
