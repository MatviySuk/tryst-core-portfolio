use std::fmt;
use std::sync::Arc;
use std::time::Instant;
use thiserror::Error;
use tracing::{Level, Subscriber, field::Visit, span};
use tracing_subscriber::{
    EnvFilter, Layer,
    layer::{Context, SubscriberExt},
    registry::LookupSpan,
    util::SubscriberInitExt,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[uniffi::export(with_foreign)]
pub trait SwiftLogger: Send + Sync + std::fmt::Debug {
    fn log(&self, level: LogLevel, target: String, message: String);
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogLevel::Error => write!(f, "error"),
            LogLevel::Warn => write!(f, "warn"),
            LogLevel::Info => write!(f, "info"),
            LogLevel::Debug => write!(f, "debug"),
            LogLevel::Trace => write!(f, "trace"),
        }
    }
}

impl From<&Level> for LogLevel {
    fn from(level: &Level) -> Self {
        match *level {
            Level::ERROR => LogLevel::Error,
            Level::WARN => LogLevel::Warn,
            Level::INFO => LogLevel::Info,
            Level::DEBUG => LogLevel::Debug,
            Level::TRACE => LogLevel::Trace,
        }
    }
}

#[derive(Debug)]
struct FfiTracingLayer {
    swift_logger_proxy: Arc<dyn SwiftLogger>,
}

#[derive(Debug)]
struct SpanData {
    start: Instant,
    fields: String,
}

#[derive(Default)]
struct FfiFieldVisitor {
    fields: String,
    message: Option<String>,
}

impl FfiFieldVisitor {
    fn finish(self) -> String {
        self.fields
    }

    fn maybe_add_separator(&mut self) {
        if !self.fields.is_empty() {
            self.fields.push_str(", ");
        }
    }
}

impl Visit for FfiFieldVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
            return;
        }
        self.maybe_add_separator();
        self.fields
            .push_str(&format!("{} = \"{}\"", field.name(), value));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.maybe_add_separator();
        self.fields
            .push_str(&format!("{} = {}", field.name(), value));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.maybe_add_separator();
        self.fields
            .push_str(&format!("{} = {}", field.name(), value));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.maybe_add_separator();
        self.fields
            .push_str(&format!("{} = {}", field.name(), value));
    }

    fn record_debug(
        &mut self,
        field: &tracing::field::Field,
        value: &dyn std::fmt::Debug,
    ) {
        if field.name() == "message" {
            self.message = Some(format!("{:?}", value));
            return;
        }
        self.maybe_add_separator();
        self.fields
            .push_str(&format!("{} = {:?}", field.name(), value));
    }
}

impl<S> Layer<S> for FfiTracingLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &span::Attributes<'_>,
        id: &span::Id,
        ctx: Context<'_, S>,
    ) {
        let span = ctx.span(id).expect("Span not found, this is a bug");
        let metadata = span.metadata();

        let mut visitor = FfiFieldVisitor::default();
        attrs.record(&mut visitor);
        let fields = visitor.finish();

        let message = format!("start: {} {{ {} }}", metadata.name(), fields);
        self.swift_logger_proxy.log(
            LogLevel::from(metadata.level()),
            metadata.target().to_string(),
            message,
        );

        let span_data = SpanData {
            start: Instant::now(),
            fields,
        };
        span.extensions_mut().insert(span_data);
    }

    fn on_record(
        &self,
        id: &span::Id,
        values: &span::Record<'_>,
        ctx: Context<'_, S>,
    ) {
        let span = ctx.span(id).expect("Span not found, this is a bug");
        let mut extensions = span.extensions_mut();

        if let Some(span_data) = extensions.get_mut::<SpanData>() {
            let mut visitor = FfiFieldVisitor::default();
            values.record(&mut visitor);
            if !visitor.fields.is_empty() {
                if !span_data.fields.is_empty() {
                    span_data.fields.push_str(", ");
                }
                span_data.fields.push_str(&visitor.finish());
            }
        }
    }

    fn on_enter(&self, _id: &span::Id, _ctx: Context<'_, S>) {}

    fn on_exit(&self, _id: &span::Id, _ctx: Context<'_, S>) {}

    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = FfiFieldVisitor::default();
        event.record(&mut visitor);

        let primary_message = visitor
            .message
            .clone()
            .unwrap_or_else(|| metadata.name().to_string());
        let fields = visitor.finish();

        let message = if fields.is_empty() {
            primary_message.to_string()
        } else {
            format!("{} {{ {} }}", primary_message, fields)
        };

        self.swift_logger_proxy.log(
            LogLevel::from(metadata.level()),
            metadata.target().to_string(),
            message,
        );
    }

    fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(&id) {
            let metadata = span.metadata();
            if let Some(data) = span.extensions().get::<SpanData>() {
                let duration = data.start.elapsed();
                // Changed message to "end:" for consistency with "start:"
                let message = format!(
                    "end: {} (duration: {:?}) {{ {} }}",
                    metadata.name(),
                    duration,
                    data.fields
                );
                self.swift_logger_proxy.log(
                    LogLevel::from(metadata.level()),
                    metadata.target().to_string(),
                    message,
                );
            }
        }
    }
}

#[derive(Debug, Error, uniffi::Error)]
pub enum LoggerInitError {
    #[error("Tracing subscriber already initialized")]
    SetLoggerError,
}

#[uniffi::export]
pub fn init_logger(
    logger: Arc<dyn SwiftLogger>,
    general_level: LogLevel,
    gossip_core_level: LogLevel,
) -> Result<(), LoggerInitError> {
    let ffi_layer = FfiTracingLayer {
        swift_logger_proxy: logger,
    };

    let filter_str = format!("{},gossip_core={}", general_level, gossip_core_level);
    let filter =
        EnvFilter::try_new(filter_str).unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(ffi_layer)
        .try_init()
        .map_err(|e| {
            eprintln!("Failed to initialize tracing subscriber: {}", e);
            LoggerInitError::SetLoggerError
        })?;

    Ok(())
}
