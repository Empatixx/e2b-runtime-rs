use opentelemetry::KeyValue;
use opentelemetry::trace::Status;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

const EXCEPTION_EVENT: &str = "exception";
const EXCEPTION_TYPE: &str = "*fmt.wrapError";
const NIL_ERROR: &str = "%!w(<nil>)";

fn exception(message: &str, error: Option<&str>, attributes: &[KeyValue]) -> Vec<KeyValue> {
    let mut event = vec![
        KeyValue::new("exception.type", EXCEPTION_TYPE),
        KeyValue::new(
            "exception.message",
            format!("{message}: {}", error.unwrap_or(NIL_ERROR)),
        ),
    ];
    event.extend_from_slice(attributes);
    event
}

pub fn record_exception(error: &str) {
    Span::current().add_event(
        EXCEPTION_EVENT,
        vec![
            KeyValue::new("exception.type", EXCEPTION_TYPE),
            KeyValue::new("exception.message", error.to_owned()),
        ],
    );
}

pub fn report_critical_error(message: &str, error: Option<&str>, attributes: &[KeyValue]) {
    tracing::error!(error = error.unwrap_or_default(), "{message}");
    let span = Span::current();
    let mut event = exception(message, error, attributes);
    event.push(KeyValue::new("error.message", message.to_owned()));
    span.add_event(EXCEPTION_EVENT, event);
    span.set_status(Status::error(message.to_owned()));
}

pub fn report_error(message: &str, error: Option<&str>, attributes: &[KeyValue]) {
    tracing::warn!(error = error.unwrap_or_default(), "{message}");
    Span::current().add_event(EXCEPTION_EVENT, exception(message, error, attributes));
}

pub trait ReportStatus {
    fn report_critical(self, message: &str, attributes: &[KeyValue]) -> Self;
}

impl<T> ReportStatus for Result<T, tonic::Status> {
    fn report_critical(self, message: &str, attributes: &[KeyValue]) -> Self {
        self.inspect_err(|status| report_critical_error(message, Some(status.message()), attributes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exception_event_wraps_the_message_around_the_error_like_upstream() {
        let event = exception(
            "sandbox not found",
            Some("no such id"),
            &[KeyValue::new("sandbox.id", "s1")],
        );

        let message = event
            .iter()
            .find(|attribute| attribute.key.as_str() == "exception.message")
            .map(|attribute| attribute.value.as_str().into_owned());

        assert_eq!(message.as_deref(), Some("sandbox not found: no such id"));
        assert_eq!(event.len(), 3);
    }

    #[test]
    fn a_missing_error_is_rendered_like_go_wraps_nil() {
        let event = exception("sandbox not found", None, &[]);

        assert_eq!(event[1].value.as_str(), "sandbox not found: %!w(<nil>)");
    }

    #[test]
    fn a_failed_result_marks_the_current_span_as_an_error_without_changing_it() {
        let failed: Result<(), tonic::Status> = Err(tonic::Status::internal("boom"));

        let reported = failed.report_critical("failed to update sandbox", &[]);

        assert_eq!(reported.unwrap_err().message(), "boom");
    }
}
