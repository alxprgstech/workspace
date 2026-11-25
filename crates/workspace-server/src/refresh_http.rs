use super::*;
use crate::auth_store::{RefreshDecision, RefreshLease};
use crate::sso::SsoError;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{Instant, sleep, timeout};

const WAIT_LIMIT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

async fn read_session(
    state: Arc<AuthState>,
    handle: String,
    csrf: Option<String>,
) -> Result<Option<SessionInfo>, StatusCode> {
    blocking(move || {
        state
            .store
            .get_session(&state.config, &handle, csrf.as_deref())
    })
    .await
}

fn needs_refresh(info: &SessionInfo) -> bool {
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return true;
    };

    let now = now.as_secs() as i64;

    info.access_expires_at_unix < info.expires_at_unix && info.access_expires_at_unix <= now + 30
}

async fn perform_refresh(state: Arc<AuthState>, lease: RefreshLease) -> Result<bool, StatusCode> {
    // No token request has been submitted yet.
    let client = match timeout(Duration::from_secs(15), state.sso_client()).await {
        Ok(Ok(client)) => client,
        _ => {
            tracing::warn!("SSO discovery unavailable before refresh");

            let released =
                blocking(move || state.store.release_unsubmitted_refresh(&lease)).await?;

            return if released {
                Err(StatusCode::SERVICE_UNAVAILABLE)
            } else {
                Ok(false)
            };
        }
    };

    // Never reuse the old token after an uncertain exchange.
    let result = timeout(
        Duration::from_secs(30),
        client.refresh_tokens(
            state.secrets.client_secret(),
            lease.refresh_token(),
            lease.subject(),
            lease.previous_id_token(),
        ),
    )
    .await;

    match result {
        Ok(Ok(refreshed)) => {
            let saved = blocking(
                move || match state.store.finish_refresh(&lease, refreshed) {
                    Ok(saved) => {
                        if !saved {
                            state.store.abort_refresh(&lease)?;
                        }

                        Ok(saved)
                    }
                    Err(error) => {
                        let _ = state.store.abort_refresh(&lease);

                        Err(error)
                    }
                },
            )
            .await?;

            if saved {
                tracing::info!("SSO session refreshed");
            }

            Ok(saved)
        }

        Ok(Err(SsoError::RefreshRejected)) => {
            blocking(move || state.store.abort_refresh(&lease)).await?;

            tracing::info!("SSO refresh grant rejected");

            Ok(false)
        }

        failure => {
            if let Ok(Err(error)) = failure {
                tracing::warn!(%error, "SSO refresh failed");
            } else {
                tracing::warn!("SSO refresh timed out");
            }

            // A rotated signing key must not stay pinned in the cache.
            state.invalidate_discovery().await;

            blocking(move || state.store.abort_refresh(&lease)).await?;

            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

pub(super) async fn resolve_session(
    state: Arc<AuthState>,
    handle: String,
    csrf: Option<String>,
) -> Result<Option<SessionInfo>, StatusCode> {
    // Fast path: no write transaction for fresh sessions.
    let current = read_session(Arc::clone(&state), handle.clone(), csrf.clone()).await?;

    if let Some(info) = current
        && !needs_refresh(&info)
    {
        return Ok(Some(info));
    }

    let deadline = Instant::now() + WAIT_LIMIT;

    loop {
        let store_state = Arc::clone(&state);
        let request_handle = handle.clone();
        let request_csrf = csrf.clone();

        let decision = blocking(move || {
            store_state.store.begin_refresh(
                &store_state.config,
                &request_handle,
                request_csrf.as_deref(),
            )
        })
        .await?;

        match decision {
            RefreshDecision::Missing => return Ok(None),

            RefreshDecision::NotNeeded => {
                let current =
                    read_session(Arc::clone(&state), handle.clone(), csrf.clone()).await?;

                if current.is_some() {
                    return Ok(current);
                }
            }

            RefreshDecision::Pending => {
                // Another request owns the refresh lease.
            }

            RefreshDecision::Acquired(lease) => {
                if !perform_refresh(Arc::clone(&state), *lease).await? {
                    return Ok(None);
                }

                return read_session(state, handle, csrf).await;
            }
        }

        if Instant::now() >= deadline {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }

        sleep(POLL_INTERVAL).await;
    }
}
