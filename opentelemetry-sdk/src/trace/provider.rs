//! # Trace Provider SDK
//!
//! ## Tracer Creation
//!
//! New [`Tracer`] instances are always created through a [`TracerProvider`].
//!
//! All configuration objects and extension points (span processors,
//! propagators) are provided by the [`TracerProvider`]. [`Tracer`] instances do
//! not duplicate this data to avoid that different [`Tracer`] instances
//! of the [`TracerProvider`] have different versions of these data.
use crate::runtime::RuntimeChannel;
use crate::trace::{
    BatchSpanProcessor, Config, RandomIdGenerator, Sampler, SimpleSpanProcessor, SpanLimits, Tracer,
};
use crate::{export::trace::SpanExporter, trace::SpanProcessor};
use crate::{InstrumentationLibrary, Resource};
use futures_util::StreamExt;
use once_cell::sync::{Lazy, OnceCell};
use opentelemetry::trace::TraceError;
use opentelemetry::{global, trace::TraceResult};
use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Default tracer name if empty string is provided.
const DEFAULT_COMPONENT_NAME: &str = "rust.opentelemetry.io/sdk/tracer";
static PROVIDER_RESOURCE: OnceCell<Resource> = OnceCell::new();

// a no nop tracer provider used as placeholder when the provider is shutdown
static NOOP_TRACER_PROVIDER: Lazy<TracerProvider> = Lazy::new(|| TracerProvider {
    inner: Arc::new(TracerProviderInner {
        processors: Vec::new(),
        config: Config {
            // cannot use default here as the default resource is not empty
            sampler: Box::new(Sampler::ParentBased(Box::new(Sampler::AlwaysOn))),
            id_generator: Box::<RandomIdGenerator>::default(),
            span_limits: SpanLimits::default(),
            resource: Cow::Owned(Resource::empty()),
        },
    }),
    is_shutdown: Arc::new(AtomicBool::new(true)),
});

/// TracerProvider inner type
#[derive(Debug)]
pub(crate) struct TracerProviderInner {
    processors: Vec<Box<dyn SpanProcessor>>,
    config: crate::trace::Config,
}

impl Drop for TracerProviderInner {
    fn drop(&mut self) {
        let processors = std::mem::take(&mut self.processors);
        crate::util::spawn_future(async move {
            for processor in processors {
                if let Err(err) = processor.shutdown().await {
                    global::handle_error(err);
                }
            }
        })
    }
}

/// Creator and registry of named [`Tracer`] instances.
///
/// `TracerProvider` is lightweight container holding pointers to `SpanProcessor` and other components.
/// Cloning and dropping them will not stop the span processing. To stop span processing, users
/// must either call `shutdown` method explicitly, or drop every clone of `TracerProvider`.
#[derive(Clone, Debug)]
pub struct TracerProvider {
    inner: Arc<TracerProviderInner>,
    is_shutdown: Arc<AtomicBool>,
}

impl Default for TracerProvider {
    fn default() -> Self {
        TracerProvider::builder().build()
    }
}

impl TracerProvider {
    /// Build a new tracer provider
    pub(crate) fn new(inner: TracerProviderInner) -> Self {
        TracerProvider {
            inner: Arc::new(inner),
            is_shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Create a new [`TracerProvider`] builder.
    pub fn builder() -> Builder {
        Builder::default()
    }

    /// Span processors associated with this provider
    pub(crate) fn span_processors(&self) -> &[Box<dyn SpanProcessor>] {
        &self.inner.processors
    }

    /// Config associated with this tracer
    pub(crate) fn config(&self) -> &crate::trace::Config {
        &self.inner.config
    }

    /// true if the provider has been shutdown
    /// Don't start span or export spans when provider is shutdown
    pub(crate) fn is_shutdown(&self) -> bool {
        self.is_shutdown.load(Ordering::Relaxed)
    }

    /// Force flush all remaining spans in span processors and return results.
    ///
    /// # Examples
    ///
    /// ```
    /// use opentelemetry::global;
    /// use opentelemetry_sdk::trace::TracerProvider;
    ///
    /// fn init_tracing() -> TracerProvider {
    ///     let provider = TracerProvider::default();
    ///
    ///     // Set provider to be used as global tracer provider
    ///     let _ = global::set_tracer_provider(provider.clone());
    ///
    ///     provider
    /// }
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let provider = init_tracing();
    ///
    ///     // create spans..
    ///
    ///     // force all spans to flush
    ///     for result in provider.force_flush().await {
    ///         if let Err(err) = result {
    ///             // .. handle flush error
    ///         }
    ///     }
    ///
    ///     // create more spans..
    ///
    ///     // dropping provider and shutting down global provider ensure all
    ///     // remaining spans are exported
    ///     drop(provider);
    ///     global::shutdown_tracer_provider();
    /// }
    /// ```
    pub async fn force_flush(&self) -> Vec<TraceResult<()>> {
        futures_util::stream::iter(self.span_processors())
            .then(|processor| processor.force_flush())
            .collect()
            .await
    }

    /// Shuts down the current `TracerProvider`.
    ///
    /// Note that shut down doesn't means the TracerProvider has dropped
    pub async fn shutdown(&self) -> TraceResult<()> {
        if self
            .is_shutdown
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            // propagate the shutdown signal to processors
            // it's up to the processor to properly block new spans after shutdown
            let mut errs = vec![];
            for processor in &self.inner.processors {
                if let Err(err) = processor.shutdown().await {
                    errs.push(err);
                }
            }

            if errs.is_empty() {
                Ok(())
            } else {
                Err(TraceError::Other(format!("{errs:?}").into()))
            }
        } else {
            Err(TraceError::Other(
                "tracer provider already shut down".into(),
            ))
        }
    }
}

impl opentelemetry::trace::TracerProvider for TracerProvider {
    /// This implementation of `TracerProvider` produces `Tracer` instances.
    type Tracer = Tracer;

    /// Create a new versioned `Tracer` instance.
    fn versioned_tracer(
        &self,
        name: impl Into<Cow<'static, str>>,
        version: Option<impl Into<Cow<'static, str>>>,
        schema_url: Option<impl Into<Cow<'static, str>>>,
        attributes: Option<Vec<opentelemetry::KeyValue>>,
    ) -> Self::Tracer {
        // Use default value if name is invalid empty string
        let name = name.into();
        let component_name = if name.is_empty() {
            Cow::Borrowed(DEFAULT_COMPONENT_NAME)
        } else {
            name
        };

        let mut builder = self.tracer_builder(component_name);

        if let Some(v) = version {
            builder = builder.with_version(v);
        }
        if let Some(s) = schema_url {
            builder = builder.with_schema_url(s);
        }
        if let Some(a) = attributes {
            builder = builder.with_attributes(a);
        }

        builder.build()
    }

    fn library_tracer(&self, library: Arc<InstrumentationLibrary>) -> Self::Tracer {
        if self.is_shutdown.load(Ordering::Relaxed) {
            return Tracer::new(library, NOOP_TRACER_PROVIDER.clone());
        }
        Tracer::new(library, self.clone())
    }
}

/// Builder for provider attributes.
#[derive(Debug, Default)]
pub struct Builder {
    processors: Vec<Box<dyn SpanProcessor>>,
    config: crate::trace::Config,
}

impl Builder {
    /// The `SpanExporter` that this provider should use.
    pub fn with_simple_exporter<T: SpanExporter + 'static>(self, exporter: T) -> Self {
        let mut processors = self.processors;
        processors.push(Box::new(SimpleSpanProcessor::new(Box::new(exporter))));

        Builder { processors, ..self }
    }

    /// The [`SpanExporter`] setup using a default [`BatchSpanProcessor`] that this provider should use.
    pub fn with_batch_exporter<T: SpanExporter + 'static, R: RuntimeChannel>(
        self,
        exporter: T,
        runtime: R,
    ) -> Self {
        let batch = BatchSpanProcessor::builder(exporter, runtime).build();
        self.with_span_processor(batch)
    }

    /// The [`SpanProcessor`] that this provider should use.
    pub fn with_span_processor<T: SpanProcessor + 'static>(self, processor: T) -> Self {
        let mut processors = self.processors;
        processors.push(Box::new(processor));

        Builder { processors, ..self }
    }

    /// The sdk [`crate::trace::Config`] that this provider will use.
    pub fn with_config(self, config: crate::trace::Config) -> Self {
        Builder { config, ..self }
    }

    /// Create a new provider from this configuration.
    pub fn build(self) -> TracerProvider {
        let mut config = self.config;

        // Standard config will contain an owned [`Resource`] (either sdk default or use supplied)
        // we can optimize the common case with a static ref to avoid cloning the underlying
        // resource data for each span.
        //
        // For the uncommon case where there are multiple tracer providers with different resource
        // configurations, users can optionally provide their own borrowed static resource.
        if matches!(config.resource, Cow::Owned(_)) {
            config.resource = match PROVIDER_RESOURCE.try_insert(config.resource.into_owned()) {
                Ok(static_resource) => Cow::Borrowed(static_resource),
                Err((prev, new)) => {
                    if prev == &new {
                        Cow::Borrowed(prev)
                    } else {
                        Cow::Owned(new)
                    }
                }
            }
        }

        // Create a new vector to hold the modified processors
        let mut processors = self.processors;

        // Set the resource for each processor
        for p in &mut processors {
            p.set_resource(config.resource.as_ref());
        }

        TracerProvider::new(TracerProviderInner { processors, config })
    }
}

#[cfg(test)]
mod tests {
    use crate::export::trace::SpanData;
    use crate::resource::{
        SERVICE_NAME, TELEMETRY_SDK_LANGUAGE, TELEMETRY_SDK_NAME, TELEMETRY_SDK_VERSION,
    };
    use crate::trace::provider::TracerProviderInner;
    use crate::trace::{Config, Span, SpanProcessor};
    use crate::Resource;
    use opentelemetry::trace::{TraceError, TraceResult, Tracer, TracerProvider};
    use opentelemetry::{Context, Key, KeyValue, Value};
    use std::borrow::Cow;
    use std::env;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;

    // fields below is wrapped with Arc so we can assert it
    #[derive(Default, Debug)]
    struct AssertInfo {
        started_span: AtomicU32,
        is_shutdown: AtomicBool,
    }

    #[derive(Default, Debug, Clone)]
    struct SharedAssertInfo(Arc<AssertInfo>);

    impl SharedAssertInfo {
        fn started_span_count(&self, count: u32) -> bool {
            self.0.started_span.load(Ordering::SeqCst) == count
        }
    }

    #[derive(Debug)]
    struct TestSpanProcessor {
        success: bool,
        assert_info: SharedAssertInfo,
    }

    impl TestSpanProcessor {
        fn new(success: bool) -> TestSpanProcessor {
            TestSpanProcessor {
                success,
                assert_info: SharedAssertInfo::default(),
            }
        }

        // get handle to assert info
        fn assert_info(&self) -> SharedAssertInfo {
            self.assert_info.clone()
        }
    }

    #[async_trait::async_trait]
    impl SpanProcessor for TestSpanProcessor {
        fn on_start(&self, _span: &mut Span, _cx: &Context) {
            self.assert_info
                .0
                .started_span
                .fetch_add(1, Ordering::SeqCst);
        }

        async fn on_end(&self, _span: SpanData) {
            // ignore
        }

        async fn force_flush(&self) -> TraceResult<()> {
            if self.success {
                Ok(())
            } else {
                Err(TraceError::from("cannot export"))
            }
        }

        async fn shutdown(&self) -> TraceResult<()> {
            if self.assert_info.0.is_shutdown.load(Ordering::SeqCst) {
                Ok(())
            } else {
                let _ = self.assert_info.0.is_shutdown.compare_exchange(
                    false,
                    true,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
                self.force_flush().await
            }
        }
    }

    #[tokio::test]
    async fn test_force_flush() {
        let tracer_provider = super::TracerProvider::new(TracerProviderInner {
            processors: vec![
                Box::from(TestSpanProcessor::new(true)),
                Box::from(TestSpanProcessor::new(false)),
            ],
            config: Default::default(),
        });

        let results = tracer_provider.force_flush().await;
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_tracer_provider_default_resource() {
        let assert_resource = |provider: &super::TracerProvider,
                               resource_key: &'static str,
                               expect: Option<&'static str>| {
            assert_eq!(
                provider
                    .config()
                    .resource
                    .get(Key::from_static_str(resource_key))
                    .map(|v| v.to_string()),
                expect.map(|s| s.to_string())
            );
        };
        let assert_telemetry_resource = |provider: &super::TracerProvider| {
            assert_eq!(
                provider
                    .config()
                    .resource
                    .get(TELEMETRY_SDK_LANGUAGE.into()),
                Some(Value::from("rust"))
            );
            assert_eq!(
                provider.config().resource.get(TELEMETRY_SDK_NAME.into()),
                Some(Value::from("opentelemetry"))
            );
            assert_eq!(
                provider.config().resource.get(TELEMETRY_SDK_VERSION.into()),
                Some(Value::from(env!("CARGO_PKG_VERSION")))
            );
        };

        // If users didn't provide a resource and there isn't a env var set. Use default one.
        temp_env::with_var_unset("OTEL_RESOURCE_ATTRIBUTES", || {
            let default_config_provider = super::TracerProvider::builder().build();
            assert_resource(
                &default_config_provider,
                SERVICE_NAME,
                Some("unknown_service"),
            );
            assert_telemetry_resource(&default_config_provider);
        });

        // If user provided a resource, use that.
        let custom_config_provider = super::TracerProvider::builder()
            .with_config(Config {
                resource: Cow::Owned(Resource::new(vec![KeyValue::new(
                    SERVICE_NAME,
                    "test_service",
                )])),
                ..Default::default()
            })
            .build();
        assert_resource(&custom_config_provider, SERVICE_NAME, Some("test_service"));
        assert_eq!(custom_config_provider.config().resource.len(), 1);

        // If `OTEL_RESOURCE_ATTRIBUTES` is set, read them automatically
        temp_env::with_var(
            "OTEL_RESOURCE_ATTRIBUTES",
            Some("key1=value1, k2, k3=value2"),
            || {
                let env_resource_provider = super::TracerProvider::builder().build();
                assert_resource(
                    &env_resource_provider,
                    SERVICE_NAME,
                    Some("unknown_service"),
                );
                assert_resource(&env_resource_provider, "key1", Some("value1"));
                assert_resource(&env_resource_provider, "k3", Some("value2"));
                assert_telemetry_resource(&env_resource_provider);
                assert_eq!(env_resource_provider.config().resource.len(), 6);
            },
        );

        // When `OTEL_RESOURCE_ATTRIBUTES` is set and also user provided config
        temp_env::with_var(
            "OTEL_RESOURCE_ATTRIBUTES",
            Some("my-custom-key=env-val,k2=value2"),
            || {
                let user_provided_resource_config_provider = super::TracerProvider::builder()
                    .with_config(Config {
                        resource: Cow::Owned(Resource::default().merge(&mut Resource::new(vec![
                            KeyValue::new("my-custom-key", "my-custom-value"),
                            KeyValue::new("my-custom-key2", "my-custom-value2"),
                        ]))),
                        ..Default::default()
                    })
                    .build();
                assert_resource(
                    &user_provided_resource_config_provider,
                    SERVICE_NAME,
                    Some("unknown_service"),
                );
                assert_resource(
                    &user_provided_resource_config_provider,
                    "my-custom-key",
                    Some("my-custom-value"),
                );
                assert_resource(
                    &user_provided_resource_config_provider,
                    "my-custom-key2",
                    Some("my-custom-value2"),
                );
                assert_resource(
                    &user_provided_resource_config_provider,
                    "k2",
                    Some("value2"),
                );
                assert_telemetry_resource(&user_provided_resource_config_provider);
                assert_eq!(
                    user_provided_resource_config_provider
                        .config()
                        .resource
                        .len(),
                    7
                );
            },
        );

        // If user provided a resource, it takes priority during collision.
        let no_service_name = super::TracerProvider::builder()
            .with_config(Config {
                resource: Cow::Owned(Resource::empty()),
                ..Default::default()
            })
            .build();

        assert_eq!(no_service_name.config().resource.len(), 0)
    }

    #[tokio::test]
    async fn test_shutdown_noops() {
        let processor = TestSpanProcessor::new(false);
        let assert_handle = processor.assert_info();
        let tracer_provider = super::TracerProvider::new(TracerProviderInner {
            processors: vec![Box::from(processor)],
            config: Default::default(),
        });

        let test_tracer_1 = tracer_provider.tracer("test1");
        let _ = test_tracer_1.start("test");

        assert!(assert_handle.started_span_count(1));

        let _ = test_tracer_1.start("test");

        assert!(assert_handle.started_span_count(2));

        let shutdown = |tracer_provider: super::TracerProvider| async move {
            let _ = tracer_provider.shutdown().await; // shutdown once
        };

        // assert tracer provider can be shutdown using on a cloned version
        shutdown(tracer_provider.clone()).await;

        // after shutdown we should get noop tracer
        let noop_tracer = tracer_provider.tracer("noop");
        // noop tracer cannot start anything
        let _ = noop_tracer.start("test");
        assert!(assert_handle.started_span_count(2));
        // noop tracer's tracer provider should be shutdown
        assert!(noop_tracer.provider().is_shutdown.load(Ordering::SeqCst));

        // existing tracer becomes noops after shutdown
        let _ = test_tracer_1.start("test");
        assert!(assert_handle.started_span_count(2));
    }
}
