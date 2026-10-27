//! The bridge's local v1 HTTP API, and the config that says which bridge to talk to.
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use ureq::Agent;
use ureq::http::Response;

use crate::color::Color;

/// The bridge's error type for "link button not pressed".
const LINK_BUTTON_NOT_PRESSED: u64 = 101;

#[derive(Debug)]
pub enum Error {
    Unreachable,
    NotPaired,
    Bridge { kind: u64, description: String },
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Unreachable => write!(f, "can't reach bridge"),
            Error::NotPaired => write!(f, "not paired yet, run: hue pair"),
            Error::Bridge { description, .. } => write!(f, "{description}"),
            Error::Other(msg) => write!(f, "{msg}"),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// No Debug on purpose: the username is the bridge's app key.
#[derive(Serialize, Deserialize)]
pub struct Config {
    pub ip: String,
    pub username: String,
}

fn config_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/hue/config.json")
}

impl Config {
    pub fn load() -> Result<Self> {
        let text = fs::read_to_string(config_path()).map_err(|_| Error::NotPaired)?;
        serde_json::from_str(&text).map_err(|e| Error::Other(format!("bad config: {e}")))
    }

    pub fn save(&self) -> Result<PathBuf> {
        let path = config_path();
        let io = |e: std::io::Error| Error::Other(format!("can't write {}: {e}", path.display()));
        fs::create_dir_all(path.parent().unwrap()).map_err(io)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(io)?;
        file.write_all(serde_json::to_string(self).unwrap().as_bytes())
            .map_err(io)?;
        Ok(path)
    }
}

#[derive(Deserialize)]
pub struct Light {
    pub name: String,
    pub state: LightState,
}

/// A light's current state, or the state a scene sets it to.
#[derive(Deserialize)]
pub struct LightState {
    pub on: bool,
    pub bri: Option<u8>,
    pub reachable: Option<bool>,
    xy: Option<[f64; 2]>,
    ct: Option<u16>,
    /// xy, ct or hs; missing in scenes.
    colormode: Option<String>,
}

impl LightState {
    /// The bridge keeps xy up to date in hs mode, so only ct needs telling apart.
    pub fn color(&self) -> Option<Color> {
        match (self.colormode.as_deref(), self.xy, self.ct) {
            (Some("ct"), _, Some(ct)) | (_, None, Some(ct)) => Some(Color::Ct(ct)),
            (_, Some(xy), _) => Some(Color::Xy(xy)),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
pub struct Group {
    pub name: String,
    /// Room, Zone, Entertainment, LightGroup, ...
    #[serde(rename = "type")]
    pub kind: String,
    lights: Vec<String>,
}

impl Group {
    pub fn light_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.lights.iter().filter_map(|id| id.parse().ok())
    }
}

#[derive(Deserialize)]
pub struct Scene {
    pub name: String,
    group: Option<String>,
    /// Set on scenes other apps and switches made for themselves; the bridge may delete these.
    #[serde(default)]
    pub recycle: bool,
    /// Only filled in by Bridge::scene, not by Bridge::scenes.
    #[serde(default)]
    pub lightstates: BTreeMap<u32, LightState>,
}

impl Scene {
    /// The room or zone this scene belongs to; None for scenes made per light.
    pub fn group_id(&self) -> Option<u32> {
        self.group.as_ref()?.parse().ok()
    }
}

/// A state change; unset fields are left alone.
#[derive(Serialize, Default)]
pub struct State {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bri: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub xy: Option<[f64; 2]>,
    /// How long the bulbs take to fade to this state, in tenths of a second.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transitiontime: Option<u16>,
    /// Scene id; only valid on a group target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scene: Option<String>,
}

pub enum Target {
    All,
    Light(u32),
    Group(u32),
}

impl Target {
    fn path(&self) -> String {
        match self {
            Target::All => "groups/0/action".into(),
            Target::Light(id) => format!("lights/{id}/state"),
            Target::Group(id) => format!("groups/{id}/action"),
        }
    }
}

fn agent() -> Agent {
    Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .into()
}

pub struct Bridge {
    agent: Agent,
    base: String,
}

impl Bridge {
    pub fn connect() -> Result<Self> {
        let config = Config::load()?;
        Ok(Bridge {
            agent: agent(),
            base: format!("http://{}/api/{}", config.ip, config.username),
        })
    }

    pub fn lights(&self) -> Result<BTreeMap<u32, Light>> {
        self.get("lights")
    }

    pub fn groups(&self) -> Result<BTreeMap<u32, Group>> {
        self.get("groups")
    }

    /// Keyed by scene id, which is a string like "a1B2c3D4e5F6g7H8".
    pub fn scenes(&self) -> Result<BTreeMap<String, Scene>> {
        self.get("scenes")
    }

    pub fn scene(&self, id: &str) -> Result<Scene> {
        self.get(&format!("scenes/{id}"))
    }

    pub fn set(&self, target: &Target, state: &State) -> Result<()> {
        let url = format!("{}/{}", self.base, target.path());
        parse::<Value>(self.agent.put(url).send_json(state)).map(|_| ())
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        parse(self.agent.get(format!("{}/{path}", self.base)).call())
    }
}

/// Ask the bridge for a new app key. Ok(None) until its link button is pressed.
pub fn create_user(ip: &str) -> Result<Option<String>> {
    let reply = agent()
        .post(format!("http://{ip}/api"))
        .send_json(json!({"devicetype": "hue-cli#mac"}));
    match parse::<Vec<Value>>(reply) {
        Ok(results) => results
            .first()
            .and_then(|r| r["success"]["username"].as_str())
            .map(|username| Some(username.to_string()))
            .ok_or_else(|| Error::Other("unexpected reply from bridge".into())),
        Err(Error::Bridge {
            kind: LINK_BUTTON_NOT_PRESSED,
            ..
        }) => Ok(None),
        Err(e) => Err(e),
    }
}

#[derive(Deserialize)]
struct Discovered {
    #[serde(rename = "internalipaddress")]
    ip: String,
}

/// IPs of the bridges Philips' discovery service has seen from this network.
pub fn discover() -> Result<Vec<String>> {
    agent()
        .get("https://discovery.meethue.com")
        .call()
        .and_then(|mut reply| reply.body_mut().read_json::<Vec<Discovered>>())
        .map(|found| found.into_iter().map(|b| b.ip).collect())
        .map_err(|_| Error::Other("bridge discovery failed, pass the IP: hue pair <ip>".into()))
}

/// Read a bridge reply, turning its [{"error": ...}] lists into an Error.
fn parse<T: DeserializeOwned>(
    reply: std::result::Result<Response<ureq::Body>, ureq::Error>,
) -> Result<T> {
    let value: Value = reply
        .map_err(|_| Error::Unreachable)?
        .body_mut()
        .read_json()
        .map_err(|e| Error::Other(format!("bad reply from bridge: {e}")))?;
    let errors: Vec<&Value> = value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r.get("error"))
        .collect();
    if let Some(first) = errors.first() {
        return Err(Error::Bridge {
            kind: first["type"].as_u64().unwrap_or(0),
            description: errors
                .iter()
                .filter_map(|e| e["description"].as_str())
                .collect::<Vec<_>>()
                .join("; "),
        });
    }
    serde_json::from_value(value)
        .map_err(|e| Error::Other(format!("unexpected reply from bridge: {e}")))
}
