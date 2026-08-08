use std::{
    collections::HashSet,
    error::Error,
    fmt,
    sync::{Arc, Mutex},
};

use prometheus_client::{
    encoding::text::encode,
    metrics::{MetricType, family::MetricConstructor, histogram::Histogram},
    registry::{Metric, Registry, Unit},
};

/// The only classic histogram buckets used by shared InferLab metrics.
pub const FIXED_HISTOGRAM_BUCKETS: [f64; 14] = [
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

/// Construct a histogram with InferLab's fixed, reviewable bucket set.
#[must_use]
pub fn fixed_histogram() -> Histogram {
    Histogram::new(FIXED_HISTOGRAM_BUCKETS)
}

/// A reusable constructor for fixed-bucket histogram families.
#[derive(Clone, Copy, Debug, Default)]
pub struct FixedHistogramConstructor;

impl MetricConstructor<Histogram> for FixedHistogramConstructor {
    fn new_metric(&self) -> Histogram {
        fixed_histogram()
    }
}

/// A registry that is mutable only while metrics and one scrape hook are wired.
///
/// Clone each Prometheus metric before registering it and retain the clone in
/// service state. Once registration is complete, put this value in an `Arc`;
/// rendering then needs only `&self`.
pub struct MetricsRegistry {
    registry: Registry,
    registered_names: HashSet<String>,
    registered_sample_names: HashSet<String>,
    before_render: Option<Arc<dyn Fn() + Send + Sync + 'static>>,
    render_lock: Mutex<()>,
}

impl fmt::Debug for MetricsRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MetricsRegistry")
            .field("registry", &self.registry)
            .field("registered_names", &self.registered_names)
            .field("registered_sample_names", &self.registered_sample_names)
            .field("has_before_render", &self.before_render.is_some())
            .field("render_lock", &self.render_lock)
            .finish()
    }
}

impl Default for MetricsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            registry: Registry::default(),
            registered_names: HashSet::new(),
            registered_sample_names: HashSet::new(),
            before_render: None,
            render_lock: Mutex::new(()),
        }
    }

    /// Register a metric under its OpenMetrics base name.
    ///
    /// `prometheus-client` adds type suffixes during encoding. For example,
    /// register a counter named `requests`, which is rendered as
    /// `requests_total`; do not register it as `requests_total`.
    pub fn register(
        &mut self,
        name: impl Into<String>,
        help: impl Into<String>,
        metric: impl Metric,
    ) -> Result<(), RegistryError> {
        let name = name.into();
        self.reserve_metric_name(&name, metric.metric_type())?;
        self.registry.register(name, help, metric);
        Ok(())
    }

    /// Register a metric with exact OpenMetrics unit metadata.
    ///
    /// The supplied name must omit the unit suffix. Registering
    /// `request_duration` with [`Unit::Seconds`], for example, renders
    /// `request_duration_seconds` and a matching `# UNIT` declaration.
    pub fn register_with_unit(
        &mut self,
        name: impl Into<String>,
        help: impl Into<String>,
        unit: Unit,
        metric: impl Metric,
    ) -> Result<(), RegistryError> {
        let name = name.into();
        validate_metric_name(&name)?;
        validate_metric_name(unit.as_str())?;
        let encoded_name = format!("{name}_{}", unit.as_str());
        self.reserve_metric_name(&encoded_name, metric.metric_type())?;
        self.registry.register_with_unit(name, help, unit, metric);
        Ok(())
    }

    fn reserve_metric_name(
        &mut self,
        name: &str,
        metric_type: MetricType,
    ) -> Result<(), RegistryError> {
        validate_metric_name(name)?;
        if !self.registered_names.insert(name.to_owned()) {
            return Err(RegistryError::DuplicateName(name.to_owned()));
        }
        let sample_names = sample_names(name, metric_type);
        if let Some(collision) = sample_names
            .iter()
            .find(|sample| self.registered_sample_names.contains(*sample))
        {
            self.registered_names.remove(name);
            return Err(RegistryError::SampleNameCollision(collision.clone()));
        }
        self.registered_sample_names.extend(sample_names);
        Ok(())
    }

    /// Install the service's single fast scrape-time snapshot refresh.
    ///
    /// The callback must not hold application locks while setting registered
    /// gauges, and must not perform I/O or consensus work.
    pub fn set_before_render(
        &mut self,
        callback: impl Fn() + Send + Sync + 'static,
    ) -> Result<(), RegistryError> {
        if self.before_render.is_some() {
            return Err(RegistryError::BeforeRenderAlreadySet);
        }
        self.before_render = Some(Arc::new(callback));
        Ok(())
    }

    /// Refresh snapshots and encode a complete OpenMetrics text document.
    pub fn render(&self) -> Result<String, fmt::Error> {
        // A single boundary must cover both the scalar snapshot refresh and
        // encoding. Otherwise a second scrape can refresh one-hot gauges while
        // the first scrape is walking the registry and produce a mixed epoch.
        let _render_guard = self
            .render_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(callback) = &self.before_render {
            callback();
        }
        let mut output = String::new();
        encode(&mut output, &self.registry)?;
        Ok(output)
    }
}

/// Errors detected while constructing a bounded metrics registry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistryError {
    InvalidName(String),
    DuplicateName(String),
    SampleNameCollision(String),
    BeforeRenderAlreadySet,
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(name) => write!(formatter, "invalid OpenMetrics name `{name}`"),
            Self::DuplicateName(name) => write!(formatter, "duplicate metric name `{name}`"),
            Self::SampleNameCollision(name) => {
                write!(
                    formatter,
                    "metric sample name `{name}` is already registered"
                )
            }
            Self::BeforeRenderAlreadySet => {
                formatter.write_str("before-render callback is already configured")
            }
        }
    }
}

fn sample_names(name: &str, metric_type: MetricType) -> Vec<String> {
    match metric_type {
        MetricType::Counter => vec![format!("{name}_total")],
        MetricType::Gauge | MetricType::Unknown => vec![name.to_owned()],
        MetricType::Histogram => ["bucket", "sum", "count"]
            .map(|suffix| format!("{name}_{suffix}"))
            .into(),
        MetricType::Info => vec![format!("{name}_info")],
    }
}

impl Error for RegistryError {}

fn validate_metric_name(name: &str) -> Result<(), RegistryError> {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return Err(RegistryError::InvalidName(name.to_owned()));
    };
    if !(first.is_ascii_alphabetic() || first == b'_' || first == b':')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':'))
    {
        return Err(RegistryError::InvalidName(name.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    };
    use std::{thread, time::Duration};

    use prometheus_client::metrics::{counter::Counter, gauge::Gauge, histogram::Histogram};

    use super::*;

    #[test]
    fn fixed_histogram_has_exact_buckets() {
        let mut registry = MetricsRegistry::new();
        let histogram = fixed_histogram();
        registry
            .register(
                "request_duration_seconds",
                "Request duration",
                histogram.clone(),
            )
            .unwrap();
        histogram.observe(0.003);

        let output = registry.render().unwrap();
        for bucket in [
            "0.001", "0.0025", "0.005", "0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1.0",
            "2.5", "5.0", "10.0", "30.0",
        ] {
            assert!(output.contains(&format!("le=\"{bucket}\"")), "{bucket}");
        }
        assert_eq!(
            output.matches("request_duration_seconds_bucket").count(),
            15
        );
    }

    #[test]
    fn render_runs_the_snapshot_hook_once_per_scrape() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = MetricsRegistry::new();
        let callback_calls = Arc::clone(&calls);
        registry
            .set_before_render(move || {
                callback_calls.fetch_add(1, Ordering::Relaxed);
            })
            .unwrap();

        registry.render().unwrap();
        registry.render().unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn concurrent_scrapes_encode_one_complete_snapshot_epoch() {
        const SCRAPES: usize = 16;
        let left = Gauge::<i64>::default();
        let right = Gauge::<i64>::default();
        let next_epoch = Arc::new(AtomicUsize::new(0));
        let active_callbacks = Arc::new(AtomicUsize::new(0));
        let maximum_active_callbacks = Arc::new(AtomicUsize::new(0));

        let mut registry = MetricsRegistry::new();
        registry
            .register("snapshot_left", "Left half of one snapshot", left.clone())
            .unwrap();
        registry
            .register(
                "snapshot_right",
                "Right half of one snapshot",
                right.clone(),
            )
            .unwrap();
        let callback_epoch = Arc::clone(&next_epoch);
        let callback_active = Arc::clone(&active_callbacks);
        let callback_maximum = Arc::clone(&maximum_active_callbacks);
        registry
            .set_before_render(move || {
                let active = callback_active.fetch_add(1, Ordering::SeqCst) + 1;
                callback_maximum.fetch_max(active, Ordering::SeqCst);
                let epoch = callback_epoch.fetch_add(1, Ordering::SeqCst) + 1;
                left.set(i64::try_from(epoch).unwrap());
                thread::sleep(Duration::from_millis(2));
                right.set(i64::try_from(epoch).unwrap());
                callback_active.fetch_sub(1, Ordering::SeqCst);
            })
            .unwrap();
        let registry = Arc::new(registry);
        let start = Arc::new(Barrier::new(SCRAPES));

        let outputs = thread::scope(|scope| {
            let handles = (0..SCRAPES)
                .map(|_| {
                    let registry = Arc::clone(&registry);
                    let start = Arc::clone(&start);
                    scope.spawn(move || {
                        start.wait();
                        registry.render().unwrap()
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });

        assert_eq!(maximum_active_callbacks.load(Ordering::SeqCst), 1);
        for output in outputs {
            let value = |name: &str| {
                output
                    .lines()
                    .find_map(|line| line.strip_prefix(&format!("{name} ")))
                    .and_then(|value| value.parse::<i64>().ok())
                    .unwrap()
            };
            assert_eq!(value("snapshot_left"), value("snapshot_right"));
        }
    }

    #[test]
    fn rejects_duplicate_and_invalid_metric_names() {
        let mut registry = MetricsRegistry::new();
        registry
            .register("valid_name", "Valid", Counter::<u64>::default())
            .unwrap();
        assert_eq!(
            registry.register("valid_name", "Duplicate", Counter::<u64>::default()),
            Err(RegistryError::DuplicateName("valid_name".to_owned()))
        );
        assert_eq!(
            registry.register("raw/path", "Invalid", Counter::<u64>::default()),
            Err(RegistryError::InvalidName("raw/path".to_owned()))
        );
        assert_eq!(
            registry.register(
                "valid_name_total",
                "Conflicting gauge sample",
                prometheus_client::metrics::gauge::Gauge::<i64>::default(),
            ),
            Err(RegistryError::SampleNameCollision(
                "valid_name_total".to_owned()
            ))
        );
        assert_eq!(registry.set_before_render(|| {}), Ok(()));
        assert_eq!(
            registry.set_before_render(|| {}),
            Err(RegistryError::BeforeRenderAlreadySet)
        );

        let _: Histogram = fixed_histogram();
    }

    #[test]
    fn explicit_units_render_metadata_and_reserve_the_encoded_name() {
        let mut registry = MetricsRegistry::new();
        registry
            .register_with_unit(
                "request_duration",
                "Request duration",
                Unit::Seconds,
                Gauge::<i64>::default(),
            )
            .unwrap();
        assert_eq!(
            registry.register(
                "request_duration_seconds",
                "Conflicting encoded name",
                Gauge::<i64>::default(),
            ),
            Err(RegistryError::DuplicateName(
                "request_duration_seconds".to_owned()
            ))
        );

        let output = registry.render().unwrap();
        assert!(output.contains("# UNIT request_duration_seconds seconds"));
        assert!(output.contains("request_duration_seconds 0"));
    }
}
