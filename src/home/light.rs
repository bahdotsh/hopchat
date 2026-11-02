//! Lights: the tools the model gets for every light, whatever its brand,
//! and what a light backend has to do to answer them.

use serde_json::{json, Value};

use super::assistant::{colour, flag, number, text, tool};
use super::driver::{backend_of, MAX_PARALLEL};
use super::{Device, Home};

/// What a light should change to; anything left `None` stays as it is.
#[derive(Default, Debug, PartialEq)]
pub struct Pilot {
    pub on: Option<bool>,
    /// Percent, 0–100; each backend clamps it to what its lights take
    /// (a Wiz bulb goes no lower than 10, HA no lower than 1).
    pub brightness: Option<u8>,
    /// 2200–6500 K.
    pub kelvin: Option<u16>,
    pub rgb: Option<(u8, u8, u8)>,
    /// One of the backend's [`LightDriver::scenes`].
    pub scene: Option<String>,
}

/// What a light can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Caps {
    pub colour: bool,
    pub white: bool,
}

impl Caps {
    pub fn describe(&self) -> &'static str {
        match (self.colour, self.white) {
            (true, _) => "colour",
            (false, true) => "white tones",
            (false, false) => "dimmable",
        }
    }
}

pub trait LightDriver: Sync {
    fn caps(&self, d: &Device) -> Caps;
    /// The scenes it has built in, by the names people say.
    fn scenes(&self, _d: &Device) -> Vec<String> {
        Vec::new()
    }
    /// Changes it, and says what it is doing now when the answer to the
    /// change said so; `None` to ask with [`status`](Self::status).
    fn set(&self, d: &Device, pilot: &Pilot) -> Result<Option<String>, String>;
    /// A one-line description of what it is doing now.
    fn status(&self, d: &Device) -> Result<String, String>;
}

type Light = (&'static dyn LightDriver, Device);

impl Home {
    /// Lights that can be driven; one that never finished pairing can't.
    pub(super) fn lights(&self) -> Vec<Light> {
        self.config
            .devices
            .iter()
            .filter_map(|d| {
                let backend = backend_of(d)?;
                backend.ready(d).then_some((backend.as_light()?, self.reach(d)))
            })
            .collect()
    }

    /// Every scene some light here has, taking each light's first, then
    /// each one's second, and so on, so a strip with a hundred effects
    /// doesn't push every other light's scenes to the end.
    pub(super) fn scenes(&self) -> Vec<String> {
        let each: Vec<Vec<String>> = self.lights().iter().map(|(driver, d)| driver.scenes(d)).collect();
        let longest = each.iter().map(Vec::len).max().unwrap_or(0);
        let mut scenes: Vec<String> = Vec::new();
        for i in 0..longest {
            for scene in each.iter().filter_map(|list| list.get(i)) {
                if !scenes.contains(scene) {
                    scenes.push(scene.clone());
                }
            }
        }
        scenes
    }

    pub(super) fn light_tools(&self) -> Vec<Value> {
        let lights = self.lights();
        if lights.is_empty() {
            return Vec::new();
        }
        let mut names: Vec<String> = lights.iter().map(|(_, d)| d.name.clone()).collect();
        names.push("all".into());
        let caps: Vec<Caps> = lights.iter().map(|(l, d)| l.caps(d)).collect();
        let mut props = json!({
            "light": {"type": "string", "enum": names, "description": "which light; all for every one"},
            "on": {"type": "boolean", "description": "true to switch on, false to switch off"},
            "brightness": {"type": "integer", "description": "percent, 1 to 100"},
        });
        if !self.scenes().is_empty() {
            props["scene"] = json!({"type": "string", "description": "one of the light scene names"});
        }
        if caps.iter().any(|c| c.white) {
            props["kelvin"] = json!({"type": "integer", "description": "white temperature, 2200 (warm) to 6500 (cool)"});
        }
        if caps.iter().any(|c| c.colour) {
            props["color"] = json!({"type": "string", "description": "a colour name like red or purple, or #rrggbb"});
        }
        vec![
            tool(
                "light_set",
                "Change a light. Give only what should change. Setting brightness, temperature, \
                 colour or a scene also turns it on.",
                props,
            ),
            tool(
                "light_status",
                "Read what the lights are doing now.",
                json!({"light": {"type": "string", "description": "which light; omit for all"}}),
            ),
        ]
    }

    /// Runs a light tool; `None` when `name` isn't one.
    pub(super) fn run_light(&mut self, name: &str, args: &Value) -> Option<Result<String, String>> {
        match name {
            "light_set" => Some(self.light_set(args)),
            "light_status" => Some(self.pick_lights(text(args, "light").as_deref()).map(|lights| {
                each_light(&lights, |l, d| match l.status(d) {
                    Ok(state) => format!("{} is {state}", d.name),
                    Err(e) => format!("{}: {e}", d.name),
                })
                .join("; ")
            })),
            _ => None,
        }
    }

    fn light_set(&mut self, args: &Value) -> Result<String, String> {
        let rgb = match text(args, "color") {
            Some(c) => Some(colour(&c).ok_or_else(|| format!("I don't know the colour {c}"))?),
            None => None,
        };
        let lights = self.pick_lights(text(args, "light").as_deref())?;
        let scene = text(args, "scene");
        if let Some(s) = &scene {
            let wanted = scene_key(s);
            if !lights.iter().any(|(l, d)| l.scenes(d).iter().any(|n| scene_key(n) == wanted)) {
                return Err(format!("no scene called {s}"));
            }
        }
        let pilot = Pilot {
            on: flag(args, "on"),
            brightness: number(args, "brightness").map(|b| b.clamp(0.0, 100.0) as u8),
            kelvin: number(args, "kelvin").map(|k| k.clamp(0.0, 10000.0) as u16),
            rgb,
            scene,
        };
        let results = each_light(&lights, |l, d| {
            let caps = l.caps(d);
            if pilot.rgb.is_some() && !caps.colour {
                return format!("{} only does white tones, not colours", d.name);
            }
            if pilot.kelvin.is_some() && !caps.white {
                return format!("{} only dims; it has no white temperature", d.name);
            }
            let now = l.set(d, &pilot).and_then(|told| told.map_or_else(|| l.status(d), Ok));
            match now {
                Ok(state) => format!("{} is now {state}", d.name),
                Err(e) => format!("{}: {e}", d.name),
            }
        });
        Ok(results.join("; "))
    }

    /// The lights a request names; none named, or "all", means every one.
    fn pick_lights(&self, name: Option<&str>) -> Result<Vec<Light>, String> {
        let lights = self.lights();
        let wanted = name.map(str::to_ascii_lowercase).filter(|n| n != "all" && !n.is_empty());
        let picked: Vec<Light> = match &wanted {
            None => lights,
            // "Light" is one light, not "Light 2" and "Light 3" too.
            Some(n) => match lights.iter().position(|(_, d)| d.name.eq_ignore_ascii_case(n)) {
                Some(at) => vec![lights.into_iter().nth(at).unwrap()],
                None => lights
                    .into_iter()
                    .filter(|(_, d)| d.name.to_ascii_lowercase().contains(n.as_str()))
                    .collect(),
            },
        };
        if picked.is_empty() {
            return Err(match wanted {
                Some(n) => format!("there's no light called {n}"),
                None => "no lights are set up".into(),
            });
        }
        Ok(picked)
    }
}

/// `f` on every light, [`MAX_PARALLEL`] at a time, its answers in the
/// lights' order. Each light can take a few seconds to answer, or to time
/// out when its hub is down, so "all" takes as long as the slowest light
/// in each batch, not their sum: home answers nothing else meanwhile.
fn each_light<F>(lights: &[Light], f: F) -> Vec<String>
where
    F: Fn(&dyn LightDriver, &Device) -> String + Sync,
{
    if let [(l, d)] = lights {
        return vec![f(*l, d)];
    }
    std::thread::scope(|s| {
        let f = &f;
        lights
            .chunks(MAX_PARALLEL)
            .flat_map(|batch| {
                let running: Vec<_> = batch.iter().map(|(l, d)| s.spawn(move || f(*l, d))).collect();
                running
                    .into_iter()
                    .zip(batch)
                    .map(|(r, (_, d))| r.join().unwrap_or_else(|_| format!("{}: something went wrong", d.name)))
                    .collect::<Vec<_>>()
            })
            .collect()
    })
}

/// A scene name as people say it: "Night-Light" is "night light".
pub fn scene_key(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace(['_', '-'], " ")
}
