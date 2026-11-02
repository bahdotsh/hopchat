//! What a kind of device has to do to be part of home.
//!
//! Two layers. A [`Category`] (lights, TVs) is what the model sees: one set
//! of tools for every light, whoever made it, so a small model isn't handed
//! a tool per brand. A [`Backend`] (Wiz, LG webOS) is how one brand is
//! found, paired and driven, and it implements its category's trait
//! ([`LightDriver`], [`TvDriver`], [`SwitchDriver`], [`PrinterDriver`]) to take part in that
//! category's tools.
//!
//! A new brand of light or TV is one file implementing [`Backend`] and the
//! category trait, plus a line in [`BACKENDS`]. A new category is a module
//! like [`super::light`] with its trait and tools, plus a [`Category`]
//! variant.

use std::net::Ipv4Addr;

use serde_json::Value;

use super::light::LightDriver;
use super::printer::PrinterDriver;
use super::switch::SwitchDriver;
use super::tv::TvDriver;
use super::{androidtv, ha, ipp, tuya, webos, wiz, Device, State};

/// Every backend home knows, in the order an address is probed.
pub static BACKENDS: &[&dyn Backend] = &[&wiz::Wiz, &tuya::Tuya, &webos::WebOs, &androidtv::AndroidTv, &ipp::Ipp, &ha::HaLight, &ha::HaTv, &ha::HaSwitch];

/// The most devices home talks to at once when it has many to reach: a
/// home can have a hundred lights, and each is a thread and a connection.
pub const MAX_PARALLEL: usize = 16;

/// The backend a device was added with; `None` for one written by a newer
/// hopchat that knows a backend this one doesn't.
pub fn backend_of(d: &Device) -> Option<&'static dyn Backend> {
    backend(&d.backend)
}

pub fn backend(id: &str) -> Option<&'static dyn Backend> {
    BACKENDS.iter().copied().find(|b| b.id() == id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Light,
    Tv,
    Switch,
    Printer,
}

impl Category {
    /// What one is called, and what a found one is named until renamed.
    pub fn noun(self) -> &'static str {
        match self {
            Category::Light => "Light",
            Category::Tv => "TV",
            Category::Switch => "Switch",
            Category::Printer => "Printer",
        }
    }

    /// What one is called mid-sentence.
    pub fn word(self) -> &'static str {
        match self {
            Category::Light => "light",
            Category::Tv => "TV",
            Category::Switch => "switch",
            Category::Printer => "printer",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Category::Light => "💡",
            Category::Tv => "📺",
            Category::Switch => "🔌",
            Category::Printer => "🖨️",
        }
    }

    /// Something to say to home about a device called `name`.
    pub fn example(self, name: &str) -> String {
        match self {
            Category::Light => format!("dim the {name}"),
            Category::Tv => format!("put netflix on the {name}"),
            Category::Switch => format!("turn on the {name}"),
            Category::Printer => format!("is the {name} out of ink?"),
        }
    }
}

/// A device that answered, before anyone has added it.
pub struct Found {
    pub backend: &'static str,
    pub ip: Ipv4Addr,
    pub mac: Option<String>,
    /// Which thing behind that address, as in [`Device::unit`].
    pub unit: Option<String>,
    /// Whatever the backend keeps about it, as in [`Device::data`].
    pub data: Value,
    /// What it is doing, when whatever found it knows already: a hub's
    /// listing has every entity's state, so none needs asking again.
    pub checked: Option<(State, String)>,
}

/// What the Devices panel says about pairing one of these.
pub struct PairHelp {
    /// Before pairing: what Enter does, and anything to set up first.
    pub before: [&'static str; 2],
    /// While it pairs: what to do on the device.
    pub during: [&'static str; 2],
    /// How long a pairing can run, for the panel's countdown.
    pub takes: std::time::Duration,
}

pub trait Backend: Sync {
    /// What `home.json` calls it, e.g. "wiz". Never change one once shipped.
    fn id(&self) -> &'static str;
    fn category(&self) -> Category;
    /// What a scan looks for, e.g. "Wiz lights".
    fn label(&self) -> &'static str;

    /// Reports everything of this kind on the network as it answers. Runs
    /// on its own thread, beside the other backends' scans. Nothing, for
    /// one a hub lists instead.
    fn discover(&self, _each: &dyn Fn(Found)) {}
    /// What is at `ip`, if it is one of these: for a device the scan can't
    /// see (another subnet, broadcasts blocked). Must not change anything
    /// on the device.
    fn probe(&self, _ip: Ipv4Addr) -> Option<Found> {
        None
    }
    /// Whether a hub home connected to lists these, rather than a scan
    /// finding them; such a device is handed its hub in [`Device::hub`].
    fn listed_by_hub(&self) -> bool {
        false
    }

    /// What it calls itself, to name it by when found; `None` to name it
    /// after its category.
    fn name(&self, _d: &Device) -> Option<String> {
        None
    }
    /// A few words for the model about one, e.g. "white tones".
    fn about(&self, d: &Device) -> String;
    /// What the Devices panel says about one it found.
    fn found_detail(&self, d: &Device) -> String {
        self.about(d)
    }
    /// Whether an added one is answering, and what it is doing. Runs on a
    /// worker thread.
    fn check(&self, d: &Device) -> (State, String);
    /// Shows which one it is (a blink, a hello on screen).
    fn test(&self, d: &mut Device) -> Result<String, String>;
    /// What [`Backend::test`] does, after "Enter", e.g. " blinks it".
    fn test_help(&self) -> &'static str;

    /// `Some` for a device that must pair before it can be driven.
    fn pairing(&self) -> Option<&'static PairHelp> {
        None
    }
    /// Pairs, keeping in `d` whatever later connections need. Runs on a
    /// worker thread; the answer is the panel's detail for it.
    fn pair(&self, _d: &mut Device) -> Result<String, String> {
        Err("this device doesn't pair".into())
    }
    /// Whether an added one can be driven: a TV that never finished
    /// pairing can't.
    fn ready(&self, _d: &Device) -> bool {
        true
    }

    fn as_light(&self) -> Option<&dyn LightDriver> {
        None
    }
    fn as_tv(&self) -> Option<&dyn TvDriver> {
        None
    }
    fn as_switch(&self) -> Option<&dyn SwitchDriver> {
        None
    }
    fn as_printer(&self) -> Option<&dyn PrinterDriver> {
        None
    }
}
