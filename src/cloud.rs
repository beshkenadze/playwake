use crate::{journal, session_store, xbox::{self, Resolution, Result, Streaming, Xbox}};
use reqwest::{Method, Url, blocking::RequestBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::{HashMap, HashSet, VecDeque}, fs, io::Read, sync::Mutex};

const LOGIN_ROUTES: [(&str, &str, &str); 3] = [
    ("https://xgpuweb.gssv-play-prod.xboxlive.com/v2/login/user", "xgpuweb", "AFD"),
    ("https://atm.gssv-play-prod.xboxlive.com/v2/login/user", "xgpuweb", "ATM"),
    ("https://xgpuwebf2p.gssv-play-prod.xboxlive.com/v2/login/user", "xgpuwebf2p", "AFD"),
];
const APP_VERSION: &str = "29.19.17";
const SDK_VERSION: &str = "10.6.57";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/148.0.0.0 Safari/537.36";

#[derive(Clone, Deserialize, Serialize)]
pub struct Game {
    pub id: String,
    pub name: String,
    /// A portrait cover from the Microsoft Store.
    #[serde(default)]
    pub cover: Option<String>,
}

/// The directory of the covers in the cover cache.
pub static COVERS: crate::covers::Set = crate::covers::Set::new("cloud");
/// Pixels: the library's cover is 64 units wide, and a unit is at most about
/// 1.5 pixels on the handhelds. The app keeps every decoded cover in memory.
const COVER_WIDTH: u64 = 144;
const CACHED_ACCOUNTS: usize = 4;
const CACHED_GAMES: usize = 4096;
const CACHE_BYTES: u64 = 8 * 1024 * 1024;
static CACHE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Deserialize, Serialize)]
struct CacheEntry {
    account: String,
    games: Vec<Game>,
}

#[derive(Default, Deserialize, Serialize)]
struct CatalogCache(VecDeque<CacheEntry>);

impl CatalogCache {
    fn get(&mut self, account: &str) -> Option<Vec<Game>> {
        let index = self.0.iter().position(|entry| entry.account == account)?;
        let entry = self.0.remove(index)?;
        let games = entry.games.clone();
        self.0.push_front(entry);
        Some(games)
    }

    fn put(&mut self, account: &str, games: Vec<Game>) {
        self.0.retain(|entry| entry.account != account);
        self.0.push_front(CacheEntry { account: account.to_owned(), games });
        self.0.truncate(CACHED_ACCOUNTS);
    }
}

fn with_cache<T>(update: impl FnOnce(&mut CatalogCache) -> Option<T>) -> Option<T> {
    let _guard = CACHE_LOCK.lock().ok()?;
    let path = session_store::path().ok()?.with_extension("cloud-catalog.json");
    let mut cache: CatalogCache = fs::File::open(&path).ok()
        .filter(|file| file.metadata().is_ok_and(|metadata| metadata.len() <= CACHE_BYTES))
        .and_then(|file| serde_json::from_reader(file.take(CACHE_BYTES)).ok())
        .unwrap_or_default();
    cache.0.truncate(CACHED_ACCOUNTS);
    cache.0.retain(|entry| entry.games.len() <= CACHED_GAMES);
    let value = update(&mut cache)?;
    let _ = session_store::write_json(path, &cache);
    Some(value)
}

pub fn cached_catalog(account: &Xbox) -> Option<Vec<Game>> {
    let games = with_cache(|cache| cache.get(&account.cloud_cache_key))?;
    journal::record("cloud_catalog_cache_hit", json!({"games":games.len()}));
    Some(games)
}

pub(super) fn service_url(raw: &str) -> Result<Url> {
    let value = if raw.contains("://") { raw.to_owned() } else { format!("https://{raw}") };
    let url = Url::parse(&value).map_err(|_| "Invalid Xbox Cloud server")?;
    if url.scheme() == "https" && url.host_str().is_some_and(|host| host.ends_with(".xboxlive.com"))
        && url.username().is_empty() && url.password().is_none() && url.port().is_none()
        && url.path() == "/" && url.query().is_none() && url.fragment().is_none() {
        Ok(url)
    } else {
        Err("Invalid Xbox Cloud server".into())
    }
}

pub(super) fn headers(request: RequestBuilder) -> RequestBuilder {
    request.header("User-Agent", USER_AGENT).header("Origin", "https://www.xbox.com")
        .header("Referer", "https://www.xbox.com/play").header("x-gssv-client", "XboxComBrowser")
        .header("Accept", "application/json")
}

pub(super) fn correlation_vector() -> String {
    let random = uuid::Uuid::new_v4().simple().to_string();
    format!("{}.0", &random[..22])
}

pub(super) fn stun_url(raw: &str) -> Option<String> {
    if raw.len() > 256 { return None; }
    let address = raw.strip_prefix("stun://").or_else(|| raw.strip_prefix("stun:")).unwrap_or(raw);
    let url = Url::parse(&format!("stun://{address}")).ok()?;
    (url.has_host() && url.username().is_empty() && url.password().is_none()
        && url.query().is_none() && url.fragment().is_none() && url.path().is_empty()
        && url.port() != Some(0)).then(|| url.to_string())
}

pub(super) fn ice_offer(candidates: &Value) -> Result<Value> {
    let candidates = candidates.as_array().filter(|list| !list.is_empty() && list.len() <= 64)
        .ok_or("GStreamer supplied no usable ICE candidates")?;
    let mut encoded: Vec<_> = candidates.iter().map(Value::to_string).collect();
    encoded.push("a=end-of-candidates".into());
    Ok(json!({"candidates":encoded}))
}

pub(super) fn decode_ice(candidates: &mut [Value]) -> Result<()> {
    if candidates.len() > 64 { return Err("Xbox Cloud returned too many ICE candidates".into()); }
    for value in candidates.iter_mut() {
        if let Some(text) = value.as_str() {
            *value = if text == "a=end-of-candidates" {
                json!({"candidate":text})
            } else {
                serde_json::from_str(text).map_err(|_| "Invalid Xbox Cloud ICE candidate")?
            };
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_transfer_and_session_paths_are_bounded() {
        assert!(service_url("uks.core.gssv-play-prod.xboxlive.com").is_ok());
        assert!(service_url("https://xboxlive.com.example.org").is_err());
        assert!(service_url("https://uks.core.gssv-play-prod.xboxlive.com/path").is_err());
        assert!(stun_url("stun:stun.example.org:3478").is_some());
        assert!(stun_url("stun:credentials@stun.example.org").is_none());
        assert!(xbox::session_path(&json!({"sessionPath":"/v5/sessions/home/session"}), "cloud", "test").is_err());
    }

    #[test]
    fn cloud_ice_uses_string_candidates_and_accepts_both_reply_shapes() {
        let candidate = json!({"candidate":"candidate:1","sdpMLineIndex":0});
        let payload = ice_offer(&json!([candidate])).unwrap();
        assert_eq!(payload["candidates"][0], candidate.to_string());
        assert_eq!(payload["candidates"][1], "a=end-of-candidates");
        let mut reply = [candidate.clone(), json!(candidate.to_string()), json!("a=end-of-candidates")];
        decode_ice(&mut reply).unwrap();
        assert_eq!(reply[0], reply[1]);
        assert_eq!(reply[2]["candidate"], "a=end-of-candidates");
    }

    #[test]
    fn the_cover_is_the_poster_or_else_the_box_art_from_the_store() {
        let image = |purpose: &str, uri: &str, width: u32, height: u32|
            json!({"ImagePurpose":purpose,"Uri":uri,"Width":width,"Height":height});
        let poster = image("Poster", "//store-images.s-microsoft.com/image/apps.1.poster", 1440, 2160);
        let box_art = image("BoxArt", "//store-images.s-microsoft.com/image/apps.1.box", 1080, 1080);
        let product = |images: Vec<Value>| json!({"LocalizedProperties":[{"Images":images}]});
        assert_eq!(cover(&product(vec![box_art.clone(), poster])).as_deref(),
            Some("https://store-images.s-microsoft.com/image/apps.1.poster?w=144&h=216"));
        assert_eq!(cover(&product(vec![image("Logo", "//store-images.s-microsoft.com/image/apps.1.logo", 300, 300), box_art])).as_deref(),
            Some("https://store-images.s-microsoft.com/image/apps.1.box?w=144&h=144"));
        assert!(cover(&product(vec![image("Poster", "//images.example.org/apps.1.poster", 1440, 2160)])).is_none());
        assert!(cover(&product(vec![image("Poster", "http://store-images.s-microsoft.com/image/a", 1440, 2160)])).is_none());
        assert!(cover(&product(vec![])).is_none());
    }

    #[test]
    fn only_the_details_that_name_a_game_can_fail_the_catalog() {
        let game = |product: &str, title: Option<&str>| (product.to_owned(), title.map(str::to_owned), Some(product.to_owned()));
        let games = [game("named", Some("Halo")), game("unnamed", None)];
        let reply = |ids: &[&str]| json!({"Products": ids.iter().map(|id| json!({"ProductId":id,
            "LocalizedProperties":[{"ProductTitle":format!("Title {id}")}]})).collect::<Vec<_>>()});
        let found = details(&games, |ids| if ids.contains(&"named") { Err("timeout".into()) } else { Ok(reply(ids)) }).unwrap();
        assert_eq!(found["unnamed"].title.as_deref(), Some("Title unnamed"));
        assert!(!found.contains_key("named"));
        assert!(details(&games, |ids| if ids.contains(&"unnamed") { Err("timeout".into()) } else { Ok(reply(ids)) }).is_err());
    }

    #[test]
    fn a_catalog_cached_before_covers_still_loads() {
        let cache: CatalogCache = serde_json::from_value(json!([{"account":"a","games":[{"id":"1","name":"Halo"}]}])).unwrap();
        assert!(cache.0[0].games[0].cover.is_none());
    }

    #[test]
    fn catalog_cache_promotes_a_hit_and_evicts_the_oldest_account() {
        let mut cache = CatalogCache::default();
        for account in ["a", "b", "c", "d"] {
            cache.put(account, vec![Game { id: account.into(), name: account.into(), cover: None }]);
        }
        assert_eq!(cache.get("a").unwrap()[0].name, "a");
        cache.put("e", vec![]);
        assert!(cache.get("b").is_none());
        assert_eq!(cache.get("a").unwrap()[0].id, "a");
        cache.put("a", vec![]);
        assert_eq!(cache.0.len(), CACHED_ACCOUNTS);
        assert!(cache.get("a").unwrap().is_empty());
    }
}

fn safe_id(value: &Value) -> Option<String> {
    let id = value.as_str().map(str::to_owned).or_else(|| value.as_u64().map(|n| n.to_string()))?;
    (id.len() <= 128 && !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')).then_some(id)
}

fn name(value: &Value) -> Option<String> {
    let title = value.as_str()?.trim();
    (!title.is_empty() && title.len() <= 300 && !title.chars().any(char::is_control)).then(|| title.to_owned())
}

pub fn catalog(account: &Xbox) -> Result<Vec<Game>> {
    let (service, market) = connect(account)?;
    let games = load_catalog(account, service, &market)?;
    with_cache(|cache| { cache.put(&account.cloud_cache_key, games.clone()); Some(()) });
    Ok(games)
}

pub fn connect(account: &Xbox) -> Result<(Streaming, String)> {
    let (login, offering, routing) = login(account)?;
    let market = login.get("market").and_then(Value::as_str)
        .filter(|value| value.len() == 2 && value.bytes().all(|b| b.is_ascii_alphabetic()))
        .ok_or("Xbox Cloud did not return a catalog market")?.to_owned();
    let service = account.streaming(login, Some((market.clone(), routing)))?;
    journal::record("cloud_login", json!({"success":true,"offering":offering,"routing":routing}));
    Ok((service, market))
}

fn login(account: &Xbox) -> Result<(Value, &'static str, &'static str)> {
    let stage = "Xbox Cloud login";
    let mut last_error = String::new();
    for (url, offering, routing) in LOGIN_ROUTES {
        let request = headers(account.client.post(url)).header("X-GSSV-Routing", routing)
            .json(&json!({"token":account.xsts,"offeringId":offering}));
        match xbox::send(request, stage).and_then(|response| xbox::read(response, stage)) {
            Ok(value) => return Ok((value, offering, routing)),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

fn load_catalog(account: &Xbox, service: Streaming, market: &str) -> Result<Vec<Game>> {
    let mut games = Vec::new();
    let mut seen = HashSet::new();
    let mut pages = HashSet::new();
    let mut continuation = None::<String>;
    let stage = "Xbox Cloud catalog";
    let mut complete = false;
    let mut offered = 0;
    for _ in 0..32 {
        let mut request = service.request(Method::GET, "/v2/titles")?.query(&[("mr", market)]);
        if let Some(token) = &continuation { request = request.query(&[("ct", token)]); }
        let reply = xbox::read(xbox::send(request, stage)?, stage)?;
        let results = reply.get("results").and_then(Value::as_array).ok_or("Invalid Xbox Cloud catalog")?;
        offered += results.len();
        for row in results {
            let details = &row["details"];
            if details["hasEntitlement"].as_bool() != Some(true)
                || details["remainingGameplayTimeInSeconds"].as_f64().is_some_and(|time| time <= 0.0) { continue; }
            let Some(id) = safe_id(&row["titleId"]).or_else(|| safe_id(&details["titleId"])) else { continue; };
            let product = safe_id(&details["productId"]).or_else(|| safe_id(&row["productId"]));
            let title = [&details["name"], &details["title"], &row["name"], &row["title"]]
                .into_iter().find_map(name);
            if (title.is_some() || product.is_some()) && seen.insert(id.clone()) {
                if games.len() >= CACHED_GAMES { return Err("Xbox Cloud catalog is too large".into()); }
                games.push((id, title, product));
            }
        }
        continuation = ["continuationToken", "continuation", "nextContinuationToken"].into_iter()
            .find_map(|key| reply[key].as_str().or_else(|| reply[key]["token"].as_str()))
            .filter(|value| !value.trim().is_empty()).map(str::to_owned);
        let Some(token) = &continuation else { complete = true; break; };
        if token.len() > 4096 || !pages.insert(token.clone()) { return Err("Invalid Xbox Cloud catalog pagination".into()); }
    }
    let metadata = details(&games, |batch| {
        let request = account.client.get("https://displaycatalog.mp.microsoft.com/v7.0/products")
            .query(&[("bigIds", batch.join(",").as_str()), ("market", market), ("languages", "en-US")]);
        let stage = "Xbox Cloud game details";
        xbox::read(xbox::send(request, stage)?, stage)
    })?;
    let mut games: Vec<_> = games.into_iter().filter_map(|(id, name, product)| {
        let found = product.as_deref().and_then(|product| metadata.get(product));
        let name = name.or_else(|| found?.title.clone())?;
        Some(Game { id, name, cover: found.and_then(|found| found.cover.clone()) })
    }).collect();
    games.sort_by_cached_key(|game| game.name.to_lowercase());
    journal::record("cloud_catalog_loaded", json!({"offered":offered,"named_playable":games.len(),"complete":complete}));
    if !complete { return Err("Xbox Cloud catalog exceeded its page limit".into()); }
    Ok(games)
}

struct Product {
    title: Option<String>,
    cover: Option<String>,
}

/// The store details of the catalog's products. The games without a title
/// need theirs, so failing to get it fails the catalog; the covers of the
/// other games are optional.
fn details(games: &[(String, Option<String>, Option<String>)], mut fetch: impl FnMut(&[&str]) -> Result<Value>)
    -> Result<HashMap<String, Product>>
{
    let ids = |named: bool| games.iter().filter(move |(_, title, _)| title.is_some() == named)
        .filter_map(|(_, _, product)| product.as_deref()).collect::<Vec<_>>();
    let mut found = products(&ids(false), &mut fetch)?;
    match products(&ids(true), &mut fetch) {
        Ok(more) => found.extend(more),
        Err(error) => journal::record("cloud_covers_failed", json!({"error":error})),
    }
    Ok(found)
}

fn products(ids: &[&str], fetch: &mut impl FnMut(&[&str]) -> Result<Value>) -> Result<HashMap<String, Product>> {
    let mut found = HashMap::new();
    for batch in ids.chunks(40) {
        let reply = fetch(batch)?;
        let products = reply["Products"].as_array().ok_or("Invalid Xbox Cloud game details response")?;
        for item in products {
            let Some(id) = safe_id(&item["ProductId"]) else { continue };
            let title = item["LocalizedProperties"].as_array().into_iter().flatten().find_map(|item| name(&item["ProductTitle"]));
            found.insert(id, Product { title, cover: cover(item) });
        }
    }
    Ok(found)
}

/// The store's portrait poster, or else its square box art, sized for a card.
fn cover(product: &Value) -> Option<String> {
    let images: Vec<&Value> = product["LocalizedProperties"].as_array().into_iter().flatten()
        .flat_map(|item| item["Images"].as_array().into_iter().flatten()).collect();
    ["Poster", "BoxArt"].into_iter().find_map(|purpose| {
        let image = images.iter().find(|image| image["ImagePurpose"] == purpose)?;
        let raw = image["Uri"].as_str().filter(|uri| uri.len() <= 512)?;
        let mut url = Url::parse(&raw.strip_prefix("//").map_or_else(|| raw.to_owned(), |rest| format!("https://{rest}"))).ok()?;
        if url.scheme() != "https" || url.host_str() != Some("store-images.s-microsoft.com") { return None; }
        let width = image["Width"].as_u64().filter(|width| *width > 0)?;
        let height = image["Height"].as_u64()?;
        url.set_query(Some(&format!("w={COVER_WIDTH}&h={}", COVER_WIDTH * height / width)));
        Some(url.to_string())
    })
}

pub fn reserve(service: &Streaming, game: &Game, resolution: Resolution) -> Result<String> {
    let (width, height) = resolution.dimensions();
    let device = json!({
        "appInfo":{"env":{"clientAppId":"www.xbox.com","clientAppType":"browser","clientAppVersion":APP_VERSION,
                          "clientSdkVersion":SDK_VERSION,"httpEnvironment":"prod","sdkInstallId":""}},
        "dev":{"hw":{"make":"Apple","model":"unknown","platformType":"desktop","sdkType":"web"},
               "os":{"name":"macOS","ver":"10.15.7","platform":"desktop"},
               "browser":{"browserName":"chrome","browserVersion":"148.0.0.0"},
               "displayInfo":{"dimensions":{"widthInPixels":width,"heightInPixels":height},"pixelDensity":{"dpiX":1,"dpiY":1}}}
    });
    let client_session = uuid::Uuid::new_v4().simple().to_string();
    let stage = "Start Xbox Cloud Gaming";
    let request = service.request(Method::POST, "/v5/sessions/cloud/play")?
        .header("X-MS-Device-Info", device.to_string())
        .json(&json!({
            "titleId":game.id,"serverId":"","systemUpdateGroup":"","clientSessionId":&client_session[..22],
            "fallbackRegionNames":[],
            "settings":{"nanoVersion":"V3;WebrtcTransport.dll","enableTextToSpeech":false,"highContrast":0,
                        "locale":"en-US","useIceConnection":false,"timezoneOffsetMinutes":0,"sdkType":"web","osName":"macOS"}
        }));
    xbox::session_path(&xbox::read(xbox::send(request, stage)?, stage)?, "cloud", stage)
}
