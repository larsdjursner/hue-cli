mod bridge;
mod color;

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};

use bridge::{Bridge, Config, Error, Light, LightState, Result, Scene, State, Target};
use color::Color;

/// Control Philips Hue lights via the bridge's local API.
#[derive(Parser)]
#[command(
    name = "hue",
    after_help = "TARGET: all | light id | light or room name (case-insensitive)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Fade to the new state over this many seconds (on, off, bri, color, scene)
    #[arg(long, global = true, value_name = "SECONDS")]
    fade: Option<f64>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Find the bridge and create an app key
    Pair { ip: Option<String> },
    /// List lights grouped by room
    Ls {
        /// Only show this room
        room: Option<String>,
    },
    /// Turn lights on
    On { target: String },
    /// Turn lights off
    Off { target: String },
    /// Set brightness in percent; 0 turns lights off
    Bri {
        target: String,
        #[arg(value_parser = clap::value_parser!(u8).range(0..=100))]
        percent: u8,
    },
    /// Set color from a hex code like ff8800
    Color { target: String, hex: String },
    /// List each room's scenes with the color they give each light
    Scenes {
        /// Only show this room
        room: Option<String>,
    },
    /// Switch a room to one of its scenes
    Scene { room: String, scene: String },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hue: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(Cli { cmd, fade }: Cli) -> Result<()> {
    let transitiontime = fade.map(to_deciseconds).transpose()?;
    let on = |on| State {
        on: Some(on),
        transitiontime,
        ..State::default()
    };
    match cmd {
        Cmd::Pair { ip } => pair(ip),
        Cmd::Ls { room } => ls(&Bridge::connect()?, room.as_deref()),
        Cmd::On { target } => set(&target, on(true)),
        Cmd::Off { target } | Cmd::Bri { target, percent: 0 } => set(&target, on(false)),
        Cmd::Bri { target, percent } => set(
            &target,
            State {
                bri: Some(to_bri(percent)),
                ..on(true)
            },
        ),
        Cmd::Color { target, hex } => {
            let xy = color::hex_to_xy(&hex).map_err(Error::Other)?;
            set(
                &target,
                State {
                    xy: Some(xy),
                    ..on(true)
                },
            )
        }
        Cmd::Scenes { room } => scenes(&Bridge::connect()?, room.as_deref()),
        Cmd::Scene { room, scene } => recall(&Bridge::connect()?, &room, &scene, transitiontime),
    }
}

/// The bridge takes transition times in tenths of a second, up to 6553.5 s.
fn to_deciseconds(seconds: f64) -> Result<u16> {
    if !(0.0..=6553.5).contains(&seconds) {
        return Err(Error::Other("--fade must be 0-6553 seconds".into()));
    }
    Ok((seconds * 10.0).round() as u16)
}

fn pair(ip: Option<String>) -> Result<()> {
    let ip = match ip {
        Some(ip) => ip,
        None => match bridge::discover()?.as_slice() {
            [] => {
                return Err(Error::Other(
                    "no bridge found, pass the IP: hue pair <ip>".into(),
                ));
            }
            [ip] => ip.clone(),
            ips => {
                let ips = ips.join(", ");
                return Err(Error::Other(format!(
                    "found several bridges ({ips}), pick one: hue pair <ip>"
                )));
            }
        },
    };
    println!("bridge at {ip}: press the link button on the bridge...");
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if let Some(username) = bridge::create_user(&ip)? {
            let path = Config { ip, username }.save()?;
            println!("paired, saved to {}", path.display());
            return Ok(());
        }
        sleep(Duration::from_secs(2));
    }
    Err(Error::Other("timed out waiting for the link button".into()))
}

fn ls(bridge: &Bridge, only: Option<&str>) -> Result<()> {
    let mut lights = bridge.lights()?;
    let mut rooms: Vec<(String, Vec<(u32, Light)>)> = Vec::new();
    for group in bridge.groups()?.into_values().filter(|g| g.kind == "Room") {
        let members = group
            .light_ids()
            .filter_map(|id| lights.remove_entry(&id))
            .collect();
        rooms.push((group.name, members));
    }
    rooms.push(("No room".into(), lights.into_iter().collect()));

    match only {
        Some(name) => {
            rooms.retain(|(room, _)| same_name(room, name));
            if rooms.is_empty() {
                return Err(Error::Other(format!("no room named '{name}'")));
            }
        }
        None => rooms.retain(|(_, members)| !members.is_empty()),
    }

    for (i, (room, members)) in rooms.iter_mut().enumerate() {
        if i > 0 {
            println!();
        }
        println!("{room}");
        members.sort_by_key(|(id, _)| *id);
        for (id, light) in members {
            let color = light.state.color().map(swatch).unwrap_or_default();
            println!(
                "  {id:>3}  {:<24} {:<11}  {color}",
                light.name,
                status(&light.state)
            );
        }
    }
    Ok(())
}

/// Scenes the user made in the app; scenes other apps and switches made for themselves are hidden.
fn scenes(bridge: &Bridge, only: Option<&str>) -> Result<()> {
    let mut by_group: BTreeMap<u32, Vec<(String, Scene)>> = BTreeMap::new();
    for (id, scene) in bridge.scenes()? {
        if let (false, Some(group)) = (scene.recycle, scene.group_id()) {
            by_group.entry(group).or_default().push((id, scene));
        }
    }
    let groups = bridge.groups()?;
    let mut sections: Vec<(&str, Vec<(String, Scene)>)> = by_group
        .into_iter()
        .filter_map(|(id, scenes)| Some((groups.get(&id)?.name.as_str(), scenes)))
        .collect();
    if let Some(name) = only {
        sections.retain(|(room, _)| same_name(room, name));
        if sections.is_empty() {
            return Err(Error::Other(format!("no scenes for a room named '{name}'")));
        }
    }

    for (i, (room, scenes)) in sections.iter_mut().enumerate() {
        if i > 0 {
            println!();
        }
        println!("{room}");
        scenes.sort_by(|(_, a), (_, b)| a.name.cmp(&b.name));
        for (id, scene) in scenes {
            let lights: Vec<String> = bridge
                .scene(id)?
                .lightstates
                .values()
                .map(|state| match state.color() {
                    Some(color) if state.on => swatch(color),
                    _ => format!("{:<10}", "off"),
                })
                .collect();
            println!("  {:<24} {}", scene.name, lights.join("  "));
        }
    }
    Ok(())
}

fn recall(bridge: &Bridge, room: &str, scene: &str, transitiontime: Option<u16>) -> Result<()> {
    let (&group, _) = bridge
        .groups()?
        .iter()
        .find(|(_, g)| same_name(&g.name, room))
        .ok_or_else(|| Error::Other(format!("no room named '{room}'")))?;
    let (id, _) = bridge
        .scenes()?
        .into_iter()
        .find(|(_, s)| s.group_id() == Some(group) && same_name(&s.name, scene))
        .ok_or_else(|| {
            Error::Other(format!(
                "no scene named '{scene}' in {room}, see: hue scenes {room}"
            ))
        })?;
    let state = State {
        scene: Some(id),
        transitiontime,
        ..State::default()
    };
    bridge.set(&Target::Group(group), &state)
}

/// A colored block plus hex code; just the hex code when output isn't a terminal.
fn swatch(color: Color) -> String {
    let rgb @ [r, g, b] = color.to_rgb();
    let hex = color::to_hex(rgb);
    if std::io::stdout().is_terminal() {
        format!("\x1b[48;2;{r};{g};{b}m  \x1b[0m #{hex}")
    } else {
        format!("#{hex}")
    }
}

fn status(state: &LightState) -> String {
    match (state.reachable, state.on, state.bri) {
        (Some(false), ..) => "unreachable".into(),
        (_, false, _) => "off".into(),
        (_, true, Some(bri)) => format!("on  {:>3}%", (bri as f64 / 254.0 * 100.0).round()),
        (_, true, None) => "on".into(),
    }
}

/// Percent (1-100) to the bridge's 1-254 brightness scale.
fn to_bri(percent: u8) -> u8 {
    (percent as f64 * 254.0 / 100.0).round().max(1.0) as u8
}

fn set(target: &str, state: State) -> Result<()> {
    let bridge = Bridge::connect()?;
    bridge.set(&resolve(&bridge, target)?, &state)
}

/// A target is "all", a light id, or a light or room name; a light name wins over a room name.
fn resolve(bridge: &Bridge, target: &str) -> Result<Target> {
    if target == "all" {
        return Ok(Target::All);
    }
    let lights = bridge.lights()?;
    if let Ok(id) = target.parse()
        && lights.contains_key(&id)
    {
        return Ok(Target::Light(id));
    }
    if let Some((&id, _)) = lights.iter().find(|(_, l)| same_name(&l.name, target)) {
        return Ok(Target::Light(id));
    }
    if let Some((&id, _)) = bridge
        .groups()?
        .iter()
        .find(|(_, g)| same_name(&g.name, target))
    {
        return Ok(Target::Group(id));
    }
    Err(Error::Other(format!("no light or room named '{target}'")))
}

fn same_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}
