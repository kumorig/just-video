//! Background SMB work for the headset browser. Network calls never run on the
//! XR frame loop: requests go to worker threads and results come back on a
//! channel. Navigation and opening use one worker, playability probes another,
//! so a slow probe never delays browsing.

use crate::config::{self, LayoutOverride, Server};
use crate::media::{Media, VideoDecoder};
use crate::playability::{self, Assessment, Platform};
use crate::readahead::ReadAhead;
use crate::smb::{Entry, SmbSession, SmbUrl};
use crate::vr::{self, Layout};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
};

pub type Path = Vec<String>;

/// Watches a listing in progress: entries received so far, and a flag that
/// stops it early (the browser left the folder).
#[derive(Clone, Debug, Default)]
pub struct Listing {
    pub count: Arc<AtomicUsize>,
    pub cancel: Arc<AtomicBool>,
}

/// Stable source paths retained while browsing for a move destination.
#[derive(Clone, Debug)]
pub struct MoveSelection {
    pub server: Server,
    pub share: String,
    pub paths: Vec<Path>,
}

impl MoveSelection {
    pub fn targets(
        &self,
        server: &Server,
        share: &str,
        destination: &Path,
    ) -> Result<Vec<Path>, String> {
        if server.url != self.server.url || !share.eq_ignore_ascii_case(&self.share) {
            return Err("Same share only.".into());
        }
        if self.paths.is_empty() {
            return Err("Nothing selected.".into());
        }
        self.paths
            .iter()
            .map(|path| {
                crate::smb::move_target(&smb_path(path), &smb_path(destination))
                    .map(|target| target.split('\\').map(str::to_string).collect())
                    .map_err(|e| e.to_string())
            })
            .collect()
    }
}

#[derive(Default)]
pub struct MoveResult {
    /// Only failed sources remain available for another destination or retry.
    pub failed: Vec<Path>,
    pub errors: Vec<String>,
}

pub enum Request {
    Shares {
        id: u64,
        server: Server,
    },
    List {
        id: u64,
        server: Server,
        share: String,
        path: Path,
        watch: Listing,
    },
    Probe {
        generation: u64,
        server: Server,
        share: String,
        path: Path,
    },
    Open {
        id: u64,
        server: Server,
        share: String,
        path: Path,
    },
    Rename {
        id: u64,
        server: Server,
        share: String,
        path: Path,
        new_name: String,
    },
    Delete {
        id: u64,
        server: Server,
        share: String,
        path: Path,
    },
    Move {
        id: u64,
        selection: MoveSelection,
        destination: Path,
    },
    /// Checks the login (and that shares can be listed), then saves the server.
    AddServer {
        id: u64,
        server: Server,
        password: String,
    },
}

pub struct Opened {
    pub decoder: VideoDecoder,
    pub layout: Layout,
    pub assessment: Assessment,
    pub name: String,
    /// Identifies the file for its saved layout and resume point.
    pub key: String,
    /// Subtitle files next to the video (`movie.srt`, `movie.en.srt`).
    pub external_subtitles: Vec<ExternalSubtitles>,
    /// Saved picture corrections for this file.
    pub image: config::ImageAdjust,
    /// Where this file was left last time (seconds), to continue from.
    pub resume: Option<f64>,
    /// The network reader's buffer, for the seek bar.
    pub buffer: crate::readahead::BufferWatch,
    /// Its reserve, for buffering ahead while paused.
    pub reserve: std::sync::Arc<crate::readahead::Reserve>,
}

pub struct ExternalSubtitles {
    /// File name, e.g. `movie.en.srt`.
    pub name: String,
    pub cues: Vec<crate::subtitles::Cue>,
}

/// Loads the .srt files next to `path`. Missing or unreadable ones are
/// skipped: subtitles must never stop a video from playing.
fn load_sidecars(session: &SmbSession, share: &str, path: &Path) -> Vec<ExternalSubtitles> {
    use std::io::Read;
    let Some((video, folder)) = path.split_last() else {
        return Vec::new();
    };
    let entries = match session.list_in(share, &smb_path(&folder.to_vec())) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("Subtitles: can't list the folder: {e:#}");
            return Vec::new();
        }
    };
    let mut names: Vec<String> = entries
        .into_iter()
        .filter(|e| !e.is_dir && e.size <= 8 << 20 && crate::subtitles::is_sidecar(video, &e.name))
        .map(|e| e.name)
        .collect();
    // `movie.srt` first, then language variants.
    names.sort_by_key(|n| (n.len(), n.clone()));
    let small = ReadAhead {
        block_size: 256 * 1024,
        blocks_ahead: 4,
        reserve_bytes: 0,
    };
    names
        .into_iter()
        .filter_map(|name| {
            let mut file = folder.to_vec();
            file.push(name.clone());
            let mut bytes = Vec::new();
            let read = session
                .open_in(share, &smb_path(&file), small)
                .and_then(|mut r| Ok(r.read_to_end(&mut bytes)?));
            if let Err(e) = read {
                eprintln!("Subtitles: can't read {name}: {e:#}");
                return None;
            }
            let cues = crate::subtitles::parse_srt(&crate::subtitles::decode_text(&bytes));
            eprintln!("Subtitles: {name}: {} cues", cues.len());
            (!cues.is_empty()).then_some(ExternalSubtitles { name, cues })
        })
        .collect()
}

pub enum Response {
    Shares {
        id: u64,
        result: Result<Vec<String>, String>,
    },
    List {
        id: u64,
        result: Result<Vec<Entry>, String>,
    },
    Probe {
        generation: u64,
        name: String,
        result: Result<Assessment, String>,
    },
    Opened {
        id: u64,
        result: Result<Box<Opened>, String>,
    },
    /// Rename or delete finished.
    Changed {
        id: u64,
        result: Result<(), String>,
    },
    Moved {
        id: u64,
        result: MoveResult,
    },
    ServerAdded {
        id: u64,
        result: Result<Server, String>,
    },
}

pub struct Library {
    main: mpsc::Sender<Request>,
    probes: mpsc::Sender<Request>,
    responses: mpsc::Receiver<Response>,
    /// Probes for other generations (folders left behind) are skipped.
    probe_generation: Arc<AtomicU64>,
}

/// What a connection is used for. Browsing and playability probes keep one
/// connection each per server; every playing video gets its own (see Open).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Purpose {
    Browse,
    Probe,
}

type Sessions = Arc<Mutex<HashMap<(String, Purpose), Arc<SmbSession>>>>;

/// Key of a file's saved layout override.
pub fn file_key(server: &Server, share: &str, path: &Path) -> String {
    format!("{}/{share}/{}", server.url, path.join("/"))
}

fn connect(server: &Server, password: Option<String>) -> Result<Arc<SmbSession>, String> {
    let url: SmbUrl = server.url.parse().map_err(|e| format!("{e:#}"))?;
    let password = match password {
        Some(p) => p,
        None => config::password(&server.url)
            .map_err(|e| format!("{e:#}"))?
            .unwrap_or_default(),
    };
    Ok(Arc::new(SmbSession::connect(url, password).map_err(
        |e| {
            format!(
                "Can't connect to {}: {}",
                server.name,
                crate::smb::explain(&e)
            )
        },
    )?))
}

fn session(
    sessions: &Sessions,
    server: &Server,
    purpose: Purpose,
) -> Result<Arc<SmbSession>, String> {
    session_kept(sessions, server, purpose).map(|(s, _)| s)
}

/// The server's connection, and whether it was already open (kept from an
/// earlier request) rather than made just now.
fn session_kept(
    sessions: &Sessions,
    server: &Server,
    purpose: Purpose,
) -> Result<(Arc<SmbSession>, bool), String> {
    let key = (server.url.clone(), purpose);
    if let Some(s) = sessions.lock().expect("sessions").get(&key) {
        return Ok((s.clone(), true));
    }
    let session = connect(server, None)?;
    sessions
        .lock()
        .expect("sessions")
        .insert(key, session.clone());
    Ok((session, false))
}

/// Runs `request` on the server's connection. A kept connection may have
/// died meanwhile (the headset slept, the Wi-Fi dropped, the server closed
/// it) and then fails every request until the app restarts; so when it
/// fails without the server having answered, reconnect and try once more.
fn with_session<T>(
    sessions: &Sessions,
    server: &Server,
    purpose: Purpose,
    request: impl Fn(&SmbSession) -> Result<T, String>,
) -> Result<T, String> {
    let (session, kept) = session_kept(sessions, server, purpose)?;
    match request(&session) {
        Err(e) if kept && !server_answered(&e) => {
            eprintln!(
                "Library: {} failed on the kept connection to {} ({e}); reconnecting",
                format!("{purpose:?}").to_lowercase(),
                server.name
            );
            evict(sessions, server, purpose);
            let session = connect(server, None)?;
            sessions
                .lock()
                .expect("sessions")
                .insert((server.url.clone(), purpose), session.clone());
            request(&session)
        }
        result => result,
    }
}

/// Whether an error came from the server itself (a status reply, a refusal,
/// a cancelled or timed-out request), as opposed to the connection failing.
fn server_answered(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    [
        "status",
        "did not answer",
        "cancelled",
        "not a file",
        "is not a file",
    ]
    .iter()
    .any(|s| e.contains(s))
}

/// Forgets a server connection after a failure so the next request reconnects;
/// a broken connection must never wedge browsing until the app restarts.
fn evict(sessions: &Sessions, server: &Server, purpose: Purpose) {
    // Dropping a wedged session can block on its logoff; never here.
    if let Some(session) = sessions
        .lock()
        .expect("sessions")
        .remove(&(server.url.clone(), purpose))
    {
        eprintln!(
            "Library: reconnecting to {} ({purpose:?}) on next request",
            server.name
        );
        crate::xr::app::drop_in_background(session);
    }
}

fn smb_path(path: &Path) -> String {
    path.join("\\")
}

fn handle(request: Request, sessions: &Sessions, hw: Option<&str>) -> Response {
    let started = std::time::Instant::now();
    let (server, what) = match &request {
        Request::Shares { server, .. } => (server.clone(), "shares"),
        Request::List { server, .. } => (server.clone(), "list"),
        Request::Probe { server, .. } => (server.clone(), "probe"),
        Request::Open { server, .. } => (server.clone(), "open"),
        Request::Rename { server, .. } => (server.clone(), "rename"),
        Request::Delete { server, .. } => (server.clone(), "delete"),
        Request::Move { selection, .. } => (selection.server.clone(), "move"),
        Request::AddServer { server, .. } => (server.clone(), "add server"),
    };
    let response = run(request, sessions, hw);
    let failure = match &response {
        Response::Shares { result: Err(e), .. }
        | Response::List { result: Err(e), .. }
        | Response::Opened { result: Err(e), .. }
        | Response::Changed { result: Err(e), .. }
        | Response::ServerAdded { result: Err(e), .. } => Some(e.clone()),
        // A bad file is not a connection problem; a stalled server is.
        Response::Probe { result: Err(e), .. }
            if e.contains("stopped sending") || e.contains("did not answer") =>
        {
            Some(e.clone())
        }
        Response::Moved { result, .. } => result
            .errors
            .iter()
            .find(|e| e.contains("stopped sending") || e.contains("did not answer"))
            .cloned(),
        _ => None,
    };
    if let Some(e) = failure {
        eprintln!(
            "Library: {what} failed after {:.1}s: {e}",
            started.elapsed().as_secs_f64()
        );
        // Only unanswered requests mean the connection itself is broken.
        if e.contains("did not answer") || e.contains("stopped sending") {
            match what {
                "probe" => evict(sessions, &server, Purpose::Probe),
                // A video has its own connection, which closes with it.
                "open" | "add server" => {}
                _ => evict(sessions, &server, Purpose::Browse),
            }
        }
    } else if started.elapsed().as_secs_f64() > 2.0 {
        eprintln!(
            "Library: {what} took {:.1}s",
            started.elapsed().as_secs_f64()
        );
    }
    response
}

fn run(request: Request, sessions: &Sessions, hw: Option<&str>) -> Response {
    let err = |e: anyhow::Error| crate::smb::explain(&e);
    match request {
        Request::Shares { id, server } => Response::Shares {
            id,
            result: with_session(sessions, &server, Purpose::Browse, |s| {
                s.shares().map_err(err)
            }),
        },
        Request::List {
            id,
            server,
            share,
            path,
            watch,
        } => Response::List {
            id,
            result: with_session(sessions, &server, Purpose::Browse, |s| {
                s.list_in_watched(&share, &smb_path(&path), &mut |n| {
                    watch.count.store(n, Ordering::Relaxed);
                    !watch.cancel.load(Ordering::Relaxed)
                })
                .map_err(err)
            }),
        },
        Request::Probe {
            generation,
            server,
            share,
            path,
        } => {
            // Header probes read little: small blocks, shallow read-ahead.
            let probe = ReadAhead {
                block_size: 256 * 1024,
                blocks_ahead: 4,
                reserve_bytes: 0,
            };
            let result = with_session(sessions, &server, Purpose::Probe, |s| {
                let reader = s.open_in(&share, &smb_path(&path), probe).map_err(err)?;
                Media::open(path.last().map_or("", String::as_str), reader)
                    .map(|m| playability::assess(Platform::current(), m.info().video.as_ref()))
                    .map_err(err)
            });
            Response::Probe {
                generation,
                name: path.last().cloned().unwrap_or_default(),
                result,
            }
        }
        Request::Open {
            id,
            server,
            share,
            path,
        } => {
            // A dedicated connection per video: if it wedges, only this video
            // is affected, and it closes when the video does.
            let key = file_key(&server, &share, &path);
            let result = connect(&server, None).and_then(|s| {
                let external_subtitles = load_sidecars(&s, &share, &path);
                let read_ahead = ReadAhead {
                    reserve_bytes: config::reserve_bytes(),
                    ..ReadAhead::default()
                };
                let reader = s
                    .open_owned(&share, &smb_path(&path), read_ahead)
                    .map_err(err)?;
                let buffer = reader.watch();
                let reserve = reader.reserve();
                let media =
                    Media::open(path.last().map_or("", String::as_str), reader).map_err(err)?;
                let name = path.last().cloned().unwrap_or_default();
                let video = media.info().video.clone();
                let assessment = playability::assess(Platform::current(), video.as_ref());
                let mut layout = vr::detect(&name, video.as_ref());
                let saved = config::layout_override(&key).ok().flatten();
                if let Some(saved) = saved {
                    saved.apply(&mut layout);
                }
                let image = saved.map(|s| s.image).unwrap_or_default();
                // The last video's decoder closes in the background; wait for
                // it, or the hardware decoder is still busy.
                if !crate::media::wait_for_decoders_closed(std::time::Duration::from_secs(10)) {
                    eprintln!("Library: the previous video's decoder is still closing");
                }
                let decoder = media.into_decoder(hw, true, "").map_err(err)?;
                let resume = config::resume_position(&key);
                Ok(Box::new(Opened {
                    decoder,
                    layout,
                    assessment,
                    name,
                    key,
                    external_subtitles,
                    image,
                    resume,
                    buffer,
                    reserve,
                }))
            });
            Response::Opened { id, result }
        }
        Request::Rename {
            id,
            server,
            share,
            path,
            new_name,
        } => {
            let result = with_session(sessions, &server, Purpose::Browse, |s| {
                s.rename_in(&share, &smb_path(&path), &new_name)
                    .map_err(err)?;
                // Keep a saved VR layout with the file.
                let mut renamed = path.clone();
                if let Some(last) = renamed.last_mut() {
                    *last = new_name.clone();
                }
                let (from, to) = (
                    file_key(&server, &share, &path),
                    file_key(&server, &share, &renamed),
                );
                let _ = config::move_layout_override(&from, &to);
                let _ = config::move_resume_position(&from, &to);
                Ok(())
            });
            Response::Changed { id, result }
        }
        Request::Delete {
            id,
            server,
            share,
            path,
        } => {
            let result = with_session(sessions, &server, Purpose::Browse, |s| {
                s.delete_in(&share, &smb_path(&path)).map_err(err)?;
                let key = file_key(&server, &share, &path);
                let _ = config::save_layout_override(&key, None);
                let _ = config::save_resume_position(&key, None);
                Ok(())
            });
            Response::Changed { id, result }
        }
        Request::Move {
            id,
            selection,
            destination,
        } => {
            let result = move_entries(&selection, &destination, |path, target| {
                let s = session(sessions, &selection.server, Purpose::Browse)?;
                s.move_in(&selection.share, &smb_path(path), &smb_path(&destination))
                    .map_err(err)?;
                let from = file_key(&selection.server, &selection.share, path);
                let to = file_key(&selection.server, &selection.share, target);
                // The remote move has already succeeded: a settings failure must
                // never put this source back in the retry list.
                if let Err(e) = config::move_file_state(&from, &to) {
                    eprintln!(
                        "Moved {} but couldn't update saved settings: {e:#}",
                        path.join("/")
                    );
                }
                Ok(())
            });
            Response::Moved { id, result }
        }
        Request::AddServer {
            id,
            server,
            password,
        } => {
            let result = connect(&server, Some(password.clone())).and_then(|s| {
                s.shares().map_err(err)?;
                config::save_server(server.clone(), &password).map_err(err)?;
                Ok(server)
            });
            Response::ServerAdded { id, result }
        }
    }
}

/// Validates the entire batch before changing files; a failed item never makes
/// successfully moved items eligible for retry. Stop on a wedged connection.
fn move_entries(
    selection: &MoveSelection,
    destination: &Path,
    mut move_one: impl FnMut(&Path, &Path) -> Result<(), String>,
) -> MoveResult {
    let targets = match selection.targets(&selection.server, &selection.share, destination) {
        Ok(targets) => targets,
        Err(e) => {
            return MoveResult {
                failed: selection.paths.clone(),
                errors: vec![e],
            };
        }
    };
    let mut result = MoveResult::default();
    for (i, (path, target)) in selection.paths.iter().zip(&targets).enumerate() {
        if let Err(e) = move_one(path, target) {
            result.failed.push(path.clone());
            let stalled = e.contains("did not answer") || e.contains("stopped sending");
            result
                .errors
                .push(format!("{}: {e}", path.last().map_or("", String::as_str)));
            if stalled {
                result.failed.extend_from_slice(&selection.paths[i + 1..]);
                break;
            }
        }
    }
    result
}

impl Library {
    #[cfg(test)]
    pub(crate) fn for_test() -> (Self, mpsc::Receiver<Request>, mpsc::Sender<Response>) {
        let (main, requests) = mpsc::channel();
        let (out, responses) = mpsc::channel();
        (
            Self {
                probes: main.clone(),
                main,
                responses,
                probe_generation: Default::default(),
            },
            requests,
            out,
        )
    }

    /// `hw` is the preferred hardware backend for playback (see `media::default_hw_backend`).
    pub fn start(hw: Option<&'static str>) -> Self {
        let sessions: Sessions = Default::default();
        let probe_generation = Arc::new(AtomicU64::new(0));
        let (response_tx, responses) = mpsc::channel();
        let mut senders = Vec::new();
        for name in ["library", "probe"] {
            let (tx, rx) = mpsc::channel::<Request>();
            let (sessions, out) = (sessions.clone(), response_tx.clone());
            let current = probe_generation.clone();
            std::thread::Builder::new()
                .name(name.into())
                .spawn(move || {
                    for request in rx {
                        if let Request::Probe { generation, .. } = &request
                            && *generation != current.load(Ordering::Relaxed)
                        {
                            continue;
                        }
                        if out.send(handle(request, &sessions, hw)).is_err() {
                            return;
                        }
                    }
                })
                .expect("spawn library worker");
            senders.push(tx);
        }
        let probes = senders.pop().expect("probe worker");
        let main = senders.pop().expect("main worker");
        Self {
            main,
            probes,
            responses,
            probe_generation,
        }
    }

    /// Only probes tagged with this generation will run from now on.
    pub fn set_probe_generation(&self, generation: u64) {
        self.probe_generation.store(generation, Ordering::Relaxed);
    }

    pub fn send(&self, request: Request) {
        let worker = if matches!(request, Request::Probe { .. }) {
            &self.probes
        } else {
            &self.main
        };
        let _ = worker.send(request);
    }

    pub fn try_recv(&self) -> Option<Response> {
        self.responses.try_recv().ok()
    }
}

impl LayoutOverride {
    pub fn apply(&self, layout: &mut Layout) {
        layout.projection = self.projection;
        layout.stereo = self.stereo;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection() -> MoveSelection {
        MoveSelection {
            server: Server {
                name: "NAS".into(),
                url: "smb://u@nas".into(),
            },
            share: "media".into(),
            paths: vec![
                vec!["old".into(), "a".into()],
                vec!["old".into(), "b".into()],
            ],
        }
    }

    #[test]
    fn moves_are_limited_to_the_source_share() {
        let selection = selection();
        let other = Server {
            url: "smb://u@other".into(),
            ..selection.server.clone()
        };
        assert!(selection.targets(&other, "media", &vec![]).is_err());
        assert!(
            selection
                .targets(&selection.server, "other", &vec![])
                .is_err()
        );
        assert!(
            selection
                .targets(&selection.server, "MEDIA", &vec![])
                .is_ok()
        );
    }

    #[test]
    fn invalid_destination_changes_nothing() {
        let selection = selection();
        let result = move_entries(
            &selection,
            &vec!["old".into(), "b".into(), "child".into()],
            |_, _| panic!("must validate all sources first"),
        );
        assert_eq!(result.failed, selection.paths);
        assert_eq!(result.errors.len(), 1);
    }

    #[test]
    fn partial_moves_retain_only_failed_sources() {
        let selection = selection();
        let mut calls = 0;
        let result = move_entries(&selection, &vec!["new".into()], |path, target| {
            calls += 1;
            assert_eq!(target[0], "new");
            if path.last().unwrap() == "b" {
                Err("Already exists.".into())
            } else {
                Ok(())
            }
        });
        assert_eq!(calls, 2);
        assert_eq!(result.failed, vec![selection.paths[1].clone()]);
        assert_eq!(result.errors, vec!["b: Already exists."]);
    }

    #[test]
    fn stalled_move_stops_the_batch() {
        let selection = selection();
        let mut calls = 0;
        let result = move_entries(&selection, &vec!["new".into()], |_, _| {
            calls += 1;
            Err("The server did not answer.".into())
        });
        assert_eq!(calls, 1);
        assert_eq!(result.failed, selection.paths);
    }
}
