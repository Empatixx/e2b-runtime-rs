use parking_lot::Mutex;
use tokio::sync::mpsc::{Receiver, Sender, channel};

use crate::proto::process::process_event::DataEvent;

const SUBSCRIBER_BUFFER: usize = 64;

pub struct Output {
    subscribers: Mutex<Option<Vec<Sender<DataEvent>>>>,
}

impl Output {
    pub fn new() -> Self {
        Self {
            subscribers: Mutex::new(Some(Vec::new())),
        }
    }

    pub fn subscribe(&self) -> Receiver<DataEvent> {
        let (sender, receiver) = channel(SUBSCRIBER_BUFFER);
        if let Some(subscribers) = self.subscribers.lock().as_mut() {
            subscribers.push(sender);
        }
        receiver
    }

    pub fn has_subscribers(&self) -> bool {
        self.subscribers
            .lock()
            .as_ref()
            .is_some_and(|subscribers| subscribers.iter().any(|sender| !sender.is_closed()))
    }

    pub async fn publish(&self, event: DataEvent) {
        let subscribers: Vec<Sender<DataEvent>> = self.subscribers.lock().iter().flatten().cloned().collect();
        for subscriber in subscribers {
            let _ = subscriber.send(event.clone()).await;
        }
        if let Some(subscribers) = self.subscribers.lock().as_mut() {
            subscribers.retain(|sender| !sender.is_closed());
        }
    }

    pub fn close(&self) {
        self.subscribers.lock().take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::process::__buffa::oneof::process_event::data_event::Output as Data;

    fn chunk(byte: u8) -> DataEvent {
        DataEvent {
            output: Some(Data::Stdout(vec![byte])),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn slow_subscriber_gets_everything_and_dropped_one_does_not_block() {
        let output = std::sync::Arc::new(Output::new());
        let mut slow = output.subscribe();
        drop(output.subscribe());

        let producer = tokio::spawn({
            let output = output.clone();
            async move {
                for byte in 0..100 {
                    output.publish(chunk(byte)).await;
                }
                output.close();
            }
        });

        let mut received = Vec::new();
        while let Some(event) = slow.recv().await {
            received.push(event);
            tokio::task::yield_now().await;
        }
        producer.await.unwrap();
        assert_eq!(received, (0..100).map(chunk).collect::<Vec<_>>());
    }

    #[test]
    fn subscribers_after_close_get_a_finished_stream() {
        let output = Output::new();
        assert!(!output.has_subscribers());
        let _live = output.subscribe();
        assert!(output.has_subscribers());
        output.close();
        assert!(!output.has_subscribers());
        assert!(output.subscribe().try_recv().is_err());
    }
}
