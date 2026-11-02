//! Up and Down in the composer: walking back through what you sent in a
//! conversation, the way a shell walks its history. Each conversation has
//! its own list, so a line recalled in one chat is never one written for
//! another.

use std::collections::HashMap;

#[derive(Default)]
pub struct Recall {
    /// Oldest first, by conversation address.
    lines: HashMap<String, Vec<String>>,
    walk: Option<Walk>,
}

struct Walk {
    /// The line in the composer.
    at: usize,
    /// What was there before the walk began, given back at the bottom.
    draft: String,
}

impl Recall {
    pub fn has(&self, address: &str) -> bool {
        self.lines.contains_key(address)
    }

    /// Fills a conversation's list, oldest first.
    pub fn load(&mut self, address: &str, lines: impl IntoIterator<Item = String>) {
        self.lines.entry(address.to_string()).or_default();
        for line in lines {
            self.push(address, line);
        }
    }

    /// Adds a line just sent, unless it repeats the one before, and ends
    /// any walk.
    pub fn push(&mut self, address: &str, line: String) {
        self.walk = None;
        let lines = self.lines.entry(address.to_string()).or_default();
        if lines.last() != Some(&line) {
            lines.push(line);
        }
    }

    pub fn forget(&mut self, address: &str) {
        self.lines.remove(address);
        self.walk = None;
    }

    pub fn stop(&mut self) {
        self.walk = None;
    }

    /// The line before the one in the composer, or the newest when `current`
    /// is not a recalled line. `None` at the oldest, or with nothing sent.
    pub fn older(&mut self, address: &str, current: &str) -> Option<String> {
        let lines = self.lines.get(address)?;
        // Editing a recalled line makes it a draft of its own.
        let walking = self.walk.as_mut().filter(|w| lines.get(w.at).is_some_and(|l| l == current));
        let line = match walking {
            Some(walk) => {
                walk.at = walk.at.checked_sub(1)?;
                lines[walk.at].clone()
            }
            None => {
                let at = lines.len().checked_sub(1)?;
                self.walk = Some(Walk { at, draft: current.to_string() });
                lines[at].clone()
            }
        };
        Some(line)
    }

    /// The line after the one in the composer, then the draft the walk began
    /// with. `None` when not walking.
    pub fn newer(&mut self, address: &str, current: &str) -> Option<String> {
        let lines = self.lines.get(address)?;
        let walk = self.walk.as_mut().filter(|w| lines.get(w.at).is_some_and(|l| l == current))?;
        if walk.at + 1 < lines.len() {
            walk.at += 1;
            return Some(lines[walk.at].clone());
        }
        self.walk.take().map(|w| w.draft)
    }
}

#[cfg(test)]
mod tests {
    use super::Recall;

    fn recall() -> Recall {
        let mut r = Recall::default();
        r.load("bob", ["one", "two", "two", "three"].map(String::from));
        r
    }

    #[test]
    fn walks_back_and_returns_the_draft() {
        let mut r = recall();
        assert_eq!(r.older("bob", "draft").as_deref(), Some("three"));
        assert_eq!(r.older("bob", "three").as_deref(), Some("two"));
        assert_eq!(r.older("bob", "two").as_deref(), Some("one"), "repeats collapse");
        assert_eq!(r.older("bob", "one"), None, "stops at the oldest");
        assert_eq!(r.newer("bob", "one").as_deref(), Some("two"));
        assert_eq!(r.newer("bob", "two").as_deref(), Some("three"));
        assert_eq!(r.newer("bob", "three").as_deref(), Some("draft"));
        assert_eq!(r.newer("bob", "draft"), None, "the walk is over");
    }

    #[test]
    fn an_edited_line_starts_a_new_walk() {
        let mut r = recall();
        r.older("bob", "");
        assert_eq!(r.newer("bob", "three!"), None);
        assert_eq!(r.older("bob", "three!").as_deref(), Some("three"));
        assert_eq!(r.newer("bob", "three").as_deref(), Some("three!"));
    }

    #[test]
    fn conversations_keep_their_own_lines() {
        let mut r = recall();
        assert!(r.has("bob") && !r.has("alice"));
        assert_eq!(r.older("alice", ""), None);
        r.forget("bob");
        assert_eq!(r.older("bob", ""), None);
    }
}
