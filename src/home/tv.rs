//! TVs: the tools the model gets for every TV, whatever its brand, and
//! what a TV backend has to do to answer them.
//!
//! Every call takes the device mutably: a connection can learn something
//! worth keeping (webOS hands out a new client key), and home saves it.

use serde_json::{json, Value};

use super::assistant::{flag, number, text, tool};
use super::driver::backend_of;
use super::watch::Finder;
use super::{Device, Home};

/// The longest title sent anywhere; the model can say what it likes.
const MAX_TITLE: usize = 200;

pub trait TvDriver: Sync {
    fn power(&self, d: &mut Device, on: bool) -> Result<String, String>;
    fn volume(&self, d: &mut Device, level: Option<u8>, change: Option<i8>, mute: Option<bool>) -> Result<String, String>;
    /// What it is showing, and its volume.
    fn status(&self, d: &mut Device) -> Result<String, String>;
    fn open_app(&self, d: &mut Device, _app: &str) -> Result<String, String> {
        Err(format!("{} can't open apps", d.name))
    }
    /// Puts on `what` in `app`, as far as the TV lets it, and says how far
    /// that was.
    fn play(&self, d: &mut Device, _app: &str, _what: &str, _find: &Finder) -> Result<String, String> {
        Err(format!("{} can't play a chosen title", d.name))
    }
    fn input(&self, d: &mut Device, _input: &str) -> Result<String, String> {
        Err(format!("{} can't switch inputs", d.name))
    }
    fn media(&self, d: &mut Device, _action: &str) -> Result<String, String> {
        Err(format!("{} can't control playback", d.name))
    }
    fn message(&self, d: &mut Device, _text: &str) -> Result<String, String> {
        Err(format!("{} can't show messages", d.name))
    }
}

type Tv = (&'static dyn TvDriver, Device);

impl Home {
    /// TVs that can be driven; one that never finished pairing can't.
    pub(super) fn tvs(&self) -> Vec<Tv> {
        self.config
            .devices
            .iter()
            .filter_map(|d| {
                let backend = backend_of(d)?;
                backend.ready(d).then_some((backend.as_tv()?, self.reach(d)))
            })
            .collect()
    }

    pub(super) fn tv_tools(&self) -> Vec<Value> {
        let tvs = self.tvs();
        if tvs.is_empty() {
            return Vec::new();
        }
        let pick = |mut props: Value| {
            if tvs.len() > 1 {
                let names: Vec<String> = tvs.iter().map(|(_, d)| d.name.clone()).collect();
                props["tv"] = json!({"type": "string", "enum": names});
            }
            props
        };
        vec![
            tool("tv_power", "Turn the TV on or off.", pick(json!({"on": {"type": "boolean"}}))),
            tool(
                "tv_volume",
                "Set, raise, lower or mute the TV's volume.",
                pick(json!({
                    "level": {"type": "integer", "description": "absolute volume, 0 to 100"},
                    "change": {"type": "integer", "description": "steps up (positive) or down (negative)"},
                    "mute": {"type": "boolean"},
                })),
            ),
            tool(
                "tv_open_app",
                "Open an app on the TV, such as Netflix, YouTube, Prime Video, Disney+, Spotify or Live TV. This only opens it; use tv_play for a particular song, video, film or show.",
                pick(json!({"app": {"type": "string"}})),
            ),
            tool(
                "tv_play",
                "Put on a particular song, video, film or show in an app on the TV. Its answer says how far that got, which may be short of playing.",
                pick(json!({
                    "what": {"type": "string", "description": "the title, as said, with the artist if they gave one"},
                    "app": {"type": "string", "description": "the app they named; leave out to use YouTube"},
                })),
            ),
            tool("tv_input", "Switch the TV's input, such as HDMI_1.", pick(json!({"input": {"type": "string"}}))),
            tool(
                "tv_media",
                "Control what is playing.",
                pick(json!({"action": {"type": "string", "enum": ["play", "pause", "stop", "rewind", "fastForward"]}})),
            ),
            tool("tv_message", "Pop up a short message on the TV screen.", pick(json!({"text": {"type": "string"}}))),
            tool("tv_status", "Read what the TV is showing and its volume.", pick(json!({}))),
        ]
    }

    /// Runs a TV tool; `None` when `name` isn't one.
    pub(super) fn run_tv(&mut self, name: &str, args: &Value) -> Option<Result<String, String>> {
        Some(match name {
            "tv_power" => match flag(args, "on") {
                Some(on) => self.on_tv(args, |tv, d| tv.power(d, on)),
                None => Err("say whether to turn the TV on or off".into()),
            },
            "tv_volume" => {
                let level = number(args, "level").map(|l| l.clamp(0.0, 100.0) as u8);
                let change = number(args, "change").map(|c| c.clamp(-20.0, 20.0) as i8);
                let mute = flag(args, "mute");
                self.on_tv(args, |tv, d| tv.volume(d, level, change, mute))
            }
            "tv_open_app" => match text(args, "app") {
                Some(app) => self.on_tv(args, |tv, d| tv.open_app(d, &app)),
                None => Err("which app?".into()),
            },
            "tv_play" => match text(args, "what") {
                Some(what) => {
                    let what: String = what.chars().take(MAX_TITLE).collect();
                    let app = text(args, "app").unwrap_or_else(|| "YouTube".into());
                    let find = Finder::new(self.config.lookups);
                    let played = self.on_tv(args, |tv, d| tv.play(d, &app, &what, &find));
                    if find.missed() {
                        self.note("home: YouTube's results page had no video on it; its layout may have changed".into());
                    }
                    played
                }
                None => Err("play what?".into()),
            },
            "tv_input" => match text(args, "input") {
                Some(input) => self.on_tv(args, |tv, d| tv.input(d, &input)),
                None => Err("which input?".into()),
            },
            "tv_media" => match text(args, "action") {
                Some(action) => self.on_tv(args, |tv, d| tv.media(d, &action)),
                None => Err("which action?".into()),
            },
            "tv_message" => match text(args, "text") {
                Some(message) => self.on_tv(args, |tv, d| tv.message(d, &message)),
                None => Err("what should it say?".into()),
            },
            "tv_status" => self.on_tv(args, |tv, d| tv.status(d)),
            _ => return None,
        })
    }

    /// Runs `f` on the TV a request names, or the only one.
    fn on_tv(
        &mut self,
        args: &Value,
        f: impl FnOnce(&dyn TvDriver, &mut Device) -> Result<String, String>,
    ) -> Result<String, String> {
        let (tv, id) = self.pick_tv(args)?;
        self.drive(&id, |d| f(tv, d)).unwrap_or_else(|| Err("no TV is set up".into()))
    }

    fn pick_tv(&self, args: &Value) -> Result<(&'static dyn TvDriver, String), String> {
        let tvs = self.tvs();
        let picked = match text(args, "tv").map(|t| t.to_ascii_lowercase()) {
            Some(n) => tvs
                .iter()
                .find(|(_, d)| d.name.eq_ignore_ascii_case(&n))
                .or_else(|| tvs.iter().find(|(_, d)| d.name.to_ascii_lowercase().contains(&n)))
                .ok_or_else(|| format!("there's no TV called {n}"))?,
            None => tvs.first().ok_or("no TV is set up")?,
        };
        Ok((picked.0, picked.1.id()))
    }
}
