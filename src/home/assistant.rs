//! The model's side of home: what it is told, the tools it may call, and
//! running those tools against the devices that have been added.

use serde_json::{json, Value};

use super::driver::backend_of;
use super::ollama::{Ollama, Turn};
use super::{Device, Home};

/// Model turns per request: act, maybe act again on what it saw, then say.
const MAX_TURNS: usize = 4;
/// The most scenes the model is told of by name. A strip can list a
/// hundred effects, and every one would be in every request's prompt.
const MAX_SCENES_SAID: usize = 40;

/// The scenes as the system prompt names them: the first few, and how
/// many more there are, any of which `light_set` still takes by name.
fn said_scenes(scenes: &[String]) -> String {
    match scenes.len() {
        0 => String::new(),
        n if n <= MAX_SCENES_SAID => format!(" Light scenes: {}.", scenes.join(", ")),
        n => format!(
            " Light scenes include: {}, and {} more; use any scene name they ask for.",
            scenes[..MAX_SCENES_SAID].join(", "),
            n - MAX_SCENES_SAID,
        ),
    }
}

impl Home {
    pub(super) fn ask(&mut self, group: &str, from: &str, text: &str) -> String {
        if self.lights().is_empty() && self.tvs().is_empty() && self.switches().is_empty() && self.printers().is_empty() {
            return "no devices are set up yet; whoever hosts home can add them with /devices.".into();
        }
        let mind = self.mind(group, Ollama::new(self.config.ollama.clone(), self.config.model.clone()));
        let tools = self.tools();
        let mut messages = vec![
            json!({"role": "system", "content": self.system_prompt()}),
            json!({"role": "user", "content": format!("{from}: {text}")}),
        ];
        let mut done = Vec::new();
        // Whose model thought, when it wasn't this machine's.
        let mut via = None;
        for _ in 0..MAX_TURNS {
            let turn = mind.think(&messages, Some(&tools)).map(|thought| {
                if thought.by.is_some() && via.is_none() {
                    self.note(format!("home: thinking with {}", thought.by.as_deref().unwrap_or_default()));
                    via = thought.by;
                }
                Turn::from_message(thought.message)
            });
            match turn {
                Ok(Turn::Say(text)) if !text.is_empty() => return one_line(&text),
                Ok(Turn::Say(_)) => break,
                Ok(Turn::Act(calls, assistant)) => {
                    messages.push(assistant);
                    for call in calls {
                        let text = match self.run_tool(&call.name, &call.args) {
                            Ok(t) => t,
                            Err(e) => format!("error: {e}"),
                        };
                        done.push(text.clone());
                        messages.push(json!({"role": "tool", "tool_name": call.name, "content": text}));
                    }
                }
                Err(e) => {
                    self.note(format!("home: {e}"));
                    return if done.is_empty() {
                        format!("can't think right now: {e}")
                    } else {
                        done.join("; ")
                    };
                }
            }
        }
        if done.is_empty() {
            "I'm not sure what to do with that.".into()
        } else {
            done.join("; ")
        }
    }

    pub(super) fn system_prompt(&self) -> String {
        let lights = self.lights();
        let tvs = self.tvs();
        let switches = self.switches();
        let printers = self.printers();
        let devices: Vec<String> = lights
            .iter()
            .map(|(_, d)| d)
            .chain(tvs.iter().map(|(_, d)| d))
            .chain(switches.iter().map(|(_, d)| d))
            .chain(printers.iter().map(|(_, d)| d))
            .filter_map(|d| {
                let backend = backend_of(d)?;
                Some(format!("- {} \"{}\" ({})", backend.category().word(), d.name, backend.about(d)))
            })
            .collect();
        let several = lights.len() > 1;
        let scenes = self.scenes();
        format!(
            "You are home, the assistant for a household's devices, answering in a group chat. \
             The devices are:\n{}\n\
             Each message starts with the sender's name. Use the tools to do what they ask, and \
             check state with a status tool when they ask how things are. Never invent device \
             state. Only say something is playing if tv_play or tv_media said so; when tv_play says \
             it only opened an app or a page, or searched, tell them that instead. {}A light that only does white tones cannot show colours; say so instead of \
             trying. After acting, reply with one short, friendly sentence saying what you did. \
             If a request has nothing to do with the devices, say briefly that you only handle \
             the devices.{}",
            devices.join("\n"),
            if several {
                "When they say \"the lights\" without naming one, use all of them. "
            } else {
                ""
            },
            said_scenes(&scenes),
        )
    }

    /// One set of tools per category that has a device here.
    pub(super) fn tools(&self) -> Value {
        let mut tools = self.light_tools();
        tools.extend(self.tv_tools());
        tools.extend(self.switch_tools());
        tools.extend(self.printer_tools());
        Value::Array(tools)
    }

    pub(super) fn run_tool(&mut self, name: &str, args: &Value) -> Result<String, String> {
        self.note(format!("home: {name} {args}"));
        self.run_light(name, args)
            .or_else(|| self.run_tv(name, args))
            .or_else(|| self.run_switch(name, args))
            .or_else(|| self.run_printer(name, args))
            .unwrap_or_else(|| Err(format!("there's no tool called {name}")))
    }
}

pub(super) fn tool(name: &str, description: &str, properties: Value) -> Value {
    json!({"type": "function", "function": {
        "name": name,
        "description": description,
        "parameters": {"type": "object", "properties": properties},
    }})
}

// ----- lenient argument reading: small models send "50" as often as 50 ----

pub(super) fn number(args: &Value, key: &str) -> Option<f64> {
    match args.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().trim_end_matches(['%', 'K', 'k']).parse().ok(),
        _ => None,
    }
}

pub(super) fn flag(args: &Value, key: &str) -> Option<bool> {
    match args.get(key)? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" => Some(true),
            "false" | "off" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

pub(super) fn text(args: &Value, key: &str) -> Option<String> {
    args.get(key)?.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// The device a request names under `one` (e.g. "switch"), or the only
/// one there is: an exact name first, then a part of one. `many` is what
/// several are called, for saying none are set up.
pub(super) fn pick<T>(mut all: Vec<(T, Device)>, args: &Value, one: &str, many: &str) -> Result<(T, Device), String> {
    let Some(n) = text(args, one).map(|n| n.to_ascii_lowercase()) else {
        return match all.len() {
            0 => Err(format!("no {many} are set up")),
            1 => Ok(all.remove(0)),
            _ => Err(format!("say which {one}")),
        };
    };
    if let Some(at) = all.iter().position(|(_, d)| d.name.eq_ignore_ascii_case(&n)) {
        return Ok(all.remove(at));
    }
    let mut close: Vec<(T, Device)> = all.into_iter().filter(|(_, d)| d.name.to_ascii_lowercase().contains(&n)).collect();
    match close.len() {
        0 => Err(format!("there's no {one} called {n}")),
        1 => Ok(close.remove(0)),
        _ => Err(format!("which {n}? {}", close.iter().map(|(_, d)| d.name.as_str()).collect::<Vec<_>>().join(", "))),
    }
}

pub(super) fn colour(name: &str) -> Option<(u8, u8, u8)> {
    let name = name.trim().to_ascii_lowercase();
    if let Some(hex) = name.strip_prefix('#')
        && hex.len() == 6
    {
        let v = u32::from_str_radix(hex, 16).ok()?;
        return Some(((v >> 16) as u8, (v >> 8) as u8, v as u8));
    }
    Some(match name.as_str() {
        "red" => (255, 0, 0),
        "orange" => (255, 120, 0),
        "yellow" => (255, 200, 0),
        "green" => (0, 255, 0),
        "cyan" | "aqua" => (0, 255, 255),
        "teal" => (0, 160, 140),
        "blue" => (0, 0, 255),
        "purple" | "violet" => (140, 0, 255),
        "magenta" => (255, 0, 255),
        "pink" => (255, 80, 160),
        _ => return None,
    })
}

fn one_line(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(400).collect()
}
