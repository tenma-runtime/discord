//! The native half of tenma/discord: the gateway connection, REST calls and voice, with PHP on the
//! other side of `Tenma\Plugin` and `Messaging\Channel`.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::ThreadId;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect::{Attempt, Policy};
use serde::Deserialize;
use songbird::{EventContext, EventHandler, Songbird, TrackEvent};
use songbird::input::HttpRequest;
use songbird::shards::TwilightMap;
use songbird::tracks::{PlayMode, Track, TrackQueue};
use tenma_plugin::{LOG_ERROR, LOG_INFO, LOG_WARNING, Plugin, Reply, Token, api};
use tokio::runtime::Runtime;
use twilight_gateway::{CloseFrame, Event, EventTypeFlags, Intents, Message, MessageSender, Shard, ShardId};
use twilight_model::gateway::payload::outgoing::UpdateVoiceState;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, GuildMarker};

const API: &str = "https://discord.com/api/v10";
/// The most tracks a guild's queue holds, so nobody fills it with endless streams.
const QUEUE_LIMIT: usize = 50;
/// What a refused audio URL answers, the same for every reason so it tells nothing about the network.
const NOT_ALLOWED: &str = "url not allowed: only public http and https addresses play";

static PLUGIN: Plugin = Plugin {
    name: c"discord".as_ptr(),
    call: Some(call),
    call_async: Some(call_async),
    adopt: None,
    message: None,
    closed: None,
    shutdown: Some(shutdown),
};

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static BOT: OnceLock<Bot> = OnceLock::new();
/// The workers that asked for events, by the thread they run on; an event goes to one of them.
static WORKERS: Mutex<Vec<ThreadId>> = Mutex::new(Vec::new());
static STOPPING: AtomicBool = AtomicBool::new(false);

struct Bot {
    token: String,
    http: reqwest::Client,
    /// Fetches the audio URLs PHP passes in, which may come from any guild member: it reaches only public
    /// addresses, also after redirects and when a name resolves differently the second time.
    audio: reqwest::Client,
    voice: OnceLock<Arc<Songbird>>,
    gateway: OnceLock<MessageSender>,
}

/// A call's token, moved into the task that completes it.
struct Pending(*mut Token);

unsafe impl Send for Pending {}

#[unsafe(no_mangle)]
extern "C" fn tenma_plugin_init(api: &'static tenma_plugin::Api, abi: u32) -> *const Plugin {
    if abi != tenma_plugin::ABI {
        return std::ptr::null();
    }
    tenma_plugin::set_api(api);
    &PLUGIN
}

#[derive(Deserialize)]
struct Start {
    token: String,
    intents: u64,
}

extern "C" fn call(
    function: *const std::ffi::c_char,
    function_len: usize,
    input: *const u8,
    input_len: usize,
    out: *mut Reply,
) -> i32 {
    let (function, input) = unsafe {
        (
            tenma_plugin::slice(function.cast(), function_len),
            tenma_plugin::slice(input, input_len),
        )
    };
    let result = match function {
        b"start" => start(input).map(|()| String::new()),
        b"register" => Ok(register()),
        _ => Err(format!("unknown function {}", String::from_utf8_lossy(function))),
    };
    match result {
        Ok(reply) => {
            api().reply(out, reply.as_bytes());
            0
        }
        Err(message) => {
            api().reply(out, message.as_bytes());
            1
        }
    }
}

/// Connects to the gateway once per process; later calls, from other workers, find it running.
fn start(input: &[u8]) -> Result<(), String> {
    let start: Start = serde_json::from_slice(input).map_err(|error| error.to_string())?;
    let bot = Bot {
        token: start.token.clone(),
        // Some stations (listen.moe behind Cloudflare) refuse requests without a user agent
        http: reqwest::Client::builder()
            .user_agent(concat!("tenma-discord/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| error.to_string())?,
        audio: audio_client().map_err(|error| error.to_string())?,
        voice: OnceLock::new(),
        gateway: OnceLock::new(),
    };
    if BOT.set(bot).is_err() {
        return Ok(());
    }
    let runtime = RUNTIME.get_or_init(|| Runtime::new().expect("tokio runtime"));
    runtime.spawn(gateway(start.token, Intents::from_bits_truncate(start.intents)));
    Ok(())
}

/// The channel this worker receives its events on. A call runs on the worker's own thread, so the
/// thread tells workers apart; a worker that registers again keeps its channel.
fn register() -> String {
    let thread = std::thread::current().id();
    let mut workers = WORKERS.lock().expect("workers");
    let index = match workers.iter().position(|known| *known == thread) {
        Some(index) => index,
        None => {
            workers.push(thread);
            workers.len() - 1
        }
    };
    channel(index)
}

fn channel(worker: usize) -> String {
    format!("discord.w{worker}")
}

#[derive(Deserialize)]
struct Dispatch {
    op: u8,
    t: Option<String>,
    d: Option<Keys>,
}

#[derive(Deserialize)]
struct Keys {
    guild_id: Option<String>,
    channel_id: Option<String>,
    id: Option<String>,
}

/// Sends a dispatch to the worker that owns its guild (or its channel, for direct messages), so the
/// events of one guild stay in order on one worker. Events of no guild, like READY, reach every worker.
fn route(text: &str) {
    let Ok(dispatch) = serde_json::from_str::<Dispatch>(text) else {
        return;
    };
    // Only dispatches go to PHP, not heartbeat acks and the like
    if dispatch.op != 0 {
        return;
    }
    let workers = WORKERS.lock().expect("workers").len();
    if workers == 0 {
        return;
    }

    let keys = dispatch.d;
    let guild_event = matches!(dispatch.t.as_deref(), Some("GUILD_CREATE" | "GUILD_UPDATE" | "GUILD_DELETE"));
    let key = keys.and_then(|keys| {
        keys.guild_id
            .or(if guild_event { keys.id } else { None })
            .or(keys.channel_id)
    });

    match key.and_then(|key| key.parse::<u64>().ok()) {
        Some(key) => api().post(&channel((key % workers as u64) as usize), text.as_bytes()),
        None => {
            for worker in 0..workers {
                api().post(&channel(worker), text.as_bytes());
            }
        }
    }
}

async fn gateway(token: String, intents: Intents) {
    let mut shard = Shard::new(ShardId::ONE, token, intents);
    let senders = Arc::new(TwilightMap::new(HashMap::from([(0, shard.sender())])));
    let _ = BOT.get().expect("started").gateway.set(shard.sender());
    let wanted = EventTypeFlags::READY
        | EventTypeFlags::VOICE_STATE_UPDATE
        | EventTypeFlags::VOICE_SERVER_UPDATE;

    while let Some(message) = shard.next().await {
        let text = match message {
            Ok(Message::Text(text)) => text,
            Ok(Message::Close(_)) if STOPPING.load(Ordering::Relaxed) => break,
            Ok(Message::Close(frame)) => {
                api().log(LOG_WARNING, &format!("discord gateway closed: {frame:?}"));
                continue;
            }
            Err(error) => {
                api().log(LOG_WARNING, &format!("discord gateway: {error}"));
                continue;
            }
        };

        // Voice needs the voice state and server updates; READY names the bot's user
        if let Ok(Some(event)) = twilight_gateway::parse(text.clone(), wanted) {
            let event = Event::from(event);
            if let Event::Ready(ready) = &event {
                let voice = Songbird::twilight(senders.clone(), ready.user.id);
                let _ = BOT.get().expect("started").voice.set(Arc::new(voice));
                api().log(LOG_INFO, &format!("discord: connected as {}", ready.user.name));
            }
            if let Some(voice) = BOT.get().and_then(|bot| bot.voice.get()) {
                voice.process(&event).await;
            }
        }

        route(&text);
    }
    if STOPPING.load(Ordering::Relaxed) {
        return;
    }
    api().log(LOG_ERROR, "discord gateway: gave up, the token or the intents were refused");
}

/// Leaves every voice channel and closes the gateway, so the bot goes offline at once instead of
/// after Discord's timeout. Bounded, since tenma waits for it.
extern "C" fn shutdown() {
    let (Some(runtime), Some(bot)) = (RUNTIME.get(), BOT.get()) else {
        return;
    };
    STOPPING.store(true, Ordering::Relaxed);
    let leave = async {
        if let Some(voice) = bot.voice.get() {
            for (guild, _) in voice.iter().collect::<Vec<_>>() {
                let _ = voice.remove(guild).await;
            }
        }
        if let Some(gateway) = bot.gateway.get() {
            let _ = gateway.close(CloseFrame::NORMAL);
        }
    };
    runtime.block_on(async {
        let _ = tokio::time::timeout(Duration::from_secs(3), leave).await;
    });
}

extern "C" fn call_async(
    function: *const std::ffi::c_char,
    function_len: usize,
    input: *const u8,
    input_len: usize,
    token: *mut Token,
) {
    let (function, input) = unsafe {
        (
            tenma_plugin::slice(function.cast(), function_len).to_vec(),
            tenma_plugin::slice(input, input_len).to_vec(),
        )
    };
    let pending = Pending(token);
    let Some(runtime) = RUNTIME.get() else {
        api().complete(pending.0, Err("discord: call start first"));
        return;
    };
    runtime.spawn(async move {
        let result = match function.as_slice() {
            b"rest" => rest(&input).await,
            b"voice.join" => voice_join(&input).await,
            b"voice.play" => voice_play(&input).await,
            b"voice.queue" => voice_queue(&input).await,
            b"voice.skip" => voice_skip(&input).await,
            b"voice.pause" => voice_pause(&input, true).await,
            b"voice.resume" => voice_pause(&input, false).await,
            b"voice.tracks" => voice_tracks(&input).await,
            b"voice.stop" => voice_stop(&input).await,
            b"voice.mute" => voice_mute(&input).await,
            b"voice.leave" => voice_leave(&input).await,
            _ => Err(format!("unknown function {}", String::from_utf8_lossy(&function))),
        };
        let pending = pending;
        match result {
            Ok(bytes) => api().complete(pending.0, Ok(&bytes)),
            Err(message) => api().complete(pending.0, Err(&message)),
        }
    });
}

#[derive(Deserialize)]
struct Rest {
    method: String,
    path: String,
    body: Option<serde_json::Value>,
}

/// A REST call with the bot's token: the response body, or an error with Discord's answer.
async fn rest(input: &[u8]) -> Result<Vec<u8>, String> {
    let rest: Rest = serde_json::from_slice(input).map_err(|error| error.to_string())?;
    let bot = BOT.get().ok_or("discord: call start first")?;
    let method = reqwest::Method::from_bytes(rest.method.as_bytes()).map_err(|error| error.to_string())?;

    let mut request = bot
        .http
        .request(method, format!("{API}{}", rest.path))
        .header("Authorization", format!("Bot {}", bot.token));
    if let Some(body) = rest.body {
        request = request.json(&body);
    }

    let response = request.send().await.map_err(|error| error.to_string())?;
    let status = response.status();
    let body = response.bytes().await.map_err(|error| error.to_string())?;
    if status.is_client_error() || status.is_server_error() {
        return Err(format!("discord {status}: {}", String::from_utf8_lossy(&body)));
    }
    Ok(body.to_vec())
}

#[derive(Deserialize)]
struct Voice {
    guild: String,
    channel: Option<String>,
    url: Option<String>,
    muted: Option<bool>,
}

fn voice(input: &[u8]) -> Result<(Voice, Arc<Songbird>), String> {
    let voice: Voice = serde_json::from_slice(input).map_err(|error| error.to_string())?;
    let songbird = BOT
        .get()
        .and_then(|bot| bot.voice.get())
        .ok_or("discord: not connected yet")?;
    Ok((voice, songbird.clone()))
}

fn id(value: &str) -> Result<NonZeroU64, String> {
    value.parse().map_err(|_| format!("not a Discord id: {value}"))
}

async fn voice_join(input: &[u8]) -> Result<Vec<u8>, String> {
    let (voice, songbird) = voice(input)?;
    let channel = voice.channel.as_deref().ok_or("voice.join needs a channel")?;
    songbird
        .join(id(&voice.guild)?, id(channel)?)
        .await
        .map_err(|error| error.to_string())?;
    Ok(Vec::new())
}

/// Plays a URL at once: the queue is emptied first.
async fn voice_play(input: &[u8]) -> Result<Vec<u8>, String> {
    enqueue(input, true).await
}

/// Plays a URL after the tracks already queued.
async fn voice_queue(input: &[u8]) -> Result<Vec<u8>, String> {
    enqueue(input, false).await
}

async fn enqueue(input: &[u8], replace: bool) -> Result<Vec<u8>, String> {
    let (voice, songbird) = voice(input)?;
    let url = voice.url.ok_or("voice.play needs a url")?;
    let guild = id(&voice.guild)?;
    let call = songbird.get(guild).ok_or("not in a voice channel of this guild")?;
    let http = BOT.get().expect("started").audio.clone();

    // The client refuses private addresses on its own; checking here turns that into this call's error
    // instead of a TRACK_FAILED later
    if !public_url(&url).await {
        return Err(NOT_ALLOWED.to_string());
    }

    let input = HttpRequest::new(http, url.clone()).into();
    let track = Track::new_with_data(input, Arc::new(url));

    let mut call = call.lock().await;
    if replace {
        call.queue().stop();
    }
    if call.queue().len() >= QUEUE_LIMIT {
        return Err(format!("the queue is full: {QUEUE_LIMIT} tracks at most"));
    }
    let handle = call.enqueue(track).await;

    let announce = Announce {
        guild,
        started: Arc::new(AtomicBool::new(false)),
        finished: Arc::new(AtomicBool::new(false)),
    };
    for event in [TrackEvent::Playable, TrackEvent::Play, TrackEvent::End, TrackEvent::Error] {
        let _ = handle.add_event(event.into(), announce.clone());
    }
    Ok(Vec::new())
}

/// The client for audio URLs: no proxy, since a proxy would resolve names past the check, a resolver
/// that drops private addresses, and redirects only to URLs that pass the same check.
fn audio_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("tenma-discord/", env!("CARGO_PKG_VERSION")))
        .no_proxy()
        .dns_resolver(Arc::new(PublicResolver))
        .redirect(Policy::custom(redirect))
        .build()
}

fn redirect(attempt: Attempt) -> reqwest::redirect::Action {
    if attempt.previous().len() >= 10 {
        return attempt.error("too many redirects");
    }
    // A redirect to an IP address skips the resolver, so allowed() looks at it here
    if allowed(attempt.url()) {
        attempt.follow()
    } else {
        attempt.error(NOT_ALLOWED)
    }
}

/// Resolves names to their public addresses only; a name with none fails to resolve.
struct PublicResolver;

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let addresses = public_addresses(name.as_str()).await;
            if addresses.is_empty() {
                return Err(NOT_ALLOWED.into());
            }
            let addresses: Addrs = Box::new(addresses.into_iter());
            Ok(addresses)
        })
    }
}

async fn public_addresses(host: &str) -> Vec<std::net::SocketAddr> {
    // The port is a placeholder: the connector puts the URL's port in
    let Ok(addresses) = tokio::net::lookup_host((host, 0)).await else {
        return Vec::new();
    };
    addresses.filter(|address| public(address.ip())).collect()
}

/// An http or https URL whose host is a public address or a name with one.
async fn public_url(url: &str) -> bool {
    let Ok(url) = Url::parse(url) else {
        return false;
    };
    if !allowed(&url) {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    // allowed() has checked an IP address already
    ip_address(host).is_some() || !public_addresses(host).await.is_empty()
}

/// The scheme is http or https, and a host given as an IP address is public. Names are left to the
/// resolver, which the connection skips for IP addresses.
fn allowed(url: &Url) -> bool {
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    ip_address(host).is_none_or(public)
}

/// The host as an IP address, IPv6 without its brackets; None for a name.
fn ip_address(host: &str) -> Option<IpAddr> {
    host.trim_start_matches('[').trim_end_matches(']').parse().ok()
}

/// An address on the public internet: not loopback, private, link-local (the cloud metadata
/// services), shared (CGNAT), unspecified, broadcast, multicast, reserved or for documentation.
fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(mapped) => public_v4(mapped),
            None => public_v6(ip),
        },
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    let this_network = a == 0;
    let shared = a == 100 && (64..128).contains(&b);
    let protocol = a == 192 && b == 0 && c == 0;
    let benchmark = a == 198 && (18..20).contains(&b);
    // Multicast, reserved and broadcast: 224.0.0.0 and up
    let above_unicast = a >= 224;

    !(this_network
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || shared
        || protocol
        || benchmark
        || ip.is_documentation()
        || above_unicast)
}

/// Only global unicast (2000::/3) counts, without documentation, Teredo and 6to4, the last two
/// because they carry an IPv4 address that may be private.
fn public_v6(ip: Ipv6Addr) -> bool {
    let [first, second, ..] = ip.segments();
    let global_unicast = first & 0xe000 == 0x2000;
    let documentation = first == 0x2001 && second == 0x0db8;
    let teredo = first == 0x2001 && second == 0;
    let six_to_four = first == 0x2002;

    global_unicast && !documentation && !teredo && !six_to_four
}

/// Runs a function on the queue of the guild's call; nothing happens outside a voice channel.
async fn with_queue(input: &[u8], action: impl FnOnce(&TrackQueue)) -> Result<Vec<u8>, String> {
    let (voice, songbird) = voice(input)?;
    if let Some(call) = songbird.get(id(&voice.guild)?) {
        action(call.lock().await.queue());
    }
    Ok(Vec::new())
}

async fn voice_skip(input: &[u8]) -> Result<Vec<u8>, String> {
    with_queue(input, |queue| {
        let _ = queue.skip();
    })
    .await
}

async fn voice_pause(input: &[u8], pause: bool) -> Result<Vec<u8>, String> {
    with_queue(input, |queue| {
        let _ = if pause { queue.pause() } else { queue.resume() };
    })
    .await
}

/// The URLs in the queue as a JSON list, the one playing first.
async fn voice_tracks(input: &[u8]) -> Result<Vec<u8>, String> {
    let (voice, songbird) = voice(input)?;
    let Some(call) = songbird.get(id(&voice.guild)?) else {
        return Ok(b"[]".to_vec());
    };
    let tracks = call.lock().await.queue().current_queue();
    let urls: Vec<String> = tracks
        .iter()
        .map(|track| track.data::<String>().to_string())
        .collect();
    serde_json::to_vec(&urls).map_err(|error| error.to_string())
}

/// Stops what plays and empties the queue.
async fn voice_stop(input: &[u8]) -> Result<Vec<u8>, String> {
    with_queue(input, |queue| queue.stop()).await
}

/// Mutes the bot in its voice channel: the stream keeps running, nothing is sent, Discord shows it muted.
async fn voice_mute(input: &[u8]) -> Result<Vec<u8>, String> {
    let (voice, songbird) = voice(input)?;
    let call = songbird.get(id(&voice.guild)?).ok_or("not in a voice channel of this guild")?;
    let muted = voice.muted.unwrap_or(true);
    call.lock().await.mute(muted).await.map_err(|error| error.to_string())?;
    Ok(Vec::new())
}

/// Tells PHP when a track starts, ends or fails, as TRACK_STARTED, TRACK_ENDED and TRACK_FAILED
/// dispatches to the guild's worker. Each happens once per track, though songbird fires Play again
/// after a pause and End after an error.
#[derive(Clone)]
struct Announce {
    guild: NonZeroU64,
    started: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
}

impl Announce {
    fn post(&self, kind: &str, url: &str, error: Option<String>) {
        let dispatch = serde_json::json!({
            "op": 0,
            "t": kind,
            "d": { "guild_id": self.guild.to_string(), "url": url, "error": error },
        });
        route(&dispatch.to_string());
    }
}

#[async_trait::async_trait]
impl EventHandler for Announce {
    async fn act(&self, context: &EventContext<'_>) -> Option<songbird::Event> {
        let EventContext::Track(tracks) = context else {
            return None;
        };
        for (state, track) in *tracks {
            let url = track.data::<String>();
            match &state.playing {
                // A queued track waits paused: it starts when the queue moves it to Play
                PlayMode::Play if !self.started.swap(true, Ordering::Relaxed) => {
                    self.post("TRACK_STARTED", &url, None);
                }
                PlayMode::Errored(error) if !self.finished.swap(true, Ordering::Relaxed) => {
                    api().log(LOG_WARNING, &format!("discord voice: {url}: {error}"));
                    self.post("TRACK_FAILED", &url, Some(error.to_string()));
                }
                PlayMode::End | PlayMode::Stop if !self.finished.swap(true, Ordering::Relaxed) => {
                    self.post("TRACK_ENDED", &url, None);
                }
                _ => {}
            }
        }
        None
    }
}

/// Leaves the guild's voice channel, also one this process never joined (a call from before a restart).
async fn voice_leave(input: &[u8]) -> Result<Vec<u8>, String> {
    let (voice, songbird) = voice(input)?;
    let guild = id(&voice.guild)?;
    if songbird.get(guild).is_some() {
        songbird.remove(guild).await.map_err(|error| error.to_string())?;
        return Ok(Vec::new());
    }
    let gateway = BOT.get().and_then(|bot| bot.gateway.get()).ok_or("discord: not connected yet")?;
    let leave = UpdateVoiceState::new(Id::<GuildMarker>::from(guild), None::<Id<ChannelMarker>>, false, false);
    gateway.command(&leave).map_err(|error| error.to_string())?;
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_addresses_inside_the_network() {
        let refused = [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "::",
            "::1",
            "fe80::1",
            "fc00::1",
            "fd00:ec2::254",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:10.0.0.1",
        ];
        for ip in refused {
            assert!(!public(ip.parse().unwrap()), "{ip} must be refused");
        }
    }

    #[test]
    fn allows_public_addresses() {
        let allowed = [
            "1.1.1.1",
            "172.32.0.1",
            "100.128.0.1",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
        ];
        for ip in allowed {
            assert!(public(ip.parse().unwrap()), "{ip} must be allowed");
        }
    }

    #[test]
    fn checks_scheme_and_literal_hosts() {
        let allowed = |url: &str| allowed(&Url::parse(url).unwrap());
        assert!(allowed("https://listen.moe/stream"));
        assert!(allowed("http://1.1.1.1:8000/"));
        assert!(!allowed("file:///etc/passwd"));
        assert!(!allowed("ftp://example.com/song.mp3"));
        assert!(!allowed("http://127.0.0.1:5432/"));
        assert!(!allowed("http://2130706433/"));
        assert!(!allowed("http://[::1]/"));
        assert!(!allowed("http://[::ffff:a9fe:a9fe]/latest/meta-data/"));
    }
}
