//! Tool output history — bounded ring buffer for `/expand` replay.

const TOOL_HISTORY_CAP: usize = 20;

/// A recorded tool call output for later replay via `/expand`.
#[derive(Clone)]
pub struct ToolOutputRecord {
    pub tool_name: String,
    pub output: String,
}

/// Bounded ring buffer of recent tool outputs.
pub struct ToolOutputHistory {
    entries: Vec<ToolOutputRecord>,
}

impl ToolOutputHistory {
    pub fn new() -> Self {
        Self {
            entries: Vec::with_capacity(TOOL_HISTORY_CAP),
        }
    }

    /// Record a tool output. Drops the oldest entry if at capacity.
    pub fn push(&mut self, tool_name: &str, output: &str) {
        if self.entries.len() >= TOOL_HISTORY_CAP {
            self.entries.remove(0);
        }
        self.entries.push(ToolOutputRecord {
            tool_name: tool_name.to_string(),
            output: output.to_string(),
        });
    }

    /// Get the Nth most recent entry (1 = last, 2 = second-to-last, etc.).
    pub fn get(&self, n: usize) -> Option<&ToolOutputRecord> {
        if n == 0 || n > self.entries.len() {
            return None;
        }
        Some(&self.entries[self.entries.len() - n])
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_and_get() {
        let mut h = ToolOutputHistory::new();
        h.push("Read", "file contents");
        h.push("Bash", "hello world");

        assert_eq!(h.len(), 2);
        assert_eq!(h.get(1).unwrap().tool_name, "Bash");
        assert_eq!(h.get(1).unwrap().output, "hello world");
        assert_eq!(h.get(2).unwrap().tool_name, "Read");
        assert!(h.get(0).is_none());
        assert!(h.get(3).is_none());
    }

    #[test]
    fn test_cap() {
        let mut h = ToolOutputHistory::new();
        for i in 0..25 {
            h.push("Bash", &format!("output {i}"));
        }
        assert_eq!(h.len(), TOOL_HISTORY_CAP);
        assert_eq!(h.get(TOOL_HISTORY_CAP).unwrap().output, "output 5");
        assert_eq!(h.get(1).unwrap().output, "output 24");
    }
}
