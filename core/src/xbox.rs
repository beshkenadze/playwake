use crate::{journal, session_store};
use reqwest::{Method, Url, blocking::{Client, RequestBuilder, Response}, redirect::Policy};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{io::Read, sync::{Mutex, atomic::{AtomicBool, Ordering}}, thread, time::{Duration, Instant}};

pub type Result<T> = std::result::Result<T, String>;

const CLIENT_ID: &str = "1f907974-e22b-4810-a9de-d9647380c97e";
const OAUTH: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0";
const SCOPE: &str = "xboxlive.signin openid profile offline_access";
const TRANSFER_SCOPE: &str = "service::http://Passport.NET/purpose::PURPOSE_XBOX_CLOUD_CONSOLE_TRANSFER_TOKEN";

#[derive(Clone, Copy, Default, PartialEq)]
pub enum Resolution {
    P720,
    #[default]
    P1080,
    P1440,
}

impl Resolution {
    pub const CHOICES: [(Self, &'static str); 3] = [
        (Self::P720, "720p"),
        (Self::P1080, "1080p"), (Self::P1440, "1440p"),
    ];

    pub fn dimensions(self) -> (u32, u32) {
        match self {
            Self::P720 => (1280, 720),
            Self::P1080 => (1920, 1080),
            Self::P1440 => (2560, 1440),
        }
    }
}

#[derive(Clone, Deserialize)]
pub struct Console {
    #[serde(rename = "serverId")]
    pub id: String,
    #[serde(rename = "deviceName", default)]
    pub name: String,
    #[serde(rename = "consoleType", default)]
    pub model: String,
    #[serde(rename = "powerState", default)]
    pub power: String,
}

#[derive(Deserialize)]
pub struct DeviceCode {
    pub user_code: String,
    verification_uri: Url,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    device_code: String,
    expires_in: u64,
    #[serde(default = "poll_interval")]
    interval: u64,
}

impl DeviceCode {
    pub fn verification_link(&self) -> (Url, bool) {
        if let Some(url) = self.verification_uri_complete.as_deref()
            .and_then(|value| Url::parse(value).ok())
            .filter(microsoft_url)
        {
            return (url, true);
        }

        let mut url = self.verification_uri.clone();
        // The Xbox consumer flow returns Microsoft Link, which accepts the OTC
        // query parameter. Leave other Microsoft verification pages untouched.
        let prefilled = matches!(url.host_str(), Some("microsoft.com" | "www.microsoft.com"))
            && matches!(url.path(), "/link" | "/link/")
            && url.query().is_none();
        if prefilled {
            url.query_pairs_mut().append_pair("otc", &self.user_code);
        }
        (url, prefilled)
    }
}

fn poll_interval() -> u64 { 5 }

#[derive(Deserialize)]
struct OAuthToken {
    access_token: Option<String>,
    refresh_token: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginResponse {
    gs_token: String,
    offering_settings: Offering,
}

#[derive(Deserialize)]
struct Offering { regions: Vec<Region> }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Region { base_uri: String, #[serde(default)] is_default: bool }

pub struct Xbox {
    pub(super) client: Client,
    pub(super) xsts: String,
    pub(super) cloud_cache_key: String,
    refresh_token: String,
    client_id: String,
    home: Mutex<Option<Streaming>>,
}

#[derive(Clone)]
pub struct Streaming {
    client: Client,
    base: Url,
    gs_token: String,
    cloud_route: Option<(String, &'static str)>,
    refresh_token: String,
    client_id: String,
}

pub struct SessionConfig {
    pub heartbeat: Duration,
    pub stun: Option<String>,
}

pub fn client() -> Result<Client> {
    Client::builder().timeout(Duration::from_secs(12)).redirect(Policy::none()).build()
        .map_err(|_| "Could not initialize the HTTPS client".to_string())
}

pub fn read(response: Response, stage: &str) -> Result<Value> {
    if !response.status().is_success() {
        let status = response.status().as_u16();
        journal::record("http_error", json!({"stage": stage, "status": status}));
        return Err(format!("{stage}: HTTP {status}"));
    }
    if response.status().as_u16() == 204 || response.content_length() == Some(0) { return Ok(Value::Null); }
    const LIMIT: usize = 8 * 1024 * 1024;
    let mut bytes = Vec::new();
    response.take(LIMIT as u64 + 1).read_to_end(&mut bytes)
        .map_err(|_| format!("{stage}: incomplete response"))?;
    if bytes.len() > LIMIT { return Err(format!("{stage}: response too large")); }
    if bytes.is_empty() { return Ok(Value::Null); }
    serde_json::from_slice(&bytes).map_err(|_| format!("{stage}: invalid response"))
}

pub(super) fn send(request: RequestBuilder, stage: &str) -> Result<Response> {
    request.send().map_err(|error| {
        let reason = if error.is_timeout() { "timed out" } else { "network request failed" };
        journal::record("network_error", json!({"stage": stage, "reason": reason}));
        format!("{stage}: {reason}")
    })
}

pub(super) fn field(value: &Value, key: &str, stage: &str) -> Result<String> {
    value.get(key).and_then(Value::as_str).filter(|text| !text.is_empty()).map(str::to_owned)
        .ok_or_else(|| format!("{stage}: missing {key}"))
}

fn microsoft_url(url: &Url) -> bool {
    url.scheme() == "https"
        && matches!(url.host_str(), Some("microsoft.com" | "www.microsoft.com" | "login.microsoftonline.com" | "login.live.com"))
        && url.username().is_empty() && url.password().is_none() && url.port().is_none()
}

fn xbox_region(url: &str) -> Result<Url> {
    let url = Url::parse(url).map_err(|_| "xHome returned an invalid region")?;
    let trusted = url.scheme() == "https"
        && url.host_str().is_some_and(|h| h.ends_with(".xboxlive.com") && h.contains("gssv-play-prod"))
        && url.username().is_empty() && url.password().is_none() && url.port().is_none();
    if trusted { Ok(url) } else { Err("xHome returned an untrusted region".into()) }
}

pub fn begin_sign_in(client: &Client, client_id: &str) -> Result<DeviceCode> {
    let stage = "Microsoft device sign-in";
    let response = send(client.post(format!("{OAUTH}/devicecode"))
        .form(&[("client_id", client_id), ("scope", SCOPE)]), stage)?;
    let device: DeviceCode = serde_json::from_value(read(response, stage)?)
        .map_err(|_| "Microsoft returned an invalid device challenge".to_string())?;
    if !microsoft_url(&device.verification_uri) || device.user_code.is_empty() {
        return Err("Microsoft returned an invalid verification URL".into());
    }
    Ok(device)
}

pub fn public_client_id() -> String {
    std::env::var("XBOX_CLIENT_ID").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| CLIENT_ID.into())
}

pub fn finish_sign_in(client: Client, client_id: String, device: DeviceCode, cancel: &AtomicBool) -> Result<Xbox> {
    let stage = "Microsoft sign-in";
    let deadline = Instant::now() + Duration::from_secs(device.expires_in.min(1800));
    let mut interval = device.interval.max(5);
    let response = loop {
        let next = Instant::now() + Duration::from_secs(interval);
        while Instant::now() < next {
            if cancel.load(Ordering::Relaxed) { return Err("Sign-in cancelled".into()); }
            if Instant::now() >= deadline { return Err("Microsoft code expired".into()); }
            thread::sleep(Duration::from_millis(150));
        }
        let raw = send(client.post(format!("{OAUTH}/token")).form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", &client_id), ("device_code", &device.device_code),
        ]), stage)?;
        let status = raw.status();
        let response = raw.json::<OAuthToken>().map_err(|_| format!("{stage}: HTTP {}", status.as_u16()))?;
        if status.is_success() {
            journal::record("microsoft_confirmation_completed", json!({}));
            break response;
        }
        match response.error.as_deref() {
            Some("authorization_pending") => {},
            Some("slow_down") => interval += 5,
            Some("authorization_declined" | "access_denied") => return Err("Microsoft sign-in was declined".into()),
            Some("expired_token") => return Err("Microsoft code expired".into()),
            _ => {
                journal::record("http_error", json!({"stage": stage, "status": status.as_u16()}));
                return Err(format!("{stage}: HTTP {}", status.as_u16()));
            }
        }
    };
    let access = response.access_token.ok_or("Microsoft returned no access token")?;
    let refresh_token = response.refresh_token.ok_or("Microsoft returned no refresh token")?;
    let catalog_key = uuid::Uuid::new_v4().simple().to_string();
    save_session(&client_id, &refresh_token, &catalog_key);
    xbox_login(client, client_id, access, refresh_token, catalog_key)
}

fn save_session(client_id: &str, refresh_token: &str, catalog_key: &str) {
    journal::record("session_saved", json!({"success": session_store::save(client_id, refresh_token, catalog_key).is_ok()}));
}

pub fn restore_sign_in() -> Result<Option<Xbox>> {
    let client_id = public_client_id();
    let Some(previous) = session_store::load(&client_id).map_err(|_| "Could not read saved Microsoft sign-in")? else {
        return Ok(None);
    };
    journal::record("session_restore_started", json!({}));
    let client = client()?;
    let stage = "Microsoft refresh";
    let raw = send(client.post(format!("{OAUTH}/token")).form(&[
        ("grant_type", "refresh_token"), ("client_id", &client_id), ("refresh_token", &previous.refresh_token), ("scope", SCOPE),
    ]), stage)?;
    let status = raw.status();
    let response = raw.json::<OAuthToken>().map_err(|_| format!("{stage}: invalid response"))?;
    if !status.is_success() {
        if matches!(response.error.as_deref(), Some("invalid_grant" | "interaction_required")) {
            session_store::clear().map_err(|_| "Could not remove expired Microsoft sign-in")?;
            journal::record("session_expired", json!({}));
            return Ok(None);
        }
        journal::record("http_error", json!({"stage": stage, "status": status.as_u16()}));
        return Err(format!("{stage}: HTTP {}", status.as_u16()));
    }
    let access = response.access_token.ok_or("Microsoft returned no access token")?;
    let catalog_key = previous.catalog_key.clone().filter(|key| uuid::Uuid::parse_str(key).is_ok())
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let updated = response.refresh_token.is_some() || previous.catalog_key.as_deref() != Some(&catalog_key);
    let refresh_token = response.refresh_token.unwrap_or(previous.refresh_token);
    if updated { save_session(&client_id, &refresh_token, &catalog_key); }
    let xbox = xbox_login(client, client_id, access, refresh_token, catalog_key)?;
    journal::record("session_restored", json!({}));
    Ok(Some(xbox))
}

fn xbox_login(client: Client, client_id: String, access: String, refresh_token: String, cloud_cache_key: String) -> Result<Xbox> {
    let stage = "Xbox user authentication";
    let user = read(send(client.post("https://user.auth.xboxlive.com/user/authenticate")
        .header("x-xbl-contract-version", "1").json(&json!({
            "Properties":{"AuthMethod":"RPS","RpsTicket":format!("d={access}"),"SiteName":"user.auth.xboxlive.com"},
            "RelyingParty":"http://auth.xboxlive.com","TokenType":"JWT"
        })), stage)?, stage)?;
    let token = field(&user, "Token", stage)?;

    let stage = "Xbox streaming authorization";
    let xsts = read(send(client.post("https://xsts.auth.xboxlive.com/xsts/authorize")
        .header("x-xbl-contract-version", "1").header("Origin", "https://www.xbox.com")
        .header("Referer", "https://www.xbox.com/").header("ms-cv", "0")
        .json(&json!({"Properties":{"SandboxId":"RETAIL","UserTokens":[token]},
            "RelyingParty":"http://gssv.xboxlive.com/","TokenType":"JWT"})), stage)?, stage)?;
    let xsts = field(&xsts, "Token", stage)?;
    Ok(Xbox { client, xsts, cloud_cache_key, refresh_token, client_id, home: Mutex::new(None) })
}

impl Xbox {
    pub fn home(&self) -> Result<Streaming> {
        let mut cached = self.home.lock().map_err(|_| "Xbox account unavailable")?;
        if let Some(service) = cached.as_ref() { return Ok(service.clone()); }
        let stage = "xHome login";
        let reply = read(send(self.client.post("https://xhome.gssv-play-prod.xboxlive.com/v2/login/user")
            .header("x-gssv-client", "XboxComBrowser").json(&json!({"token":self.xsts,"offeringId":"xhome"})), stage)?, stage)?;
        let service = self.streaming(reply, None)?;
        *cached = Some(service.clone());
        Ok(service)
    }

    pub fn consoles(&self) -> Result<Vec<Console>> { self.home()?.consoles() }

    pub(super) fn streaming(&self, value: Value, cloud_route: Option<(String, &'static str)>) -> Result<Streaming> {
        let login: LoginResponse = serde_json::from_value(value)
            .map_err(|_| "Xbox returned an invalid streaming login response")?;
        let region = login.offering_settings.regions.iter().find(|r| r.is_default)
            .or_else(|| login.offering_settings.regions.first()).ok_or("Xbox returned no streaming region")?;
        let base = if cloud_route.is_some() { crate::cloud::service_url(&region.base_uri)? } else { xbox_region(&region.base_uri)? };
        Ok(Streaming { client: self.client.clone(), base, gs_token: login.gs_token, cloud_route,
            refresh_token: self.refresh_token.clone(), client_id: self.client_id.clone() })
    }
}

impl Streaming {
    pub(super) fn request(&self, method: Method, path: &str) -> Result<RequestBuilder> {
        let url = self.base.join(path).map_err(|_| "Invalid Xbox session path")?;
        if url.origin() != self.base.origin() { return Err("Invalid Xbox session host".into()); }
        let request = self.client.request(method, url).bearer_auth(&self.gs_token)
            .header("Content-Type", "application/json");
        Ok(match &self.cloud_route {
            Some((market, routing)) => crate::cloud::headers(request).header("X-GSSV-Routing", *routing)
                .header("x-xbl-market", market).header("ms-cv", crate::cloud::correlation_vector()),
            None => request.header("x-gssv-client", "XboxComBrowser"),
        })
    }

    pub fn consoles(&self) -> Result<Vec<Console>> {
        #[derive(Deserialize)] struct ConsoleList { results: Vec<Console> }
        let stage = "Xbox list";
        let value = read(send(self.request(Method::GET, "/v6/servers/home")?, stage)?, stage)?;
        serde_json::from_value::<ConsoleList>(value).map(|list| list.results)
            .map_err(|_| "xHome returned an invalid console list".to_string())
    }

    pub fn reserve(&self, console: &Console, resolution: Resolution) -> Result<String> {
        let (width, height) = resolution.dimensions();
        let device = json!({
          "appInfo":{"env":{"clientAppId":"Microsoft.GamingApp","clientAppType":"native",
             "clientAppVersion":"2203.1001.4.0","clientSdkVersion":"8.5.2","httpEnvironment":"prod","sdkInstallId":""}},
          "dev":{"hw":{"make":"Microsoft","model":"Surface Pro","sdktype":"native"},
             "os":{"name":"Windows 11","ver":"22631.2715","platform":"desktop"},
             "displayInfo":{"dimensions":{"widthInPixels":width,"heightInPixels":height},"pixelDensity":{"dpiX":1,"dpiY":1}}}
        });
        let stage = "Start Xbox Remote Play";
        let reply = read(send(self.request(Method::POST, "/v5/sessions/home/play")?
            .header("X-MS-Device-Info", device.to_string()).json(&json!({
               "titleId":"","systemUpdateGroup":"","clientSessionId":"","serverId":console.id,"fallbackRegionNames":[],
               "settings":{"nanoVersion":"V3;WebrtcTransport.dll","enableTextToSpeech":false,"highContrast":0,
                    "locale":"en-US","useIceConnection":false,"timezoneOffsetMinutes":0,"sdkType":"web","osName":"windows"}
            })), stage)?, stage)?;
        session_path(&reply, "home", stage)
    }

    pub fn provision(&mut self, path: &str, cancel: &AtomicBool, status: impl Fn(String)) -> Result<SessionConfig> {
        let mut sent_transfer = false;
        let stage = "Xbox session state";
        let state = format!("{path}/state");
        let cloud = self.cloud_route.is_some();
        let deadline = Instant::now() + Duration::from_secs(if cloud { 900 } else { 42 });
        let mut previous = None::<String>;
        let mut wait_status = None;
        while Instant::now() < deadline {
            if cancel.load(Ordering::Relaxed) { return Err("Stream cancelled".into()); }
            let reply = read(send(self.request(Method::GET, &state)?, stage)?, stage)?;
            if cloud && let Some(uri) = reply.get("transferUri").and_then(Value::as_str)
                .filter(|uri| !uri.is_empty()) { self.base = crate::cloud::service_url(uri)?; }
            let current = reply.get("state").and_then(Value::as_str);
            if cloud {
                let public_state = current.filter(|state| matches!(*state,
                    "WaitingForResources" | "ReadyToConnect" | "Provisioning" | "Provisioned" | "Failed" | "Ended"))
                    .unwrap_or("Unknown");
                if previous.as_deref() != Some(public_state) {
                    journal::record("cloud_session_state", json!({"state":public_state}));
                    previous = Some(public_state.to_owned());
                }
            }
            match current {
                Some("Provisioned") if cloud => {
                    let configuration = read(send(self.request(Method::GET, &format!("{path}/configuration"))?,
                        "Xbox Cloud configuration")?, "Xbox Cloud configuration")?;
                    let seconds = configuration.get("keepAlivePulseInSeconds").and_then(Value::as_u64)
                        .filter(|seconds| (5..=120).contains(seconds)).unwrap_or(30);
                    let stun = configuration.pointer("/serverDetails/stunServerAddresses")
                        .and_then(Value::as_array).into_iter().flatten().take(32)
                        .filter_map(Value::as_str).find_map(crate::cloud::stun_url);
                    return Ok(SessionConfig { heartbeat: Duration::from_secs(seconds), stun });
                }
                Some("Provisioned") => return Ok(SessionConfig { heartbeat: Duration::from_secs(30), stun: None }),
                Some("Failed" | "Ended" | "Disconnected") => return Err("Xbox could not provision the session".into()),
                Some("ReadyToConnect") if !sent_transfer => {
                    let stage = "Xbox console transfer";
                    let transfer = read(send(self.client.post("https://login.live.com/oauth20_token.srf").form(&[
                        ("client_id", self.client_id.as_str()), ("scope", TRANSFER_SCOPE),
                        ("grant_type", "refresh_token"), ("refresh_token", self.refresh_token.as_str()),
                    ]), stage)?, stage)?;
                    let token = field(&transfer, "access_token", stage)?;
                    read(send(self.request(Method::POST, &format!("{path}/connect"))?.json(&json!({"userToken":token})), stage)?, stage)?;
                    sent_transfer = true;
                }
                Some("WaitingForResources") if cloud => {
                    let eta = reply.get("estimatedTotalWaitTimeInSeconds").and_then(Value::as_u64);
                    let message = eta.map(|seconds| format!("Waiting for Xbox Cloud · about {} min", seconds.div_ceil(60).max(1)))
                        .unwrap_or_else(|| "Waiting for an Xbox Cloud server…".into());
                    if wait_status.as_ref() != Some(&message) {
                        status(message.clone());
                        wait_status = Some(message);
                    }
                }
                _ => {}
            }
            for _ in 0..if cloud { 12 } else { 5 } {
                if cancel.load(Ordering::Relaxed) { return Err("Stream cancelled".into()); }
                thread::sleep(Duration::from_millis(150));
            }
        }
        Err("Xbox provisioning timed out".into())
    }

    fn exchange(&self, path: &str, payload: Value, stage: &str) -> Result<Value> {
        read(send(self.request(Method::POST, path)?.json(&payload), stage)?, stage)?;
        for _ in 0..20 {
            let reply = read(send(self.request(Method::GET, path)?, stage)?, stage)?;
            if let Some(data) = reply.get("exchangeResponse").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                return serde_json::from_str(data).map_err(|_| format!("{stage}: invalid exchange response"));
            }
            if self.cloud_route.is_some() && let Some(response) = reply.get("exchangeResponse").filter(|v| v.is_object() || v.is_array()) {
                return Ok(response.clone());
            }
            thread::sleep(Duration::from_millis(350));
        }
        Err(format!("{stage}: timed out"))
    }

    pub fn sdp(&self, path: &str, offer: &str) -> Result<String> {
        let stage = "Xbox SDP exchange";
        let mut payload = json!({"messageType":"offer","sdp":offer,"configuration":{
            "chatConfiguration":{"bytesPerSample":2,"expectedClipDurationMs":20,
                "format":{"codec":"opus","container":"webm"},"numChannels":1,"sampleFrequencyHz":24000},
            "chat":{"minVersion":1,"maxVersion":1},"control":{"minVersion":1,"maxVersion":3},
            "input":{"minVersion":1,"maxVersion":8},"message":{"minVersion":1,"maxVersion":1}
        }});
        if self.cloud_route.is_some() {
            payload["requestId"] = json!(1);
            payload["cv"] = json!(crate::cloud::correlation_vector());
            let configuration = payload["configuration"].as_object_mut().ok_or("Invalid SDP configuration")?;
            if let Some(chat) = configuration.remove("chat") {
                configuration.insert("chatStream".into(), chat);
            }
        }
        let reply = self.exchange(&format!("{path}/sdp"), payload, stage)?;
        field(&reply, "sdp", stage)
    }

    pub fn ice(&self, path: &str, candidates: Value) -> Result<Value> {
        let cloud = self.cloud_route.is_some();
        let payload = if cloud { crate::cloud::ice_offer(&candidates)? }
            else { json!({"messageType":"iceCandidate","candidate":candidates}) };
        let mut reply = self.exchange(&format!("{path}/ice"), payload, "Xbox ICE exchange")?;
        let list = reply.as_array_mut().ok_or("Xbox ICE response was not a list")?;
        if cloud { crate::cloud::decode_ice(list)?; }
        let derived: Vec<_> = list.iter().flat_map(teredo_candidates).collect();
        list.extend(derived);
        Ok(reply)
    }

    pub fn keepalive(&self, path: &str) -> Result<()> {
        let stage = "Xbox stream keepalive";
        read(send(self.request(Method::POST, &format!("{path}/keepalive"))?.json(&json!({})), stage)?, stage).map(|_| ())
    }

    pub fn stop(&self, path: &str) {
        if let Ok(request) = self.request(Method::DELETE, path) { let _ = request.send(); }
    }
}

pub(super) fn session_path(reply: &Value, kind: &str, stage: &str) -> Result<String> {
    let path = field(reply, "sessionPath", stage)?;
    let prefix = format!("v5/sessions/{kind}/");
    let id = path.trim_start_matches('/').strip_prefix(&prefix)
        .filter(|id| !id.is_empty() && id.len() <= 256
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
        .ok_or("Xbox returned an invalid session path")?;
    Ok(format!("/{prefix}{id}"))
}

fn teredo_candidates(value: &Value) -> Vec<Value> {
    let Some(address) = value.get("candidate").and_then(Value::as_str)
        .and_then(|line| line.split_whitespace().nth(4))
        .and_then(|text| text.parse::<std::net::Ipv6Addr>().ok()) else { return vec![]; };
    let bytes = address.octets();
    if bytes[..4] != [0x20, 0x01, 0, 0] { return vec![]; }
    let ip = std::net::Ipv4Addr::new(!bytes[12], !bytes[13], !bytes[14], !bytes[15]);
    let port = !u16::from_be_bytes([bytes[10], bytes[11]]);
    [9002, port].into_iter().enumerate().map(|(index, port)| {
        json!({"candidate":format!("candidate:{} 1 UDP 1 {ip} {port} typ host", index + 10),
            "sdpMLineIndex":0,"sdpMid":"0"})
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_foreign_token_endpoints() {
        assert!(xbox_region("https://uks.core.gssv-play-prodxhome.xboxlive.com").is_ok());
        assert!(xbox_region("https://gssv-play-prod.xboxlive.com.example.net").is_err());
        assert!(!microsoft_url(&Url::parse("https://example.net/devicelogin").unwrap()));
    }
    #[test]
    fn prefills_only_supported_microsoft_verification_links() {
        let mut device = DeviceCode {
            verification_uri: Url::parse("https://www.microsoft.com/link").unwrap(),
            verification_uri_complete: Some("https://example.net/go".into()),
            user_code: "TEST+123".into(),
            device_code: String::new(),
            expires_in: 900,
            interval: 5,
        };
        let (url, prefilled) = device.verification_link();
        assert!(prefilled);
        assert_eq!(url.as_str(), "https://www.microsoft.com/link?otc=TEST%2B123");

        device.verification_uri = Url::parse("https://microsoft.com/devicelogin").unwrap();
        assert_eq!(device.verification_link(), (device.verification_uri.clone(), false));

        let complete = "https://login.live.com/oauth20_remoteconnect.srf?otc=TEST";
        device.verification_uri_complete = Some(complete.into());
        assert_eq!(device.verification_link(), (Url::parse(complete).unwrap(), true));
    }
    #[test]
    fn decodes_teredo_endpoints() {
        let list = teredo_candidates(&json!({"candidate":"candidate:1 1 UDP 1 2001:0:4136:e378:8000:63bf:3fff:fdd2 1 typ host"}));
        assert!(list[0]["candidate"].as_str().unwrap().contains("192.0.2.45 9002"));
        assert!(list[1]["candidate"].as_str().unwrap().contains("192.0.2.45 40000"));
    }
}
