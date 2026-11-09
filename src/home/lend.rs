//! Borrowing a model from someone in the group.
//!
//! The machine with the devices needn't run a model. When it has none
//! ready, it sends each model turn (what was said, and the tools it
//! offers) to a group member whose model is, and runs the tools that come
//! back itself: the borrowed model only thinks, it never touches the
//! devices. The same goes for `@ai` on a host whose model is down.
//!
//! This travels as hidden one-to-one messages, end-to-end encrypted like
//! any other and never shown in a chat. A machine whose model is ready
//! says so to each peer it has a session with (`have`), and takes it back
//! (`gone`) when it stops being ready or stops sharing. A lender only
//! answers someone who is in the group the question came from, with
//! itself in it too.

use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ollama::Ollama;
use crate::engine::{Command, Input};

/// What a hidden message starts with; nobody types this.
pub const MARK: &str = "\u{1}hopchat-model\u{1}";
/// How long to wait for a lender: its own turn may take two minutes.
const WAIT: Duration = Duration::from_secs(150);
/// A plain answer is written out whole, and the lender allows it five minutes.
const WAIT_ANSWER: Duration = Duration::from_secs(330);
/// Lenders to try before giving up on a turn.
const TRIES: usize = 3;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Wire {
    /// My model is ready and I'll lend it.
    Have { model: String },
    /// Not any more.
    Gone,
    /// Please think about this; `tools` is null for a plain answer.
    Ask { id: String, group: String, messages: Vec<Value>, tools: Value },
    /// The assistant message the model wrote.
    Turn { id: String, message: Value },
    Fail { id: String, error: String },
}

impl Wire {
    pub fn encode(&self) -> String {
        format!("{MARK}{}", serde_json::to_string(self).unwrap_or_default())
    }

    /// `Some` for any hidden message, even one too garbled to read, so it
    /// is never shown as chat.
    pub fn decode(text: &str) -> Option<Option<Wire>> {
        let body = text.strip_prefix(MARK)?;
        Some(serde_json::from_str(body).ok())
    }
}

/// What the engine says to a request to borrow.
pub enum Borrowed {
    Answer { by: String, message: Value },
    /// That lender couldn't; another may.
    Failed { lender: String, error: String },
    /// Nobody in the group has offered a model.
    Nobody,
}

/// Where a model turn gets computed: here when this machine's model is
/// ready, otherwise on a group member's.
pub struct Mind {
    pub local: Ollama,
    pub local_ready: bool,
    /// The pooled model this machine hosts, once it's loaded (see `pool`).
    pub pool: Option<String>,
    pub engine: Sender<Input>,
    pub group: String,
}

/// A model's answer, and whose model it was when it wasn't ours.
pub struct Thought {
    pub message: Value,
    pub by: Option<String>,
}

impl Mind {
    pub fn think(&self, messages: &[Value], tools: Option<&Value>) -> Result<Thought, String> {
        let mut pool_error = None;
        if let Some(url) = self.pool.as_ref().filter(|_| super::pool::takes(tools)) {
            match super::pool::ask(url, messages) {
                Ok(message) => return Ok(Thought { message, by: None }),
                Err(e) => pool_error = Some(e),
            }
        }
        self.think_alone(messages, tools).map_err(|e| match pool_error {
            Some(p) => format!("{p}; {e}"),
            None => e,
        })
    }

    fn think_alone(&self, messages: &[Value], tools: Option<&Value>) -> Result<Thought, String> {
        let local_error = if self.local_ready {
            match self.local.raw(messages, tools) {
                Ok(message) => return Ok(Thought { message, by: None }),
                Err(e) => Some(e),
            }
        } else {
            None
        };
        let mut skip = Vec::new();
        let mut lender_error = None;
        for _ in 0..TRIES {
            match self.borrow(messages, tools, &skip) {
                Borrowed::Answer { message, by, .. } => return Ok(Thought { message, by: Some(by) }),
                Borrowed::Failed { lender, error } => {
                    skip.push(lender);
                    lender_error = Some(error);
                }
                Borrowed::Nobody => break,
            }
        }
        // Not known to be ready may still mean it started since we looked.
        let local_error = match local_error {
            Some(e) => e,
            None => match self.local.raw(messages, tools) {
                Ok(message) => return Ok(Thought { message, by: None }),
                Err(e) => e,
            },
        };
        Err(match lender_error {
            Some(e) => format!("{local_error}; borrowing a model failed too: {e}"),
            None => format!("{local_error}, and nobody else in the group shares a model"),
        })
    }

    fn borrow(&self, messages: &[Value], tools: Option<&Value>, skip: &[String]) -> Borrowed {
        let (reply, answer) = mpsc::channel();
        let asked = self.engine.send(Input::Ui(Command::Borrow {
            group: self.group.clone(),
            messages: messages.to_vec(),
            tools: tools.cloned().unwrap_or(Value::Null),
            skip: skip.to_vec(),
            reply,
        }));
        if asked.is_err() {
            return Borrowed::Nobody;
        }
        // The lender gives an empty tool list the plain answer's patience too.
        let with_tools = tools.and_then(Value::as_array).is_some_and(|t| !t.is_empty());
        let wait = if with_tools { WAIT } else { WAIT_ANSWER };
        answer.recv_timeout(wait).unwrap_or(Borrowed::Nobody)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wire_round_trips_and_hides_garble() {
        let ask = Wire::Ask {
            id: "1".into(),
            group: "g".into(),
            messages: vec![json!({"role": "user", "content": "hi"})],
            tools: Value::Null,
        };
        assert_eq!(Wire::decode(&ask.encode()), Some(Some(ask)));
        assert_eq!(Wire::decode(&format!("{MARK}{{nonsense")), Some(None));
        assert_eq!(Wire::decode("an ordinary message"), None);
    }
}
