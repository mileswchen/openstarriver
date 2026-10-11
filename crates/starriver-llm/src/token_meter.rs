//! Token usage aggregation (token-meter parity, N5).
//!
//! Model-call usage is evented as `llm/usage-recorded` by the kernel dispatch
//! path (one event per `Usage` chunk); this module aggregates those records
//! in memory by session, agent, and tumbling time window, and exposes total /
//! windowed queries. Persistence is an interface reservation only: durable
//! storage belongs to the session store and lands in a later slice.

use std::sync::{Arc, Mutex};

use cordis_rslite::{EffectHandle, EventBus};
use starriver_kernel::llm::{LlmUsageRecord, TokenUsage};

/// Cumulative token usage plus call count for one aggregate view.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageTotals {
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
}

impl UsageTotals {
    fn absorb(&mut self, usage: &TokenUsage) {
        self.calls += 1;
        self.input_tokens += usage.input_tokens;
        self.output_tokens += usage.output_tokens;
        self.cache_read_tokens += usage.cache_read_tokens.unwrap_or(0);
        self.cache_write_tokens += usage.cache_write_tokens.unwrap_or(0);
        self.reasoning_tokens += usage.reasoning_tokens.unwrap_or(0);
    }
}

/// Tumbling window granularity for time-bucketed queries. Windows are aligned
/// to the Unix epoch, so buckets are timezone-free and stable across restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowGranularity {
    Minute,
    Hour,
    Day,
}

impl WindowGranularity {
    pub fn duration_ms(&self) -> u64 {
        match self {
            WindowGranularity::Minute => 60_000,
            WindowGranularity::Hour => 3_600_000,
            WindowGranularity::Day => 86_400_000,
        }
    }

    fn bucket_start(&self, at_ms: u64) -> u64 {
        at_ms - (at_ms % self.duration_ms())
    }
}

/// Persistence reservation: a sink receives every recorded usage sample.
/// The durable implementation (session store) lands in a later slice; the
/// in-memory meter is the only runtime consumer today.
pub trait UsageSink: Send + Sync {
    fn persist(&self, record: &LlmUsageRecord);
}

struct MeterState {
    records: Vec<LlmUsageRecord>,
    sink: Option<Arc<dyn UsageSink>>,
}

/// Thread-safe token meter aggregating `llm/usage-recorded` events by
/// session, agent, and time window (in memory).
pub struct TokenMeter {
    state: Mutex<MeterState>,
}

impl TokenMeter {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(MeterState {
                records: Vec::new(),
                sink: None,
            }),
        }
    }

    /// Create a meter subscribed to `llm/usage-recorded` on `bus`. The
    /// returned handle keeps the subscription alive; dropping it detaches.
    pub fn attach(bus: &EventBus) -> (Arc<Self>, EffectHandle) {
        let meter = Arc::new(Self::new());
        let listener = meter.clone();
        let handle = bus.on_sync::<starriver_kernel::llm::LlmUsageRecorded, _>(move |event| {
            listener.record(&event.record);
            Ok(cordis_rslite::Value::new(()))
        });
        (meter, handle)
    }

    /// Record one usage sample (and forward it to the sink when attached).
    pub fn record(&self, record: &LlmUsageRecord) {
        let sink = {
            let mut state = self.state.lock().expect("token meter poisoned");
            state.records.push(record.clone());
            state.sink.clone()
        };
        if let Some(sink) = sink {
            sink.persist(record);
        }
    }

    /// Attach the persistence sink (interface reservation; see [`UsageSink`]).
    pub fn set_sink(&self, sink: Option<Arc<dyn UsageSink>>) {
        self.state.lock().expect("token meter poisoned").sink = sink;
    }

    /// Grand totals across every recorded sample.
    pub fn total(&self) -> UsageTotals {
        self.total_where(|_| true)
    }

    /// Totals for one session (zero when unknown).
    pub fn session_total(&self, session_id: &str) -> UsageTotals {
        self.total_where(|record| record.session_id.as_deref() == Some(session_id))
    }

    /// Totals for one agent (zero when unknown).
    pub fn agent_total(&self, agent_id: &str) -> UsageTotals {
        self.total_where(|record| record.agent_id.as_deref() == Some(agent_id))
    }

    /// Totals for samples captured in `[from_ms, to_ms)` (epoch ms).
    pub fn range_total(&self, from_ms: u64, to_ms: u64) -> UsageTotals {
        self.total_where(|record| record.at_ms >= from_ms && record.at_ms < to_ms)
    }

    /// Per-window totals at `granularity`, ascending by window start; only
    /// windows with at least one sample are listed.
    pub fn windows(&self, granularity: WindowGranularity) -> Vec<(u64, UsageTotals)> {
        let records = &self.state.lock().expect("token meter poisoned").records;
        let mut buckets: Vec<(u64, UsageTotals)> = Vec::new();
        for record in records {
            let start = granularity.bucket_start(record.at_ms);
            match buckets.last_mut() {
                Some((bucket_start, totals)) if *bucket_start == start => {
                    totals.absorb(&record.usage);
                }
                _ => {
                    // Records are appended in capture order, but clock skew
                    // can move a sample back one bucket; keep buckets sorted.
                    match buckets.binary_search_by_key(&start, |(s, _)| *s) {
                        Ok(index) => buckets[index].1.absorb(&record.usage),
                        Err(index) => {
                            let mut totals = UsageTotals::default();
                            totals.absorb(&record.usage);
                            buckets.insert(index, (start, totals));
                        }
                    }
                }
            }
        }
        buckets
    }

    fn total_where(&self, keep: impl Fn(&LlmUsageRecord) -> bool) -> UsageTotals {
        let mut totals = UsageTotals::default();
        for record in &self.state.lock().expect("token meter poisoned").records {
            if keep(record) {
                totals.absorb(&record.usage);
            }
        }
        totals
    }
}

impl Default for TokenMeter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::llm::CallPurpose;

    fn usage(input: u64, output: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: Some(1),
            cache_write_tokens: None,
            reasoning_tokens: Some(2),
        }
    }

    fn record(
        session: Option<&str>,
        agent: Option<&str>,
        at_ms: u64,
        usage: TokenUsage,
    ) -> LlmUsageRecord {
        LlmUsageRecord {
            session_id: session.map(String::from),
            agent_id: agent.map(String::from),
            provider: "anthropic".into(),
            model: "claude".into(),
            purpose: Some(CallPurpose::SessionTitle),
            usage,
            at_ms,
        }
    }

    #[test]
    fn aggregates_totals_by_session_and_agent() {
        let meter = TokenMeter::new();
        meter.record(&record(Some("s1"), Some("a1"), 1_000, usage(10, 5)));
        meter.record(&record(Some("s1"), Some("a2"), 2_000, usage(20, 6)));
        meter.record(&record(Some("s2"), Some("a1"), 3_000, usage(30, 7)));

        let total = meter.total();
        assert_eq!(total.calls, 3);
        assert_eq!(total.input_tokens, 60);
        assert_eq!(total.output_tokens, 18);
        assert_eq!(total.cache_read_tokens, 3);
        assert_eq!(total.reasoning_tokens, 6);

        let s1 = meter.session_total("s1");
        assert_eq!((s1.calls, s1.input_tokens), (2, 30));
        let a1 = meter.agent_total("a1");
        assert_eq!((a1.calls, a1.input_tokens), (2, 40));
        assert_eq!(meter.session_total("missing"), UsageTotals::default());
    }

    #[test]
    fn windows_bucket_and_range_queries() {
        let meter = TokenMeter::new();
        // Three minutes, one sample each; all within one hour/day.
        let minute = WindowGranularity::Minute;
        meter.record(&record(Some("s"), None, 0, usage(1, 1)));
        meter.record(&record(Some("s"), None, minute.duration_ms(), usage(2, 1)));
        meter.record(&record(
            Some("s"),
            None,
            minute.duration_ms() * 2 + 5_000,
            usage(3, 1),
        ));

        let windows = meter.windows(WindowGranularity::Minute);
        assert_eq!(
            windows.iter().map(|(start, _)| *start).collect::<Vec<_>>(),
            vec![0, 60_000, 120_000]
        );
        assert_eq!(windows[0].1.input_tokens, 1);
        assert_eq!(windows[2].1.input_tokens, 3);
        assert_eq!(windows[2].1.calls, 1);

        let hour = meter.windows(WindowGranularity::Hour);
        assert_eq!(hour.len(), 1);
        assert_eq!(hour[0].1.input_tokens, 6);

        let range = meter.range_total(60_000, 180_000);
        assert_eq!((range.calls, range.input_tokens), (2, 5));
    }

    #[test]
    fn sink_receives_every_record() {
        struct Captured(Mutex<Vec<u64>>);
        impl UsageSink for Captured {
            fn persist(&self, record: &LlmUsageRecord) {
                self.0.lock().unwrap().push(record.at_ms);
            }
        }
        let meter = TokenMeter::new();
        let sink = Arc::new(Captured(Mutex::new(Vec::new())));
        meter.set_sink(Some(sink.clone()));
        meter.record(&record(Some("s"), None, 42, usage(1, 1)));
        meter.record(&record(Some("s"), None, 43, usage(1, 1)));
        assert_eq!(*sink.0.lock().unwrap(), vec![42, 43]);
        meter.set_sink(None);
    }

    #[tokio::test]
    async fn meter_aggregates_synthetic_event_stream() {
        // A synthetic adapter stream emitting two Usage chunks through the
        // kernel dispatch path must feed the attached meter end to end.
        use futures::StreamExt;
        use starriver_kernel::llm::{
            dispatch_llm_stream, GenerateOptions, LlmAdapter, LlmError, StreamChunk,
        };

        struct Streaming;
        impl LlmAdapter for Streaming {
            fn providers(&self) -> &[String] {
                static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
                PROVIDERS.get_or_init(|| vec!["metered".into()])
            }

            fn stream(
                &self,
                _options: GenerateOptions,
            ) -> futures::stream::BoxStream<'static, Result<StreamChunk, LlmError>> {
                Box::pin(async_stream::stream! {
                    yield Ok(StreamChunk::Usage { usage: usage(100, 40) });
                    yield Ok(StreamChunk::Finish {
                        reason: starriver_kernel::llm::FinishReason::Stop,
                        replay_state: None,
                    });
                })
            }
        }

        let bus = EventBus::new();
        let (meter, _listener) = TokenMeter::attach(&bus);
        let options = GenerateOptions {
            provider: "metered".into(),
            model: "m".into(),
            reasoning_effort: None,
            messages: Vec::new(),
            system: None,
            tools: None,
            temperature: None,
            max_tokens: None,
            stop: None,
            session_id: Some("s9".into()),
            agent_id: Some("a9".into()),
            tool_choice: None,
            cancellation: None,
            purpose: None,
            loop_assembled: false,
        };
        let _ = dispatch_llm_stream(&bus, options, Arc::new(Streaming))
            .collect::<Vec<_>>()
            .await;
        // Events dispatch asynchronously; give the sync listener a beat.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let totals = meter.session_total("s9");
        assert_eq!(
            (totals.calls, totals.input_tokens, totals.output_tokens),
            (1, 100, 40)
        );
        assert!(meter.total().calls == 1);
        assert!(starriver_kernel::llm::now_epoch_ms() > 0);
    }
}
