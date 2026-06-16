//! Cached ADC bearer token bridged to genai's AuthResolver.

use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Cached OAuth2 bearer token with expiry.
#[derive(Debug, Clone)]
struct CachedToken {
    token: String,
    expires_at: Instant,
}

/// Async token fetcher: yields a `(bearer token, ttl)` pair or an error string.
type TokenFetcher = dyn Fn() -> futures::future::BoxFuture<'static, Result<(String, Duration), String>>
    + Send
    + Sync;

/// Token cache with async refresh.
pub struct VertexAuth {
    cached: Mutex<Option<CachedToken>>,
    fetcher: Box<TokenFetcher>,
    refresh_margin: Duration,
}

impl std::fmt::Debug for VertexAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VertexAuth")
            .field("refresh_margin", &self.refresh_margin)
            .finish_non_exhaustive()
    }
}

impl VertexAuth {
    /// Construct with a custom token fetcher (for tests).
    pub fn with_fetcher(
        fetcher: impl Fn() -> futures::future::BoxFuture<'static, Result<(String, Duration), String>>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self {
            cached: Mutex::new(None),
            fetcher: Box::new(fetcher),
            refresh_margin: Duration::from_secs(60),
        }
    }

    /// Construct using `google-cloud-auth` ADC for token fetching (production path).
    ///
    /// The credential is built **once** and reused: `google-cloud-auth` caches the
    /// bearer and refreshes it internally, so `access_token()` returns a
    /// currently-valid token on every call. We therefore delegate freshness to it
    /// and keep only a short synapse-side TTL.
    ///
    /// The previous implementation pinned a fixed 50-minute lifetime on whatever
    /// token it fetched. The GKE metadata server serves the *same* token until
    /// minutes before its ~1-hour expiry, so a token fetched late in its life was
    /// cached for another 50 minutes and went stale — surfacing as Vertex
    /// `401 ACCESS_TOKEN_EXPIRED` for a ~40-minute window every refresh cycle.
    pub fn from_adc() -> Self {
        use google_cloud_auth::credentials::{AccessTokenCredentials, Builder};
        use std::sync::Arc;
        use tokio::sync::OnceCell;

        let creds: Arc<OnceCell<AccessTokenCredentials>> = Arc::new(OnceCell::new());
        Self::with_fetcher(move || {
            let creds = creds.clone();
            Box::pin(async move {
                let creds = creds
                    .get_or_try_init(|| async {
                        Builder::default()
                            .with_scopes(["https://www.googleapis.com/auth/cloud-platform"])
                            .build_access_token_credentials()
                            .map_err(|e| e.to_string())
                    })
                    .await?;
                let token = creds.access_token().await.map_err(|e| e.to_string())?;
                // Short TTL: the library owns refresh; this just throttles how often
                // we re-call the (cheap, in-memory) cached `access_token()`.
                Ok((token.token, Duration::from_secs(120)))
            })
        })
    }

    /// Returns a cached token if still fresh; otherwise fetches a new one.
    pub async fn token(&self) -> Result<String, String> {
        {
            let guard = self.cached.lock();
            if let Some(c) = &*guard {
                if c.expires_at > Instant::now() + self.refresh_margin {
                    return Ok(c.token.clone());
                }
            }
        }
        let (token, ttl) = (self.fetcher)().await?;
        let expires_at = Instant::now() + ttl;
        let mut guard = self.cached.lock();
        *guard = Some(CachedToken {
            token: token.clone(),
            expires_at,
        });
        Ok(token)
    }

    /// Wrap as a genai `AuthResolver` that returns the cached bearer per request.
    pub fn into_auth_resolver(self: std::sync::Arc<Self>) -> genai::resolver::AuthResolver {
        let auth = self;
        genai::resolver::AuthResolver::from_resolver_async_fn(
            move |_model_iden: genai::ModelIden| {
                let auth = auth.clone();
                Box::pin(async move {
                    let token = auth
                        .token()
                        .await
                        .map_err(|e| genai::resolver::Error::Custom(format!("vertex adc: {e}")))?;
                    Ok(Some(genai::resolver::AuthData::from_single(token)))
                })
                    as std::pin::Pin<
                        Box<
                            dyn std::future::Future<
                                    Output = genai::resolver::Result<
                                        Option<genai::resolver::AuthData>,
                                    >,
                                > + Send,
                        >,
                    >
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    fn fetcher_with_counter(ttl: Duration) -> (Arc<AtomicU32>, VertexAuth) {
        let counter = Arc::new(AtomicU32::new(0));
        let c2 = counter.clone();
        let auth = VertexAuth::with_fetcher(move || {
            let c = c2.clone();
            Box::pin(async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
                Ok((format!("tok-{n}"), ttl))
            })
        });
        (counter, auth)
    }

    #[tokio::test]
    async fn fetches_once_then_uses_cache() {
        let (counter, auth) = fetcher_with_counter(Duration::from_secs(3600));
        let t1 = auth.token().await.unwrap();
        let t2 = auth.token().await.unwrap();
        assert_eq!(t1, t2);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refreshes_when_near_expiry() {
        // TTL shorter than refresh_margin → always refreshes.
        let (counter, auth) = fetcher_with_counter(Duration::from_secs(30));
        let _ = auth.token().await.unwrap();
        let _ = auth.token().await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn fetcher_error_propagates() {
        let auth = VertexAuth::with_fetcher(|| Box::pin(async { Err("nope".to_string()) }));
        let err = auth.token().await.unwrap_err();
        assert_eq!(err, "nope");
    }

    #[tokio::test]
    async fn into_auth_resolver_constructs_without_panicking() {
        let auth = std::sync::Arc::new(VertexAuth::with_fetcher(|| {
            Box::pin(async { Ok(("test-tok".into(), Duration::from_secs(60))) })
        }));
        let _resolver = auth.clone().into_auth_resolver();
        // Smoke test only — we can't drive genai's internal resolver without a full Client.
    }
}
