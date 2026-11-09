//! `@ai`: lets the people in a group ask the model running on this
//! machine anything, not just about the devices.
//!
//! Answers are written on a worker thread, so a long one doesn't hold up
//! the devices, and each goes back as a reply to the question it answers.
//! Each group keeps a short memory of what was asked and answered, so
//! "and in French?" works.

use std::thread;

use serde_json::{json, Value};

use super::ollama::{text_of, Ollama};
use super::{Home, ModelState, Request};
use crate::engine::{Command, Input, Quote};
use crate::lobby::LOBBY;
use crate::me::ME;

/// The word that addresses the model in a group.
pub const MENTION: &str = "@ai";
/// What every answer from the model starts with.
pub const SIGNATURE: &str = "🤖 ";
const MAX_QUESTION_CHARS: usize = 2000;
/// Long enough for a real answer, short enough to read in a chat.
const MAX_ANSWER_CHARS: usize = 4000;
/// Questions and answers each group's memory keeps.
const MEMORY: usize = 12;

/// A group message that mentioned [`MENTION`].
pub struct Question {
    pub group: String,
    pub from: String,
    /// The message's id and sender, so the answer can reply to it.
    pub id: String,
    pub sender: String,
    pub text: String,
    /// The person at this machine asked.
    pub mine: bool,
}

impl Home {
    pub fn ai_enabled(&self, group: &str) -> bool {
        self.config.ai_groups.iter().any(|g| g == group)
    }

    /// For everyone where it is switched on, and for the person at this
    /// machine in Me, the lobby, and any group while a model runs here.
    /// Without one it keeps quiet in a group rather than borrow the very
    /// model whose owner switched `@ai` on there and is answering already.
    fn ai_answers(&self, q: &Question) -> bool {
        self.ai_enabled(&q.group)
            || (q.mine
                && (q.group == LOBBY
                    || q.group == ME
                    || self.model_state == ModelState::Ready
                    || self.pools.url().is_some()))
    }

    pub(super) fn ai_ask(&mut self, q: Question) {
        if !self.ai_answers(&q) {
            if q.mine {
                self.note("ai: no model here, so whoever switched @ai on in this group answers".into());
            }
            return;
        }
        let text: String = q.text.chars().take(MAX_QUESTION_CHARS).collect();
        if text.is_empty() {
            return self.ai_say(&q, "ask me something after @ai.".into());
        }
        let mut messages = vec![json!({"role": "system", "content": self.ai_prompt()})];
        messages.extend(self.memory.get(&q.group).cloned().unwrap_or_default());
        let asked = json!({"role": "user", "content": format!("{}: {text}", q.from)});
        messages.push(asked.clone());
        let mind = self.mind(&q.group, Ollama::patient(self.config.ollama.clone(), self.config.model.clone()));
        let me = self.me.clone();
        thread::spawn(move || {
            let answer = mind.think(&messages, None).map(|thought| text_of(&thought.message));
            let _ = me.send(Request::Answered { question: q, asked, answer });
        });
    }

    pub(super) fn ai_answered(&mut self, q: Question, asked: Value, answer: Result<String, String>) {
        // Switched off while it was thinking.
        if !self.ai_answers(&q) {
            return;
        }
        let text = match answer {
            Ok(text) if !text.is_empty() => {
                let memory = self.memory.entry(q.group.clone()).or_default();
                memory.push(asked);
                memory.push(json!({"role": "assistant", "content": text}));
                let excess = memory.len().saturating_sub(MEMORY * 2);
                memory.drain(..excess);
                clip(&text)
            }
            Ok(_) => "I don't have an answer to that.".into(),
            Err(e) => {
                self.note(format!("ai: {e}"));
                format!("can't think right now: {e}")
            }
        };
        self.ai_say(&q, text);
    }

    fn ai_say(&self, q: &Question, text: String) {
        let _ = self.engine.send(Input::Ui(Command::SendGroup {
            group: q.group.clone(),
            text: format!("{SIGNATURE}{text}"),
            reply: Some(Quote {
                id: q.id.clone(),
                sender: q.sender.clone(),
                text: q.text.chars().take(80).collect(),
            }),
            from_home: true,
        }));
    }

    /// `/ai on|off` in a group, and the bare `/ai` status.
    pub(super) fn ai_configure(&mut self, group: Option<&str>, verb: &str, value: Option<&str>) -> String {
        match verb {
            "on" => {
                let Some(group) = group else {
                    return "open a group first: /ai on lets that group ask your model".into();
                };
                if !self.ai_enabled(group) {
                    self.config.ai_groups.push(group.to_string());
                    self.save();
                }
                let hello = if self.model_state == super::ModelState::Ready {
                    format!("{} is here. Mention {MENTION} to ask it anything.", self.config.model)
                } else {
                    format!("is listening. Mention {MENTION} to ask anything; a model someone here shares will answer.")
                };
                self.say_as(group, SIGNATURE, hello);
                if group == LOBBY {
                    return format!("everyone in the lobby can ask {} now", self.config.model);
                }
                format!("this group can ask {} now", self.config.model)
            }
            "off" => {
                let Some(group) = group else { return "open the group to switch off".into() };
                self.config.ai_groups.retain(|g| g != group);
                self.memory.remove(group);
                self.save();
                self.say_as(group, SIGNATURE, "is only answering its owner now.".into());
                if group == LOBBY {
                    return "only you can ask your model in the lobby now".into();
                }
                "only you can ask your model in this group now".into()
            }
            "share" | "lend" => {
                self.config.share = !matches!(value, Some("off" | "no"));
                self.save();
                self.offer();
                if self.config.share {
                    format!("group members can borrow your {} when theirs isn't running", self.config.model)
                } else {
                    "nobody can borrow your model now".into()
                }
            }
            "forget" => {
                let Some(group) = group else { return "open the group whose chat to forget".into() };
                self.memory.remove(group);
                "forgot this group's chat with the model".into()
            }
            _ => {
                let n = self.config.ai_groups.len();
                let here = match group {
                    Some(g) if self.ai_enabled(g) => " · on here",
                    Some(_) => " · off here",
                    None => "",
                };
                let shared = if self.config.share { "shared" } else { "not shared" };
                let borrow = if self.lenders.is_empty() {
                    String::new()
                } else {
                    format!(" · can borrow {}", self.lenders.join(", "))
                };
                format!(
                    "ai: model {} ({shared}) · on in {n} group{}{here}{borrow} · /ai on|off in a group · /ai share on|off",
                    self.config.model,
                    if n == 1 { "" } else { "s" },
                )
            }
        }
    }

    fn ai_prompt(&self) -> String {
        format!(
            "You are {}, a helpful assistant running on one person's computer, answering in a \
             group chat on their local network. Each message starts with the sender's name; \
             several people may be talking to you. Answer the latest message directly and \
             concisely, in plain text that reads well in a terminal: no tables, little or no \
             markdown. You have no internet access and can't see anything outside this chat.",
            self.config.model,
        )
    }
}

/// Whether `text` addresses the model, and the question with the mention
/// removed.
pub fn addressed(text: &str) -> Option<String> {
    super::mentioned(text, MENTION)
}

fn clip(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= MAX_ANSWER_CHARS {
        return text.to_string();
    }
    let cut: String = text.chars().take(MAX_ANSWER_CHARS).collect();
    format!("{}…", cut.trim_end())
}
