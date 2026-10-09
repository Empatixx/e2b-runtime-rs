use opentelemetry::KeyValue;
use opentelemetry::trace::Status;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

const EXCEPTION_EVENT: &str = "exception";
const EXCEPTION_TYPE: &str = "*fmt.wrapError";

fn exception(message: &str, error: &str, attributes: &[KeyValue]) -> Vec<KeyValue> {
    let mut event = vec![
        KeyValue::new("exception.type", EXCEPTION_TYPE),
        KeyValue::new("exception.message", format!("{message}: {error}")),
    ];
    event.extend_from_slice(attributes);
    event
}

pub fn report_critical_error(message: &str, error: &str, attributes: &[KeyValue]) {
    tracing::error!(%error, "{message}");
    let span = Span::current();
    let mut event = exception(message, error, attributes);
    event.push(KeyValue::new("error.message", message.to_owned()));
    span.add_event(EXCEPTION_EVENT, event);
    span.set_status(Status::error(message.to_owned()));
}

pub fn report_error(message: &str, error: &str, attributes: &[KeyValue]) {
    tracing::warn!(%error, "{message}");
    Span::current().add_event(EXCEPTION_EVENT, exception(message, error, attributes));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exception_event_wraps_the_message_around_the_error_like_upstream() {
        let event = exception("sandbox not found", "no such id", &[KeyValue::new("sandbox.id", "s1")]);

        let message = event
            .iter()
            .find(|attribute| attribute.key.as_str() == "exception.message")
            .map(|attribute| attribute.value.as_str().into_owned());

        assert_eq!(message.as_deref(), Some("sandbox not found: no such id"));
        assert_eq!(event.len(), 3);
    }
}
