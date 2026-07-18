use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Delivery mode for user steering text.
pub enum SteeringMode {
    /// Send at the next tool-call boundary.
    NextToolCall,
    /// Interrupt current generation and send immediately.
    InterruptNow,
}

#[derive(Debug, Default)]
/// Queue storing steering text until it reaches a safe delivery boundary.
pub struct SteeringQueue {
    queued: Option<String>,
}

impl SteeringQueue {
    /// Queue steering for delivery at the next tool-call boundary.
    pub fn queue_for_next_tool_call(&mut self, message: impl Into<String>) {
        self.queued = Some(message.into());
    }

    /// Take queued steering for delivery at a tool-call boundary.
    pub fn take_for_tool_call(&mut self) -> Option<String> {
        self.queued.take()
    }

    /// Clear queued steering and return a message for immediate interruption.
    pub fn interrupt_and_take(&mut self, message: impl Into<String>) -> String {
        let queued = self.queued.take();
        immediate_steering_text(queued.as_deref(), &message.into())
    }

    /// Return currently queued steering text.
    pub fn queued(&self) -> Option<&str> {
        self.queued.as_deref()
    }
}

/// Append steering text to an existing queued prompt using the queue display delimiter.
pub fn append_queued_steering_text(existing: Option<&str>, text: &str) -> String {
    match existing {
        Some(existing) => format!("{existing}\n{}", text.trim()),
        None => text.trim().to_string(),
    }
}

fn immediate_steering_text(queued: Option<&str>, message: &str) -> String {
    let message = message.trim();
    match queued {
        Some(queued) if message.is_empty() => queued.to_string(),
        Some(queued) if complete_prompt_prefix(message, queued) => message.to_string(),
        Some(queued) if complete_prompt_prefix(queued, message) => queued.to_string(),
        Some(queued) => append_queued_steering_text(Some(queued), message),
        None => message.to_string(),
    }
}

fn complete_prompt_prefix(text: &str, prefix: &str) -> bool {
    text == prefix
        || text
            .strip_prefix(prefix)
            .is_some_and(|remaining| remaining.starts_with('\n'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_uses_queued_text_when_message_is_empty() {
        let mut queue = SteeringQueue::default();
        queue.queue_for_next_tool_call("first");

        assert_eq!(queue.interrupt_and_take(""), "first");
        assert_eq!(queue.queued(), None);
    }

    #[test]
    fn interrupt_does_not_duplicate_equal_queued_text() {
        let mut queue = SteeringQueue::default();
        queue.queue_for_next_tool_call("first\nsecond");

        assert_eq!(queue.interrupt_and_take("first\nsecond"), "first\nsecond");
        assert_eq!(queue.queued(), None);
    }

    #[test]
    fn interrupt_uses_longer_queued_text_when_message_is_stale() {
        let mut queue = SteeringQueue::default();
        queue.queue_for_next_tool_call("first\nsecond");

        assert_eq!(queue.interrupt_and_take("first"), "first\nsecond");
        assert_eq!(queue.queued(), None);
    }

    #[test]
    fn interrupt_uses_longer_message_when_queue_ack_is_stale() {
        let mut queue = SteeringQueue::default();
        queue.queue_for_next_tool_call("first");

        assert_eq!(queue.interrupt_and_take("first\nsecond"), "first\nsecond");
        assert_eq!(queue.queued(), None);
    }

    #[test]
    fn interrupt_appends_distinct_message_to_existing_queue() {
        let mut queue = SteeringQueue::default();
        queue.queue_for_next_tool_call("first");

        assert_eq!(queue.interrupt_and_take("second"), "first\nsecond");
        assert_eq!(queue.queued(), None);
    }
}
