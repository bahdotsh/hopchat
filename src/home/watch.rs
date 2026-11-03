//! What "play this in that app" becomes, whatever the TV.
//!
//! Only YouTube can be started on a title from here, because a video's id
//! is all it takes and its results page gives that away. For films and
//! shows, JustWatch knows which app has a title and the link to its page
//! there, which leaves one press of play. Anything else takes a search link
//! at best, and a reply has to say so rather than claim it's playing.
//!
//! These lookups are the one thing home sends off the network: the title
//! being asked for, to youtube.com or apis.justwatch.com, and only once
//! `/home lookups on` has allowed it. With lookups off, an app gets a search
//! link if it takes one. Spotify keeps its search either way: its ids take
//! an account to look up.

use std::cell::Cell;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};

const CATALOG: &str = "https://apis.justwatch.com/graphql";
/// Offers a subscription or nothing at all pays for; preferred over rentals.
const INCLUDED: &[&str] = &["FLATRATE", "FREE", "ADS"];
const MAX_LINK: usize = 512;
/// How many titles to look through for the one asked for.
const CANDIDATES: u32 = 8;
const LOOKUP: Duration = Duration::from_secs(6);
/// More than any answer needs; what comes back is a stranger's.
const MAX_PAGE: u64 = 4 * 1024 * 1024;
/// YouTube's own filter for videos alone, so no channel or playlist comes
/// first.
const VIDEOS_ONLY: &str = "EgIQAQ%253D%253D";
const VIDEO: &str = "\"videoRenderer\":{\"videoId\":\"";
const VIDEO_ID: usize = 11;
/// Apps that take a search link, with what the link starts with.
const SEARCHES: &[(&str, &str, &str)] = &[
    ("netflix", "Netflix", "https://www.netflix.com/search?q="),
    ("prime video", "Prime Video", "https://app.primevideo.com/search?phrase="),
    ("amazon", "Prime Video", "https://app.primevideo.com/search?phrase="),
    ("disney", "Disney+", "https://www.disneyplus.com/search?q="),
    ("spotify", "Spotify", "spotify:search:"),
];

/// Turns a request for a title into a [`Pick`], going off the network if
/// lookups are on. Asking it is what sends the title away, so a driver asks
/// only once it knows the TV can use the answer.
pub struct Finder {
    lookups: Option<Lookups>,
    missed: Cell<bool>,
}

/// Where lookups go, as functions so tests can stand in for the internet.
struct Lookups {
    country: String,
    results: fn(&str) -> Option<String>,
    catalog: fn(&str, &str) -> Option<String>,
}

impl Finder {
    /// `country` is where to look a title up, and `None` keeps titles on
    /// this network, which is the household's say.
    pub fn new(country: Option<&str>) -> Finder {
        let lookups = country.map(|c| Lookups { country: c.to_string(), results: youtube_results, catalog });
        Finder { lookups, missed: Cell::new(false) }
    }

    /// Where to go for `what` in `app`, or `None` for an app that only
    /// opens. Can take a few seconds, and sends `what` off the network if
    /// lookups are on.
    pub fn find(&self, app: &str, what: &str) -> Option<Pick> {
        let Some(lookups) = &self.lookups else {
            return pick_with(app, what, |_| None, |_| None);
        };
        let video = |what: &str| {
            let page = (lookups.results)(what)?;
            let video = first_video(&page);
            self.missed.set(video.is_none());
            video
        };
        let title = |what: &str| title_link(&(lookups.catalog)(&lookups.country, what)?, app, what);
        pick_with(app, what, video, title)
    }

    /// Whether a results page came back with no video on it, which is what
    /// YouTube changing its page looks like.
    pub fn missed(&self) -> bool {
        self.missed.get()
    }

    #[cfg(test)]
    pub fn with(results: fn(&str) -> Option<String>) -> Finder {
        let lookups = Lookups { country: "US".into(), results, catalog: |_, _| None };
        Finder { lookups: Some(lookups), missed: Cell::new(false) }
    }
}

/// Where a request for a title ended up.
#[derive(Debug, PartialEq)]
pub enum Pick {
    /// A video YouTube will start by itself, by id and by link.
    Video { id: String, link: String, music: bool },
    /// A link to a film or show's own page in the app, as the person named
    /// the app.
    Title { link: String, app: String },
    /// A link to the app's search for it, with the app's name.
    Search { link: String, app: &'static str },
}

impl Pick {
    pub fn link(&self) -> &str {
        match self {
            Pick::Video { link, .. } | Pick::Title { link, .. } | Pick::Search { link, .. } => link,
        }
    }

    /// The video to name to a TV that takes an id rather than a link.
    pub fn youtube_video(&self) -> Option<&str> {
        match self {
            Pick::Video { id, .. } => Some(id),
            Pick::Title { .. } | Pick::Search { .. } => None,
        }
    }

    /// What to tell the model, which repeats it to the household.
    pub fn said(&self, what: &str) -> String {
        match self {
            Pick::Video { music: true, .. } => format!("sent the top YouTube match for {what} to YouTube Music"),
            Pick::Video { .. } => sent_to_youtube(what),
            Pick::Title { app, .. } => {
                format!("opened the closest match for {what} in {app}; someone may still have to press play")
            }
            Pick::Search { app, .. } => {
                format!("{app} was sent a search for {what}; nothing plays until someone picks a result")
            }
        }
    }
}

/// What to say when a video went to YouTube itself.
pub fn sent_to_youtube(what: &str) -> String {
    format!("sent the top YouTube match for {what} to YouTube")
}

/// What to say when `app` can only be opened.
pub fn only_opened(app: &str, what: &str) -> String {
    format!("opened {app}, but can't tell it to play {what}; nothing is playing yet, so someone has to find it there")
}

fn pick_with(
    app: &str,
    what: &str,
    video: impl Fn(&str) -> Option<String>,
    title: impl Fn(&str) -> Option<String>,
) -> Option<Pick> {
    let spoken = app.trim().to_ascii_lowercase();
    let query = encode(what.trim());
    if spoken.contains("youtube") {
        let music = spoken.contains("music");
        let host = if music { "music.youtube.com" } else { "www.youtube.com" };
        return Some(match video(what.trim()) {
            Some(id) => Pick::Video { link: format!("https://{host}/watch?v={id}"), id, music },
            None if music => Pick::Search { link: format!("https://{host}/search?q={query}"), app: "YouTube Music" },
            None => Pick::Search { link: format!("https://{host}/results?search_query={query}"), app: "YouTube" },
        });
    }
    let songs = spoken.contains("music") || spoken.contains("spotify");
    if !songs && let Some(link) = title(what.trim()) {
        return Some(Pick::Title { link, app: app.trim().to_string() });
    }
    SEARCHES
        .iter()
        // Amazon Music isn't Prime Video.
        .find(|(name, ..)| spoken.contains(name) && !spoken.contains("music"))
        .map(|(_, app, link)| Pick::Search { link: format!("{link}{query}"), app })
}

/// Percent-encodes `text` for a link: everything but the unreserved
/// characters, so nothing said can reach past the query.
fn encode(text: &str) -> String {
    text.bytes().fold(String::new(), |mut out, b| {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
        out
    })
}

/// One client for every lookup.
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let tls = ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::NativeTls)
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .build();
        ureq::Agent::config_builder().tls_config(tls).timeout_global(Some(LOOKUP)).build().new_agent()
    })
}

/// YouTube's results page for `what`, if it can be reached.
fn youtube_results(what: &str) -> Option<String> {
    let url = format!("https://www.youtube.com/results?search_query={}&sp={VIDEOS_ONLY}", encode(what));
    let mut page = agent()
        .get(url)
        .header("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Chrome/130.0 Safari/537.36")
        .header("Accept-Language", "en")
        // Skips the consent page some countries get first.
        .header("Cookie", "SOCS=CAI")
        .call()
        .ok()?;
    page.body_mut().with_config().limit(MAX_PAGE).read_to_string().ok()
}

/// What the catalogue has for `what` in `country`, as it answered.
fn catalog(country: &str, what: &str) -> Option<String> {
    let query = "query Find($country: Country!, $language: Language!, $first: Int!, $filter: TitleFilter) {
        popularTitles(country: $country, first: $first, filter: $filter) { edges { node {
            content(country: $country, language: $language) { title }
            offers(country: $country, platform: WEB) { monetizationType standardWebURL package { clearName } }
        } } }
    }";
    let variables = json!({"country": country, "language": "en", "first": CANDIDATES, "filter": {"searchQuery": what}});
    let mut answer = agent()
        .post(CATALOG)
        .send_json(json!({"operationName": "Find", "query": query, "variables": variables}))
        .ok()?;
    answer.body_mut().with_config().limit(MAX_PAGE).read_to_string().ok()
}

/// The link to `what` in `app` from a catalogue answer. A title it has in
/// some other app, or one that merely came up, is no use: "Dark" in Prime
/// Video mustn't start "Black". An exact name wins over a near one.
fn title_link(answer: &str, app: &str, what: &str) -> Option<String> {
    let answer: Value = serde_json::from_str(answer).ok()?;
    let titles = answer["data"]["popularTitles"]["edges"].as_array()?;
    let (app, wanted) = (plain(app), plain(what));
    if app.is_empty() || wanted.is_empty() {
        return None;
    }
    let named = |t: &&Value| t["node"]["content"]["title"].as_str().map(plain).unwrap_or_default();
    let exact = titles.iter().filter(|t| named(t) == wanted);
    let near = titles.iter().filter(|t| {
        let name = named(t);
        !name.is_empty() && (name.contains(&wanted) || wanted.contains(&name))
    });
    exact.chain(near).find_map(|t| offer(&t["node"], &app))
}

/// A title's link in `app`, a subscription's before a rental's.
fn offer(title: &Value, app: &str) -> Option<String> {
    let offers = title["offers"].as_array()?;
    let ours = || {
        offers.iter().filter(|o| {
            o["package"]["clearName"].as_str().is_some_and(|n| plain(n).contains(app)) && o["standardWebURL"].as_str().is_some_and(safe_link)
        })
    };
    let included = |o: &&Value| o["monetizationType"].as_str().is_some_and(|m| INCLUDED.contains(&m));
    let best = ours().find(included).or_else(|| ours().next())?;
    best["standardWebURL"].as_str().map(str::to_string)
}

/// The answer is a stranger's and its links go to the TV, so only a plain
/// web link gets through.
fn safe_link(link: &str) -> bool {
    link.starts_with("https://") && link.len() <= MAX_LINK && link.bytes().all(|b| b.is_ascii_graphic() && !b"\"'<>\\".contains(&b))
}

/// `text` as letters and digits, to compare names however they're written.
fn plain(text: &str) -> String {
    text.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// The first video on a results page. The page is a stranger's, so only
/// something shaped like a video id comes out of it.
fn first_video(page: &str) -> Option<String> {
    let (_, rest) = page.split_once(VIDEO)?;
    let (id, _) = rest.split_once('"')?;
    (id.len() == VIDEO_ID && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')).then(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(_: &str) -> Option<String> {
        Some("pMpTUqJMnzM".into())
    }

    fn no_title(_: &str) -> Option<String> {
        None
    }

    fn pick_with(app: &str, what: &str, video: impl Fn(&str) -> Option<String>) -> Option<Pick> {
        super::pick_with(app, what, video, no_title)
    }

    /// As the catalogue answered a search for "Dark".
    fn answer() -> String {
        let offer = |name: &str, how: &str, link: &str| json!({"monetizationType": how, "standardWebURL": link, "package": {"clearName": name}});
        let title = |name: &str, offers: Value| json!({"node": {"content": {"title": name}, "offers": offers}});
        json!({"data": {"popularTitles": {"edges": [
            title("The Dark Knight", json!([offer("Netflix", "FLATRATE", "https://www.netflix.com/title/1")])),
            title("Black", json!([offer("Amazon Prime Video", "FLATRATE", "https://app.primevideo.com/detail?gti=black")])),
            title("Dark", json!([
                offer("JioHotstar", "RENT", "https://www.hotstar.com/in/2"),
                offer("JioHotstar", "FLATRATE", "https://www.hotstar.com/in/3"),
                offer("Netflix", "FLATRATE", "https://www.netflix.com/title/80100172"),
                offer("Bad", "FLATRATE", "intent://evil"),
            ])),
        ]}}}).to_string()
    }

    #[test]
    fn a_title_is_found_in_the_app_named_or_not_at_all() {
        let link = |app, what| title_link(&answer(), app, what);
        assert_eq!(link("Netflix", "Dark").as_deref(), Some("https://www.netflix.com/title/80100172"), "exact beats first");
        assert_eq!(link("hotstar", "dark").as_deref(), Some("https://www.hotstar.com/in/3"), "subscription beats rental");
        assert_eq!(link("Prime Video", "Dark"), None, "never a different title");
        assert_eq!(link("Bad", "Dark"), None, "only web links reach the TV");
        assert_eq!(link("Netflix", "!!!"), None);
        assert_eq!(title_link("not json", "Netflix", "Dark"), None);
    }

    #[test]
    fn a_title_page_wins_over_a_search_and_says_how_far_it_got() {
        let page = |_: &str| Some("https://www.netflix.com/title/1".to_string());
        let pick = super::pick_with("Netflix", "Dark", found, page).unwrap();
        assert_eq!(pick.link(), "https://www.netflix.com/title/1");
        assert!(pick.said("Dark").contains("press play"));
        assert!(matches!(super::pick_with("hotstar", "x", found, page), Some(Pick::Title { .. })), "any app the catalogue knows");
        assert!(matches!(super::pick_with("spotify", "x", found, page), Some(Pick::Search { .. })), "no film lookups for songs");
        assert_eq!(super::pick_with("hotstar", "x", found, no_title), None);
    }

    /// Reaches apis.justwatch.com, so it only runs when asked for.
    #[test]
    #[ignore]
    fn finds_a_real_title() {
        let link = title_link(&catalog("US", "Stranger Things").expect("an answer"), "netflix", "Stranger Things");
        assert!(link.expect("a link").starts_with("https://www.netflix.com/"));
    }

    #[test]
    fn youtube_starts_the_video_it_finds() {
        let pick = pick_with("YouTube", "Ithinno Aadame", found).unwrap();
        assert_eq!(pick.link(), "https://www.youtube.com/watch?v=pMpTUqJMnzM");
        assert!(matches!(&pick, Pick::Video { id, music: false, .. } if id == "pMpTUqJMnzM"));
        let music = pick_with("youtube music", "x", found).unwrap();
        assert_eq!(music.link(), "https://music.youtube.com/watch?v=pMpTUqJMnzM");
    }

    #[test]
    fn youtube_out_of_reach_falls_back_to_a_search() {
        let pick = pick_with("youtube", "Ithinno Aadame", |_| None).unwrap();
        assert_eq!(pick.link(), "https://www.youtube.com/results?search_query=Ithinno%20Aadame");
        assert!(pick.said("it").contains("nothing plays"), "a search isn't playing");
        let music = pick_with("youtube music", "a", |_| None).unwrap();
        assert_eq!(music.link(), "https://music.youtube.com/search?q=a");
    }

    #[test]
    fn other_apps_search_or_only_open() {
        assert_eq!(pick_with("Netflix", "Dark", found).unwrap().link(), "https://www.netflix.com/search?q=Dark");
        assert_eq!(pick_with("spotify", "a b", found).unwrap().link(), "spotify:search:a%20b");
        assert_eq!(pick_with("in.startv.hotstar", "x", found), None);
    }

    #[test]
    fn nothing_said_reaches_past_the_query() {
        let link = pick_with("netflix", "a&b=c#d/../e?f", found).unwrap();
        assert_eq!(link.link(), "https://www.netflix.com/search?q=a%26b%3Dc%23d%2F..%2Fe%3Ff");
        assert_eq!(encode("ഇത്"), "%E0%B4%87%E0%B4%A4%E0%B5%8D");
    }

    #[test]
    fn only_a_video_id_comes_off_a_page() {
        let page = |id: &str| format!("x{VIDEO}{id}\",\"thumbnail\":{{}}}}{VIDEO}second00000\"");
        assert_eq!(first_video(&page("pMpTUqJ-n_M")).as_deref(), Some("pMpTUqJ-n_M"));
        assert_eq!(first_video(&page("short")), None);
        assert_eq!(first_video(&page("../../etc/x")), None);
        assert_eq!(first_video("<html>a consent page</html>"), None);
    }

    /// Reaches youtube.com, so it only runs when asked for.
    #[test]
    #[ignore]
    fn finds_a_real_video() {
        let id = first_video(&youtube_results("Ithinno Aadame").expect("a page")).expect("a video");
        println!("{id}");
    }

    #[test]
    fn lookups_off_keeps_the_title_home() {
        let off = Finder::new(None);
        assert!(matches!(off.find("youtube", "x"), Some(Pick::Search { .. })));
        assert!(!off.missed());
    }

    #[test]
    fn a_page_with_no_video_is_noticed() {
        let lost = Finder::with(|_| Some("<html>something new</html>".into()));
        assert!(matches!(lost.find("youtube", "x"), Some(Pick::Search { .. })));
        assert!(lost.missed());
        let unreachable = Finder::with(|_| None);
        unreachable.find("youtube", "x");
        assert!(!unreachable.missed(), "being offline isn't YouTube changing");
    }

    #[test]
    fn amazon_music_is_not_prime_video() {
        assert_eq!(pick_with("amazon music", "x", found), None);
        assert!(pick_with("amazon", "x", found).is_some());
    }

    #[test]
    fn replies_never_claim_more_than_happened() {
        assert!(only_opened("Hotstar", "x").contains("nothing is playing"));
        let video = pick_with("youtube", "x", found).unwrap();
        assert!(video.said("x").contains("top YouTube match"));
    }
}
