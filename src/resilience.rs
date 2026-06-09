//! Retries + circuit breakers for synapse outbound HTTP clients.
//!
//! Mirrors talos-core `src/resilience.rs` (profiles, breakers, `is_retryable` rules).

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use backon::{ExponentialBuilder, Retryable};
use parking_lot::Mutex;

#[derive(Debug, Clone, Copy)]
pub enum Profile {
    Default,
    Fast,
    Aggressive,
}

#[derive(Debug, Clone)]
pub struct ResiliencePolicy {
    pub max_attempts: u32,
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub multiplier: f32,
    pub jitter: bool,
    pub breaker_threshold: u32,
    pub breaker_open_for: Duration,
}

impl Profile {
    pub fn policy(self) -> ResiliencePolicy {
        match self {
            Profile::Default => ResiliencePolicy {
                max_attempts: 3,
                initial_delay: Duration::from_millis(200),
                max_delay: Duration::from_secs(5),
                multiplier: 2.0,
                jitter: true,
                breaker_threshold: 5,
                breaker_open_for: Duration::from_secs(30),
            },
            Profile::Fast => ResiliencePolicy {
                max_attempts: 1,
                initial_delay: Duration::from_millis(0),
                max_delay: Duration::from_millis(0),
                multiplier: 1.0,
                jitter: false,
                breaker_threshold: 10,
                breaker_open_for: Duration::from_secs(15),
            },
            Profile::Aggressive => ResiliencePolicy {
                max_attempts: 5,
                initial_delay: Duration::from_millis(100),
                max_delay: Duration::from_secs(10),
                multiplier: 2.0,
                jitter: true,
                breaker_threshold: 10,
                breaker_open_for: Duration::from_secs(60),
            },
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResilienceError<E = reqwest::Error> {
    #[error("upstream call failed after retries: {0}")]
    Exhausted(E),
    #[error("circuit breaker {name} is open")]
    CircuitOpen { name: String },
}

impl From<reqwest::Error> for ResilienceError {
    fn from(e: reqwest::Error) -> Self {
        ResilienceError::Exhausted(e)
    }
}

pub async fn run<F, Fut, T>(
    op: F,
    profile: Profile,
    breaker: &CircuitBreaker,
    label: &'static str,
) -> Result<T, ResilienceError>
where
    F: FnMut() -> Fut + Send,
    Fut: std::future::Future<Output = Result<T, reqwest::Error>> + Send,
{
    run_with_classifier(op, profile, breaker, label, is_retryable_reqwest).await
}

pub async fn run_with_classifier<T, E, F, Fut, Cls>(
    op: F,
    profile: Profile,
    breaker: &CircuitBreaker,
    label: &'static str,
    is_retryable: Cls,
) -> Result<T, ResilienceError<E>>
where
    F: FnMut() -> Fut + Send,
    Fut: std::future::Future<Output = Result<T, E>> + Send,
    E: std::fmt::Debug + Send,
    Cls: Fn(&E) -> bool + Send + Sync,
{
    let start = Instant::now();

    if breaker.guard().is_err() {
        record_call_metrics(label, "circuit_open", start.elapsed());
        return Err(ResilienceError::CircuitOpen {
            name: breaker.name.to_string(),
        });
    }

    let policy = profile.policy();
    let mut builder = ExponentialBuilder::default()
        .with_max_times(policy.max_attempts.saturating_sub(1) as usize)
        .with_min_delay(policy.initial_delay)
        .with_max_delay(policy.max_delay)
        .with_factor(policy.multiplier);
    if policy.jitter {
        builder = builder.with_jitter();
    }

    let mut attempt: u32 = 0;
    let result = op
        .retry(builder)
        .when(&is_retryable)
        .notify(|err, dur| {
            attempt += 1;
            metrics::counter!(
                "synapse_resilience_retry_attempts_total",
                "label" => label,
            )
            .increment(1);
            tracing::warn!(
                label,
                attempt,
                next_delay_ms = dur.as_millis() as u64,
                error = ?err,
                "retrying outbound call",
            );
        })
        .await;

    breaker.record(&result);

    let outcome = if result.is_ok() {
        "success"
    } else {
        "exhausted"
    };
    record_call_metrics(label, outcome, start.elapsed());

    result.map_err(ResilienceError::Exhausted)
}

fn record_call_metrics(label: &'static str, outcome: &'static str, elapsed: Duration) {
    metrics::counter!(
        "synapse_resilience_calls_total",
        "label" => label,
        "outcome" => outcome,
    )
    .increment(1);
    metrics::histogram!(
        "synapse_resilience_call_duration_seconds",
        "label" => label,
        "outcome" => outcome,
    )
    .record(elapsed.as_secs_f64());
}

pub fn is_retryable_reqwest(e: &reqwest::Error) -> bool {
    if e.is_timeout() || e.is_connect() {
        return true;
    }
    if let Some(status) = e.status() {
        return status.is_server_error()
            || status == reqwest::StatusCode::REQUEST_TIMEOUT
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
    }
    false
}

pub struct CircuitBreaker {
    pub(crate) name: &'static str,
    pub(crate) threshold: u32,
    pub(crate) open_for: Duration,
    pub(crate) consecutive_failures: AtomicU64,
    pub(crate) state: AtomicU8,
    pub(crate) opened_at: Mutex<Option<Instant>>,
}

pub(crate) const STATE_CLOSED: u8 = 0;
pub(crate) const STATE_OPEN: u8 = 1;
pub(crate) const STATE_HALF_OPEN: u8 = 2;

impl CircuitBreaker {
    pub fn new(name: &'static str, profile: Profile) -> Self {
        let p = profile.policy();
        Self {
            name,
            threshold: p.breaker_threshold,
            open_for: p.breaker_open_for,
            consecutive_failures: AtomicU64::new(0),
            state: AtomicU8::new(STATE_CLOSED),
            opened_at: Mutex::new(None),
        }
    }

    pub fn guard(&self) -> Result<(), ResilienceError> {
        match self.state.load(Ordering::Acquire) {
            STATE_CLOSED | STATE_HALF_OPEN => Ok(()),
            STATE_OPEN => {
                let opened_at = self.opened_at.lock();
                let opened = opened_at.unwrap_or_else(Instant::now);
                drop(opened_at);
                if Instant::now().duration_since(opened) >= self.open_for {
                    let prev = self.state.swap(STATE_HALF_OPEN, Ordering::AcqRel);
                    tracing::info!(name = self.name, "circuit breaker half-open");
                    record_breaker_transition(self.name, "half_open", prev, STATE_HALF_OPEN);
                    Ok(())
                } else {
                    Err(ResilienceError::CircuitOpen {
                        name: self.name.to_string(),
                    })
                }
            }
            _ => Ok(()),
        }
    }

    pub fn record<T, E>(&self, result: &Result<T, E>) {
        match result {
            Ok(_) => {
                self.consecutive_failures.store(0, Ordering::Release);
                let prev = self.state.swap(STATE_CLOSED, Ordering::AcqRel);
                if prev == STATE_HALF_OPEN {
                    tracing::info!(name = self.name, "circuit breaker closed");
                    record_breaker_transition(self.name, "closed", prev, STATE_CLOSED);
                }
            }
            Err(_) => {
                let n = self.consecutive_failures.fetch_add(1, Ordering::AcqRel) + 1;
                if n >= self.threshold as u64 {
                    let prev = self.state.swap(STATE_OPEN, Ordering::AcqRel);
                    *self.opened_at.lock() = Some(Instant::now());
                    if prev != STATE_OPEN {
                        tracing::warn!(
                            name = self.name,
                            consecutive_failures = n,
                            "circuit breaker opened",
                        );
                        record_breaker_transition(self.name, "open", prev, STATE_OPEN);
                    }
                }
            }
        }
    }
}

fn record_breaker_transition(name: &'static str, transition: &'static str, prev: u8, new: u8) {
    metrics::counter!(
        "synapse_resilience_breaker_transitions_total",
        "name" => name,
        "transition" => transition,
    )
    .increment(1);
    metrics::gauge!("synapse_resilience_breaker_state", "name" => name).set(new as f64);
    let _ = prev;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    async fn fetch(url: &str) -> reqwest::Result<reqwest::Response> {
        reqwest::Client::new()
            .get(url)
            .timeout(Duration::from_millis(50))
            .send()
            .await
    }

    #[tokio::test]
    async fn is_retryable_timeout_is_true() {
        let err = fetch("http://10.255.255.1:1/").await.unwrap_err();
        assert!(
            is_retryable_reqwest(&err),
            "expected timeout/connect to be retryable, got {err:?}"
        );
    }

    #[tokio::test]
    async fn is_retryable_5xx_is_true() {
        use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let resp = reqwest::Client::new()
            .get(server.uri())
            .send()
            .await
            .unwrap();
        let err = resp.error_for_status().unwrap_err();
        assert!(is_retryable_reqwest(&err));
    }

    #[tokio::test]
    async fn run_retries_until_success() {
        use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let breaker = CircuitBreaker::new("test", Profile::Default);
        let attempts = AtomicU32::new(0);
        let result = run(
            || async {
                attempts.fetch_add(1, Ordering::SeqCst);
                reqwest::Client::new()
                    .get(server.uri())
                    .send()
                    .await?
                    .error_for_status()
            },
            Profile::Default,
            &breaker,
            "test",
        )
        .await;
        assert!(result.is_ok(), "got {result:?}");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn breaker_opens_after_threshold() {
        let b = CircuitBreaker::new("t", Profile::Default);
        for _ in 0..5 {
            let err: Result<(), reqwest::Error> = Err(make_fake_reqwest_error());
            b.record(&err);
        }
        assert!(matches!(
            b.guard(),
            Err(ResilienceError::CircuitOpen { .. })
        ));
    }

    fn make_fake_reqwest_error() -> reqwest::Error {
        reqwest::Client::new()
            .get("http://invalid url")
            .build()
            .unwrap_err()
    }

    #[tokio::test]
    async fn run_with_classifier_retries_on_classified_retryable() {
        #[derive(Debug)]
        struct MyErr(bool /* retryable */);

        let breaker = CircuitBreaker::new("test-classifier", Profile::Fast);
        let attempts = std::sync::atomic::AtomicU32::new(0);

        let result: Result<u32, ResilienceError<MyErr>> = run_with_classifier(
            || {
                let n = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if n < 2 {
                        Err(MyErr(true))
                    } else {
                        Ok(42u32)
                    }
                }
            },
            Profile::Default,
            &breaker,
            "test-classifier",
            |e: &MyErr| e.0,
        )
        .await;

        assert_eq!(result.unwrap(), 42);
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn run_with_classifier_fails_fast_on_non_retryable() {
        #[derive(Debug)]
        struct MyErr(bool);

        let breaker = CircuitBreaker::new("test-fail-fast", Profile::Fast);
        let attempts = std::sync::atomic::AtomicU32::new(0);

        let result: Result<u32, ResilienceError<MyErr>> = run_with_classifier(
            || {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move { Err(MyErr(false)) }
            },
            Profile::Default,
            &breaker,
            "test-fail-fast",
            |e: &MyErr| e.0,
        )
        .await;

        assert!(matches!(
            result,
            Err(ResilienceError::Exhausted(MyErr(false)))
        ));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
