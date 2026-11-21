//! `capture-audio` on Windows: what this machine plays, except VRChat and PetPlay.
//!
//!   petplay-media capture-audio [LISTEN_ADDR] [--iroh-share] [--bitrate N] [--exclude NAME]...
//!
//! Plain loopback would carry VRChat (a friend's own voice, coming back late) and PetPlay's own
//! playback of the other side's audio (an echo loop). So each audio session on the default
//! output is captured on its own with process loopback and mixed. Windows can only include a
//! process together with its whole tree, so a session whose tree holds an excluded process
//! (steam.exe, which starts VRChat) is left out too, and one already inside a captured tree is
//! not captured twice. Sessions are rescanned every two seconds.
//!
//! Prints `MOQ_READY`, `IROH_SHARE` (with `--iroh-share`), `AUDIO_SOURCE`, then
//! `AUDIO_CAPTURE pid= name=` per captured process and `AUDIO_STATS` every five seconds.
use crate::audio::{AudioPublisher, CHANNELS, FRAME, RATE};
use crate::publish::Transport;
use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};

const USAGE: &str = "usage: petplay-media capture-audio [LISTEN_ADDR] [--iroh-share] [--bitrate BITS_PER_SECOND] [--exclude PROCESS.exe]...";
/// Never shared: VRChat, and PetPlay's own processes (its playback of the other side).
const EXCLUDED: &[&str] = &[
    "vrchat.exe",
    "petplay-media.exe",
    "petplay-agent.exe",
    "petplay.exe",
];
/// A source's backlog beyond this is dropped, so a stalled mix cannot build up latency.
const SOURCE_MAX: usize = RATE as usize * 200 / 1000 * CHANNELS;

struct Source {
    name: String,
    samples: Arc<Mutex<VecDeque<f32>>>,
    alive: Arc<AtomicBool>,
}

/// Every process: pid → (parent, lowercase exe name).
fn processes() -> HashMap<u32, (u32, String)> {
    let mut all = HashMap::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return all;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut ok = Process32FirstW(snapshot, &mut entry).is_ok();
        while ok {
            let end = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..end]).to_lowercase();
            all.insert(entry.th32ProcessID, (entry.th32ParentProcessID, name));
            ok = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    all
}

/// Processes with an audio session on the default output.
fn session_pids() -> Result<HashSet<u32>> {
    let device = wasapi::DeviceEnumerator::new()?.get_default_device(wasapi::Direction::Render)?;
    let sessions = device
        .get_iaudiosessionmanager()?
        .get_audiosessionenumerator()?;
    let mut pids = HashSet::new();
    for i in 0..sessions.get_count()? {
        let session = sessions.get_session(i)?;
        if matches!(session.get_state(), Ok(wasapi::SessionState::Expired)) {
            continue;
        }
        if let Ok(pid) = session.get_process_id() {
            // 0 is the system sounds session, which process loopback cannot target.
            if pid != 0 {
                pids.insert(pid);
            }
        }
    }
    Ok(pids)
}

/// Which session processes to capture, given the excluded names.
fn choose(
    sessions: &HashSet<u32>,
    all: &HashMap<u32, (u32, String)>,
    excluded: &[String],
) -> Vec<u32> {
    let ancestors = |pid: u32| {
        let mut chain = Vec::new();
        let mut at = pid;
        while let Some((parent, _)) = all.get(&at) {
            if *parent == 0 || *parent == at || chain.contains(parent) || chain.len() > 64 {
                break;
            }
            chain.push(*parent);
            at = *parent;
        }
        chain
    };
    let is_excluded = |pid: u32| all.get(&pid).is_some_and(|(_, n)| excluded.contains(n));
    // Every process under an excluded one, and every ancestor of one: including either
    // would capture the excluded process too.
    let mut tainted = HashSet::new();
    for (&pid, _) in all.iter().filter(|(pid, _)| is_excluded(**pid)) {
        tainted.insert(pid);
        tainted.extend(ancestors(pid));
    }
    let candidates: Vec<u32> = sessions
        .iter()
        .copied()
        .filter(|pid| {
            all.contains_key(pid)
                && !tainted.contains(pid)
                && !ancestors(*pid).iter().any(|a| is_excluded(*a))
        })
        .collect();
    // A process inside another candidate's tree is already captured with it.
    candidates
        .iter()
        .copied()
        .filter(|pid| !ancestors(*pid).iter().any(|a| candidates.contains(a)))
        .collect()
}

fn capture(pid: u32, samples: Arc<Mutex<VecDeque<f32>>>, alive: Arc<AtomicBool>) -> Result<()> {
    let _ = wasapi::initialize_mta().ok();
    let mut client = wasapi::AudioClient::new_application_loopback_client(pid, true)?;
    let format = wasapi::WaveFormat::new(
        32,
        32,
        wasapi::SampleType::Float,
        RATE as usize,
        CHANNELS,
        None,
    );
    client.initialize_client(
        &format,
        wasapi::Direction::Capture,
        wasapi::StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: 200_000,
        },
        None,
    )?;
    let event = client.set_get_eventhandle()?;
    let reader = client.get_audiocaptureclient()?;
    client.start_stream()?;
    let mut bytes = VecDeque::new();
    while alive.load(Ordering::Relaxed) {
        if event.wait_for_event(500).is_err() {
            continue;
        }
        reader.read_from_device_to_deque(&mut bytes)?;
        let mut queue = samples.lock().unwrap();
        while bytes.len() >= 4 {
            let b = [bytes[0], bytes[1], bytes[2], bytes[3]];
            bytes.drain(..4);
            queue.push_back(f32::from_le_bytes(b));
        }
        if queue.len() > SOURCE_MAX {
            let excess = (queue.len() - SOURCE_MAX + CHANNELS - 1) / CHANNELS * CHANNELS;
            let excess = excess.min(queue.len());
            queue.drain(..excess);
        }
    }
    let _ = client.stop_stream();
    Ok(())
}

pub fn run(args: &[std::ffi::OsString]) -> Result<()> {
    let (mut share, mut bitrate) = (false, 128_000i32);
    let mut excluded: Vec<String> = EXCLUDED.iter().map(|s| s.to_string()).collect();
    let mut positional = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let text = arg.to_str().context("non-UTF-8 argument")?;
        match text {
            "--iroh-share" => share = true,
            "--bitrate" => {
                bitrate = rest
                    .next()
                    .context("--bitrate needs a value")?
                    .to_str()
                    .context("bitrate")?
                    .parse()?
            }
            "--exclude" => excluded.push(
                rest.next()
                    .context("--exclude needs a name")?
                    .to_str()
                    .context("name")?
                    .to_lowercase(),
            ),
            _ if text.starts_with("--") => bail!("unknown option {text}\n{USAGE}"),
            _ => positional.push(text.to_owned()),
        }
    }
    if positional.len() > 1 || !(16_000..=510_000).contains(&bitrate) {
        bail!("{USAGE}");
    }
    let bind = positional
        .first()
        .map(String::as_str)
        .unwrap_or("127.0.0.1:0")
        .parse()?;
    let _ = wasapi::initialize_mta().ok();
    // Windows sleeps in 15.6 ms steps by default, which would send 20 ms packets in bursts.
    unsafe {
        windows::Win32::Media::timeBeginPeriod(1);
    }
    let mut publisher = AudioPublisher::new(
        bind,
        if share {
            Transport::Peer
        } else {
            Transport::Browser
        },
        bitrate,
    )?;
    println!(
        "MOQ_READY listen={bind} fingerprint={}",
        publisher.fingerprint
    );
    println!("AUDIO_SOURCE rate={RATE} channels={CHANNELS} codec=opus");

    let mut sources: HashMap<u32, Source> = HashMap::new();
    let mut scanned = Instant::now() - Duration::from_secs(10);
    let start = Instant::now();
    let packet = Duration::from_millis(20);
    let mut next = Instant::now();
    let mut mix = vec![0f32; FRAME * CHANNELS];
    let (mut published, mut bytes, mut reported) = (0u64, 0u64, Instant::now());
    loop {
        if scanned.elapsed() >= Duration::from_secs(2) {
            scanned = Instant::now();
            let all = processes();
            let chosen: HashSet<u32> = match session_pids() {
                Ok(sessions) => choose(&sessions, &all, &excluded).into_iter().collect(),
                Err(e) => {
                    eprintln!("audio sessions: {e}");
                    sources.keys().copied().collect()
                }
            };
            sources.retain(|pid, source| {
                let keep = chosen.contains(pid) && all.contains_key(pid);
                if !keep {
                    source.alive.store(false, Ordering::Relaxed);
                    println!("AUDIO_RELEASE pid={pid} name={}", source.name);
                }
                keep
            });
            for pid in chosen {
                if sources.contains_key(&pid) {
                    continue;
                }
                let name = all.get(&pid).map(|(_, n)| n.clone()).unwrap_or_default();
                let samples = Arc::new(Mutex::new(VecDeque::new()));
                let alive = Arc::new(AtomicBool::new(true));
                let (s, a, n) = (samples.clone(), alive.clone(), name.clone());
                std::thread::spawn(move || {
                    if let Err(e) = capture(pid, s, a) {
                        eprintln!("audio capture {pid} ({n}): {e}");
                    }
                });
                println!("AUDIO_CAPTURE pid={pid} name={name}");
                sources.insert(
                    pid,
                    Source {
                        name,
                        samples,
                        alive,
                    },
                );
            }
        }
        // One packet per 20 ms of wall clock: each source gives what it has, silence if not.
        let now = Instant::now();
        if now < next {
            std::thread::sleep(next - now);
        }
        next += packet;
        if Instant::now() > next + packet * 5 {
            // Fell far behind (a stall): start the clock over rather than burst.
            next = Instant::now() + packet;
        }
        mix.fill(0.0);
        for source in sources.values() {
            let mut queue = source.samples.lock().unwrap();
            let take = queue.len().min(mix.len());
            for (m, s) in mix.iter_mut().zip(queue.drain(..take)) {
                *m += s;
            }
        }
        for m in mix.iter_mut() {
            *m = m.clamp(-1.0, 1.0);
        }
        bytes += publisher.publish(&mix, start.elapsed().as_micros() as u64)? as u64;
        published += 1;
        if reported.elapsed() >= Duration::from_secs(5) {
            println!(
                "AUDIO_STATS sources={} packets={published} kbps={}",
                sources.len(),
                bytes * 8 / 1000 / reported.elapsed().as_secs().max(1)
            );
            bytes = 0;
            reported = Instant::now();
        }
    }
}
