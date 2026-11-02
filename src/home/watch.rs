//! What "play this in that app" becomes, whatever the TV.
//!
//! Only YouTube can be started on a title from here, because a video's id
//! is all it takes and its results page gives that away. The other apps
//! take a search link at best, which leaves the choosing to whoever holds
//! the remote; a reply has to say so rather than claim it's playing.
//!
//! The YouTube lookup is the one thing home sends off the network: the
//! title being asked for, to youtube.com, when someone asks for it.

use std::time::Duration;

const LOOKUP: Duration = Duration::from_secs(6);
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

/// Where a request for a title ended up.
#[derive(Debug, PartialEq)]
pub enum Pick {
    /// A video YouTube will start by itself, by id and by link.
    Video { id: String, link: String, music: bool },
    /// A link to the app's search for it, with the app's name.
    Search { link: String, app: &'static str },
}

impl Pick {
    pub fn link(&self) -> &str {
        match self {
            Pick::Video { link, .. } | Pick::Search { link, .. } => link,
        }
    }

    /// What to tell the model, which repeats it to the household.
    pub fn said(&self, what: &str) -> String {
        match self {
            Pick::Video { music: true, .. } => format!("sent the top YouTube match for {what} to YouTube Music"),
            Pick::Video { .. } => format!("sent the top YouTube match for {what} to YouTube"),
            Pick::Search { app, .. } => {
                format!("{app} was sent a search for {what}; nothing plays until someone picks a result")
            }
        }
    }
}

/// What to say when `app` can only be opened.
pub fn only_opened(app: &str, what: &str) -> String {
    format!("opened {app}, but can't tell it to play {what}; nothing is playing yet, so someone has to find it there")
}

/// Where to go for `what` in `app`, or `None` for an app that only opens.
pub fn pick(app: &str, what: &str) -> Option<Pick> {
    pick_with(app, what, youtube_video)
}

fn pick_with(app: &str, what: &str, video: impl Fn(&str) -> Option<String>) -> Option<Pick> {
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
    SEARCHES
        .iter()
        .find(|(name, ..)| spoken.contains(name))
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

/// The top video YouTube has for `what`, if it can be reached.
fn youtube_video(what: &str) -> Option<String> {
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::NativeTls)
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    let agent: ureq::Agent = ureq::Agent::config_builder().tls_config(tls).timeout_global(Some(LOOKUP)).build().new_agent();
    let url = format!("https://www.youtube.com/results?search_query={}&sp={VIDEOS_ONLY}", encode(what));
    let mut page = agent
        .get(url)
        .header("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Chrome/130.0 Safari/537.36")
        .header("Accept-Language", "en")
        // Skips the consent page some countries get first.
        .header("Cookie", "SOCS=CAI")
        .call()
        .ok()?;
    first_video(&page.body_mut().read_to_string().ok()?)
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
        let id = youtube_video("Ithinno Aadame").expect("a video");
        println!("{id}");
    }

    #[test]
    fn replies_never_claim_more_than_happened() {
        assert!(only_opened("Hotstar", "x").contains("nothing is playing"));
        let video = pick_with("youtube", "x", found).unwrap();
        assert!(video.said("x").contains("top YouTube match"));
    }
}
