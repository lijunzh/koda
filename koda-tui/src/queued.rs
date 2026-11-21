//! Messages queued for after the running turn, as Codex's Tab queues them.
//!
//! `Enter` during a turn steers it: the text reaches the model before its
//! next call. `Tab` queues it instead: the text waits, and goes out as the
//! next prompt once the turn completes, one message per turn, oldest first.
//! A turn stopped or failed sends nothing: the queue goes back to the input
//! box, as Codex puts queued messages back in its composer.

use crate::input::ImageData;
use std::collections::VecDeque;

/// One message waiting for the turn to end, with the pastes and images its
/// markers stand for.
#[derive(Debug, Clone, Default)]
pub(crate) struct QueuedMessage {
    /// What was typed, markers included.
    pub text: String,
    /// The pasted text behind each `[Pasted text #N]` marker in `text`.
    pub pastes: Vec<(String, String)>,
    /// The image behind each `[Image #N]` marker in `text`.
    pub images: Vec<(String, ImageData)>,
}

impl QueuedMessage {
    /// Queue `text`, moving out of `pastes` and `images` the entries whose
    /// markers it holds. The others stay: they belong to what is typed next.
    pub(crate) fn take(
        text: String,
        pastes: &mut Vec<(String, String)>,
        images: &mut Vec<(String, ImageData)>,
    ) -> Self {
        let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(pastes)
            .into_iter()
            .partition(|(marker, _)| text.contains(marker.as_str()));
        *pastes = rest;
        let (mine_images, rest_images): (Vec<_>, Vec<_>) = std::mem::take(images)
            .into_iter()
            .partition(|(marker, _)| text.contains(marker.as_str()));
        *images = rest_images;
        Self {
            text,
            pastes: mine,
            images: mine_images,
        }
    }
}

/// The messages waiting, oldest first.
#[derive(Debug, Default)]
pub(crate) struct Queue {
    messages: VecDeque<QueuedMessage>,
}

impl Queue {
    pub(crate) fn push(&mut self, message: QueuedMessage) {
        self.messages.push_back(message);
    }

    /// The next message to send.
    pub(crate) fn next(&mut self) -> Option<QueuedMessage> {
        self.messages.pop_front()
    }

    /// The message queued last, to edit it (Codex's Alt+Up).
    pub(crate) fn take_last(&mut self) -> Option<QueuedMessage> {
        self.messages.pop_back()
    }

    /// Every message, oldest first, emptying the queue.
    pub(crate) fn drain(&mut self) -> Vec<QueuedMessage> {
        self.messages.drain(..).collect()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> ImageData {
        ImageData {
            media_type: "image/png".into(),
            base64: "AA==".into(),
        }
    }

    #[test]
    fn a_queued_message_takes_only_its_own_pastes_and_images() {
        let mut pastes = vec![
            ("[Pasted text #1]".to_string(), "one".to_string()),
            ("[Pasted text #2]".to_string(), "two".to_string()),
        ];
        let mut images = vec![
            ("[Image #1]".to_string(), image()),
            ("[Image #2]".to_string(), image()),
        ];
        let queued = QueuedMessage::take(
            "see [Pasted text #2] and [Image #1]".into(),
            &mut pastes,
            &mut images,
        );
        assert_eq!(queued.pastes, [("[Pasted text #2]".into(), "two".into())]);
        assert_eq!(queued.images.len(), 1);
        assert_eq!(queued.images[0].0, "[Image #1]");
        assert_eq!(pastes, [("[Pasted text #1]".into(), "one".into())]);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].0, "[Image #2]");
    }

    #[test]
    fn messages_go_out_oldest_first_and_the_last_can_be_taken_back() {
        let mut queue = Queue::default();
        for text in ["one", "two", "three"] {
            queue.push(QueuedMessage {
                text: text.into(),
                ..Default::default()
            });
        }
        assert_eq!(queue.take_last().unwrap().text, "three");
        assert_eq!(queue.next().unwrap().text, "one");
        assert_eq!(queue.drain().len(), 1);
        assert!(queue.is_empty());
    }
}
