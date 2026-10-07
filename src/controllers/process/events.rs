use std::sync::Arc;
use std::time::Duration;

use connectrpc::{ConnectError, ErrorCode, ServiceStream};
use tokio::sync::mpsc::{Receiver, channel};
use tokio::time::{Instant, MissedTickBehavior, interval_at};
use tokio_stream::wrappers::ReceiverStream;

use super::spawn::Process;
use crate::proto::process::__buffa::oneof::process_event::Event;
use crate::proto::process::ProcessEvent;
use crate::proto::process::process_event::{DataEvent, EndEvent, KeepAlive, StartEvent};

pub fn process_event(event: impl Into<Option<Event>>) -> ProcessEvent {
    ProcessEvent {
        event: event.into(),
        ..Default::default()
    }
}

fn start_event(pid: u32) -> ProcessEvent {
    process_event(StartEvent {
        pid,
        ..Default::default()
    })
}

pub fn live_events<R: Send + 'static>(
    process: Arc<Process>,
    mut data: Receiver<DataEvent>,
    keepalive_interval: Duration,
    wrap: fn(ProcessEvent) -> R,
) -> ServiceStream<R> {
    let (responses, stream) = channel(16);
    tokio::spawn(async move {
        if responses.send(Ok(wrap(start_event(process.pid)))).await.is_err() {
            return;
        }

        let mut keepalive = interval_at(Instant::now() + keepalive_interval, keepalive_interval);
        keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            let event = tokio::select! {
                _ = responses.closed() => return,
                _ = keepalive.tick() => process_event(KeepAlive::default()),
                data = data.recv() => match data {
                    Some(data) => {
                        keepalive.reset();
                        process_event(data)
                    }
                    None => break,
                },
            };
            if responses.send(Ok(wrap(event))).await.is_err() {
                return;
            }
        }

        let mut end = process.end.subscribe();
        let response = tokio::select! {
            _ = responses.closed() => return,
            end = end.wait_for(Option::is_some) => end
                .map(|end| wrap(process_event(end.clone().unwrap_or_default())))
                .map_err(|_| ConnectError::new(ErrorCode::Unknown, "end event channel closed before sending end event")),
        };
        let _ = responses.send(response).await;
    });
    Box::pin(ReceiverStream::new(stream))
}

pub fn exited_events<R: Send + 'static>(pid: u32, end: EndEvent, wrap: fn(ProcessEvent) -> R) -> ServiceStream<R> {
    let events = [start_event(pid), process_event(end)].map(|event| Ok(wrap(event)));
    Box::pin(tokio_stream::iter(events))
}
