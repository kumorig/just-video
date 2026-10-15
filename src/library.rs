//! Background SMB work for the headset browser. Network calls never run on the
//! XR frame loop: requests go to worker threads and results come back on a
//! channel. Navigation and opening use one worker, playability probes another,
//! so a slow probe never delays browsing. Requests for the headset's own
//! storage ([`local`]) take the same path, served from the filesystem.

use crate::config::{self, LayoutOverride, Server};
use crate::local;
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
    /// Fetches a subtitle file found when the video opened.
    Subtitle {
        id: u64,
        server: Server,
        share: String,
        path: Path,
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
    /// Subtitle files found for the video, fetched when chosen.
    pub external_subtitles: Vec<ExternalSubtitles>,
    /// Where the video is, for fetching its subtitle files.
    pub server: Server,
    pub share: String,
    /// Saved picture corrections for this file.
    pub image: config::ImageAdjust,
    /// Where this file was left last time (seconds), to continue from.
    pub resume: Option<f64>,
    /// The network reader's buffer, for the seek bar (none for a local file).
    pub buffer: Option<crate::readahead::BufferWatch>,
    /// Its reserve, for buffering ahead while paused.
    pub reserve: Option<std::sync::Arc<crate::readahead::Reserve>>,
}

pub struct ExternalSubtitles {
    /// File name, e.g. `movie.en.srt` or `2_English.srt`.
    pub name: String,
    /// How it is listed: its language where the name says it.
    pub label: String,
    /// The file's path in the share.
    pub path: Path,
}

/// Finds the .srt files for the video at `path`: next to it (`movie.srt`,
/// `movie.en.srt`), in a `Subs` folder there, and in that folder's
/// subfolder named after the video (season packs). Nothing is read yet:
/// a pack holds dozens of languages, and opening must stay quick. A
/// folder that can't be listed is skipped: subtitles must never stop a
/// video from playing. `list_folder` lists a folder of the share.
fn find_sidecars(
    path: &Path,
    list_folder: impl Fn(&Path) -> anyhow::Result<Vec<Entry>>,
) -> Vec<ExternalSubtitles> {
    let Some((video, folder)) = path.split_last() else {
        return Vec::new();
    };
    let list = |parts: Vec<String>| match list_folder(&parts) {
        Ok(entries) => Some(entries),
        Err(e) => {
            eprintln!("Subtitles: can't list {}: {e:#}", parts.join("/"));
            None
        }
    };
    let Some(here) = list(folder.to_vec()) else {
        return Vec::new();
    };
    let subs = here
        .iter()
        .find(|e| e.is_dir && crate::subtitles::is_subs_folder(&e.name))
        .map(|e| e.name.clone())
        .and_then(|dir| {
            let mut parts = folder.to_vec();
            parts.push(dir.clone());
            list(parts).map(|entries| (dir, entries))
        });
    let stem = crate::subtitles::stem(video);
    let own = subs.as_ref().and_then(|(dir, entries)| {
        let sub = entries
            .iter()
            .find(|e| e.is_dir && e.name.eq_ignore_ascii_case(stem))?
            .name
            .clone();
        let mut parts = folder.to_vec();
        parts.push(dir.clone());
        parts.push(sub.clone());
        list(parts).map(|entries| (sub, entries))
    });
    choose_sidecars(
        video,
        &here,
        subs.as_ref().map(|(d, e)| (d.as_str(), e.as_slice())),
        own.as_ref().map(|(d, e)| (d.as_str(), e.as_slice())),
    )
    .into_iter()
    .map(|(label, relative)| {
        let name = relative.last().cloned().unwrap_or_default();
        let mut path = folder.to_vec();
        path.extend(relative);
        ExternalSubtitles { name, label, path }
    })
    .collect()
}

/// The subtitle files for `video` among the listings of its folder, of the
/// `Subs` folder there and of that folder's subfolder named after the
/// video: (label, path relative to the video's folder), in listing order.
pub(crate) fn choose_sidecars(
    video: &str,
    here: &[Entry],
    subs: Option<(&str, &[Entry])>,
    own: Option<(&str, &[Entry])>,
) -> Vec<(String, Vec<String>)> {
    use crate::subtitles::{distinct_labels, is_sidecar, is_srt, sidecar_label, sidecar_order};
    let small = |e: &&Entry| !e.is_dir && e.size <= 8 << 20;
    let mut files: Vec<(Vec<String>, String)> = here
        .iter()
        .filter(small)
        .filter(|e| is_sidecar(video, &e.name))
        .map(|e| (Vec::new(), e.name.clone()))
        .collect();
    if let Some((dir, entries)) = subs {
        files.extend(
            entries
                .iter()
                .filter(small)
                .filter(|e| is_sidecar(video, &e.name))
                .map(|e| (vec![dir.to_string()], e.name.clone())),
        );
        if let Some((sub, entries)) = own {
            files.extend(
                entries
                    .iter()
                    .filter(small)
                    .filter(|e| is_srt(&e.name))
                    .map(|e| (vec![dir.to_string(), sub.to_string()], e.name.clone())),
            );
        }
    }
    files.sort_by_cached_key(|(_, name)| sidecar_order(name));
    let labels = distinct_labels(
        files
            .iter()
            .map(|(_, name)| sidecar_label(video, name))
            .collect(),
    );
    files
        .into_iter()
        .zip(labels)
        .map(|((mut parts, name), label)| {
            parts.push(name);
            (label, parts)
        })
        .collect()
}

/// Reads and parses one subtitle file.
fn read_subtitles(
    session: &SmbSession,
    share: &str,
    path: &Path,
) -> anyhow::Result<Vec<crate::subtitles::Cue>> {
    use std::io::Read;
    let small = ReadAhead {
        block_size: 256 * 1024,
        blocks_ahead: 4,
        reserve_bytes: 0,
    };
    let mut bytes = Vec::new();
    session
        .open_in(share, &smb_path(path), small)?
        .read_to_end(&mut bytes)?;
    parse_subtitles(&bytes)
}

fn parse_subtitles(bytes: &[u8]) -> anyhow::Result<Vec<crate::subtitles::Cue>> {
    let cues = crate::subtitles::parse_srt(&crate::subtitles::decode_text(bytes));
    anyhow::ensure!(!cues.is_empty(), "No subtitles in this file.");
    Ok(cues)
}

/// Judges how well the file at `path` will play, from its headers.
fn probe(path: &Path, reader: impl crate::media::Source + 'static) -> Result<Assessment, String> {
    Media::open(path.last().map_or("", String::as_str), reader)
        .map(|m| playability::assess(Platform::current(), m.info().video.as_ref()))
        .map_err(|e| crate::smb::explain(&e))
}

/// Opens the video at `path` for playback, with its saved layout, picture
/// corrections and resume point.
#[allow(clippy::too_many_arguments)]
fn open_video(
    server: &Server,
    share: &str,
    path: &Path,
    reader: impl crate::media::Source + 'static,
    external_subtitles: Vec<ExternalSubtitles>,
    buffer: Option<crate::readahead::BufferWatch>,
    reserve: Option<Arc<crate::readahead::Reserve>>,
    hw: Option<&str>,
) -> Result<Box<Opened>, String> {
    let err = |e: anyhow::Error| crate::smb::explain(&e);
    let key = file_key(server, share, path);
    let media = Media::open(path.last().map_or("", String::as_str), reader).map_err(err)?;
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
        server: server.clone(),
        share: share.to_string(),
        image,
        resume,
        buffer,
        reserve,
    }))
}

/// Keeps a renamed file's saved VR layout and resume point with it.
fn keep_saved_state(server: &Server, share: &str, path: &Path, new_name: &str) {
    let mut renamed = path.clone();
    if let Some(last) = renamed.last_mut() {
        *last = new_name.to_string();
    }
    let (from, to) = (
        file_key(server, share, path),
        file_key(server, share, &renamed),
    );
    let _ = config::move_layout_override(&from, &to);
    let _ = config::move_resume_position(&from, &to);
}

/// Forgets a deleted file's saved VR layout and resume point.
fn forget_saved_state(server: &Server, share: &str, path: &Path) {
    let key = file_key(server, share, path);
    let _ = config::save_layout_override(&key, None);
    let _ = config::save_resume_position(&key, None);
}

/// Moves `path` to `target` on the saved-settings side, after the file moved.
fn moved_saved_state(server: &Server, share: &str, path: &Path, target: &Path) {
    let from = file_key(server, share, path);
    let to = file_key(server, share, target);
    // The move has already succeeded: a settings failure must never put
    // this source back in the retry list.
    if let Err(e) = config::move_file_state(&from, &to) {
        eprintln!(
            "Moved {} but couldn't update saved settings: {e:#}",
            path.join("/")
        );
    }
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
    Subtitle {
        id: u64,
        result: Result<Vec<crate::subtitles::Cue>, String>,
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
        Request::Subtitle { server, .. } => (server.clone(), "subtitle"),
    };
    let response = if local::is_local(&server) {
        run_local(request, hw)
    } else {
        run(request, sessions, hw)
    };
    let failure = match &response {
        Response::Shares { result: Err(e), .. }
        | Response::List { result: Err(e), .. }
        | Response::Opened { result: Err(e), .. }
        | Response::Changed { result: Err(e), .. }
        | Response::ServerAdded { result: Err(e), .. }
        | Response::Subtitle { result: Err(e), .. } => Some(e.clone()),
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
                self::probe(&path, reader)
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
            let result = connect(&server, None).and_then(|s| {
                let external_subtitles =
                    find_sidecars(&path, |folder| s.list_in(&share, &smb_path(folder)));
                let read_ahead = ReadAhead {
                    reserve_bytes: config::reserve_bytes(),
                    ..ReadAhead::default()
                };
                let reader = s
                    .open_owned(&share, &smb_path(&path), read_ahead)
                    .map_err(err)?;
                let (buffer, reserve) = (Some(reader.watch()), Some(reader.reserve()));
                open_video(
                    &server,
                    &share,
                    &path,
                    reader,
                    external_subtitles,
                    buffer,
                    reserve,
                    hw,
                )
            });
            Response::Opened { id, result }
        }
        Request::Subtitle {
            id,
            server,
            share,
            path,
        } => Response::Subtitle {
            id,
            result: with_session(sessions, &server, Purpose::Browse, |s| {
                read_subtitles(s, &share, &path).map_err(err)
            }),
        },
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
                keep_saved_state(&server, &share, &path, &new_name);
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
                forget_saved_state(&server, &share, &path);
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
                moved_saved_state(&selection.server, &selection.share, path, target);
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

/// [`run`] for the headset's own storage: the same requests, on local files.
fn run_local(request: Request, hw: Option<&str>) -> Response {
    let err = |e: anyhow::Error| format!("{e:#}");
    match request {
        Request::Shares { id, .. } => Response::Shares {
            id,
            result: Ok(local::shares()),
        },
        Request::List {
            id,
            share,
            path,
            watch,
            ..
        } => {
            let result = local::list(&share, &path).map_err(err);
            if let Ok(entries) = &result {
                watch.count.store(entries.len(), Ordering::Relaxed);
            }
            Response::List { id, result }
        }
        Request::Probe {
            generation,
            share,
            path,
            ..
        } => Response::Probe {
            generation,
            name: path.last().cloned().unwrap_or_default(),
            result: local::open(&share, &path)
                .map_err(err)
                .and_then(|reader| probe(&path, reader)),
        },
        Request::Open {
            id,
            server,
            share,
            path,
        } => {
            let external_subtitles = find_sidecars(&path, |folder| local::list(&share, folder));
            let result = local::open(&share, &path).map_err(err).and_then(|reader| {
                open_video(
                    &server,
                    &share,
                    &path,
                    reader,
                    external_subtitles,
                    None,
                    None,
                    hw,
                )
            });
            Response::Opened { id, result }
        }
        Request::Subtitle {
            id, share, path, ..
        } => Response::Subtitle {
            id,
            result: local::resolve(&share, &path)
                .and_then(|file| Ok(std::fs::read(file)?))
                .and_then(|bytes| parse_subtitles(&bytes))
                .map_err(err),
        },
        Request::Rename {
            id,
            server,
            share,
            path,
            new_name,
        } => {
            let result = local::rename(&share, &path, &new_name)
                .map(|()| keep_saved_state(&server, &share, &path, &new_name))
                .map_err(err);
            Response::Changed { id, result }
        }
        Request::Delete {
            id,
            server,
            share,
            path,
        } => {
            let result = local::delete(&share, &path)
                .map(|()| forget_saved_state(&server, &share, &path))
                .map_err(err);
            Response::Changed { id, result }
        }
        Request::Move {
            id,
            selection,
            destination,
        } => {
            let result = move_entries(&selection, &destination, |path, target| {
                local::move_entry(&selection.share, path, target).map_err(err)?;
                moved_saved_state(&selection.server, &selection.share, path, target);
                Ok(())
            });
            Response::Moved { id, result }
        }
        Request::AddServer { id, .. } => Response::ServerAdded {
            id,
            result: Err("The headset's storage is built in and can't be added".into()),
        },
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

    fn entry(name: &str, is_dir: bool) -> Entry {
        Entry {
            name: name.into(),
            is_dir,
            size: if is_dir { 0 } else { 90_000 },
        }
    }

    #[test]
    fn season_pack_subtitles_are_found_and_labelled() {
        let video = "Show.S01E01.1080p.WEBRip.x265-RARBG[eztv.re].mp4";
        let stem = "Show.S01E01.1080p.WEBRip.x265-RARBG[eztv.re]";
        let here = [
            entry("Show.S01E01.1080p.WEBRip.x265-RARBG[eztv.re].mp4", false),
            entry("Show.S01E02.1080p.WEBRip.x265-RARBG[eztv.re].mp4", false),
            entry("Subs", true),
        ];
        let subs = [
            entry(stem, true),
            entry("Show.S01E02.1080p.WEBRip.x265-RARBG[eztv.re]", true),
        ];
        let own = [
            entry("43_Chinese.srt", false),
            entry("4_Arabic.srt", false),
            entry("9_Danish.srt", false),
            entry("3_English.srt", false),
            entry("2_English.srt", false),
            entry("29_nor.srt", false),
            entry("34_slo.srt", false),
            entry("35_Slovenian.srt", false),
            entry("readme.txt", false),
        ];
        let chosen = choose_sidecars(video, &here, Some(("Subs", &subs)), Some((stem, &own)));
        let labels: Vec<&str> = chosen.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            [
                "English",
                "English (2)",
                "Arabic",
                "Danish",
                "Norwegian",
                "Slovak",
                "Slovenian",
                "Chinese"
            ]
        );
        assert_eq!(chosen[0].1, ["Subs", stem, "2_English.srt"]);
        // A file beside the video still counts, and comes after the numbered ones.
        let here = [
            entry("movie.mkv", false),
            entry("movie.en.srt", false),
            entry("movie.srt", false),
            entry("other.srt", false),
        ];
        let chosen = choose_sidecars("movie.mkv", &here, None, None);
        let labels: Vec<&str> = chosen.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["Subtitles", "English"]);
        assert_eq!(chosen[1].1, ["movie.en.srt"]);
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
