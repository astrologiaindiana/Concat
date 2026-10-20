//! The Music page of the library (Astrologia Indiana): search the Mixkit
//! catalogue, hear a track in place, and bring it into the project the way
//! CapCut's music shelf does, without leaving the window.
//!
//! Mixkit's music is under one licence (Mixkit Stock Music Free License):
//! commercial use, no credit required. Each track brought in is written down
//! in the project's `musica/ORIGEM.md` all the same.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::rc::Rc;
use std::time::Duration;

use slint::{ComponentHandle, ModelRc, VecModel};

use crate::host::spawn_in_project;
use crate::panes::Msg;
use crate::panes::media_bin::MediaMsg;
use crate::studio::Studio;
use crate::ui::{App, Editor, MusicTrackData};

#[derive(Clone)]
struct Track {
    id: String,
    title: String,
    author: String,
    url: String,
    meta: String,
}

thread_local! {
    static TRACKS: RefCell<Vec<Track>> = RefCell::default();
    static ROWS: Rc<VecModel<MusicTrackData>> = Rc::new(VecModel::default());
    /// The preview playing, and the row it belongs to.
    static PLAYER: RefCell<Option<(usize, Child)>> = RefCell::default();
    static TICK: slint::Timer = slint::Timer::default();
}

/// Hands the page its rows and wires search and preview. Adding a track
/// needs the project, so that one is wired in `lib.rs` with the others.
pub fn bind(app: &App) {
    let editor = app.global::<Editor>();
    editor.set_music_tracks(ROWS.with(|rows| ModelRc::from(rows.clone())));

    let weak = app.as_weak();
    editor.on_music_search(move |query| {
        let weak = weak.clone();
        let query = query.to_string();
        status(&weak, "Buscando…");
        std::thread::spawn(move || {
            let found = search(&query);
            let _ = slint::invoke_from_event_loop(move || {
                stop();
                match found {
                    Ok(tracks) => {
                        status(&weak, &match tracks.len() {
                            0 => "Nada encontrado. A busca é em inglês: tente outra palavra ou um clima ao lado.".to_string(),
                            1 => "1 música".to_string(),
                            n => format!("{n} músicas"),
                        });
                        TRACKS.with(|t| *t.borrow_mut() = tracks);
                    }
                    Err(error) => status(&weak, &format!("Sem resposta da Mixkit: {error}")),
                }
                refresh();
            });
        });
    });

    let weak = app.as_weak();
    editor.on_music_play(move |row| play(row as usize, weak.clone()));

    // A preview that reached its end gives its row back its play button.
    TICK.with(|tick| {
        tick.start(slint::TimerMode::Repeated, Duration::from_millis(300), || {
            let ended = PLAYER.with(|p| {
                matches!(p.borrow_mut().as_mut().map(|(_, child)| child.try_wait()), Some(Ok(Some(_))))
            });
            if ended {
                PLAYER.with(|p| *p.borrow_mut() = None);
                refresh();
            }
        });
    });
}

/// The + on a row: download into `<project>/musica/`, note where it came
/// from, and import it into the bin.
pub fn add(studio: &mut Studio, row: usize) {
    let Some(track) = TRACKS.with(|t| t.borrow().get(row).cloned()) else {
        return;
    };
    let Some(dir) = studio.session.as_ref().map(|s| PathBuf::from(s.path()).join("musica")) else {
        return;
    };
    studio.notify(&format!("Baixando “{}”…", track.title), false);
    spawn_in_project(
        move || fetch(&track, &dir),
        |studio, _, _, got: Result<PathBuf, String>| match got {
            Ok(path) => studio.handle(Msg::Media(MediaMsg::Import(vec![path]))),
            Err(error) => studio.notify(&format!("Não deu para baixar a música: {error}"), true),
        },
    );
}

fn status(weak: &slint::Weak<App>, text: &str) {
    if let Some(app) = weak.upgrade() {
        app.global::<Editor>().set_music_status(text.into());
    }
}

fn refresh() {
    let playing = PLAYER.with(|p| p.borrow().as_ref().map(|(row, _)| *row));
    let rows: Vec<MusicTrackData> = TRACKS.with(|t| {
        t.borrow()
            .iter()
            .enumerate()
            .map(|(i, track)| MusicTrackData {
                title: track.title.as_str().into(),
                meta: track.meta.as_str().into(),
                playing: playing == Some(i),
            })
            .collect()
    });
    ROWS.with(|model| model.set_vec(rows));
}

fn stop() {
    PLAYER.with(|p| {
        if let Some((_, mut child)) = p.borrow_mut().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    });
}

fn play(row: usize, weak: slint::Weak<App>) {
    let current = PLAYER.with(|p| p.borrow().as_ref().map(|(r, _)| *r));
    stop();
    if current == Some(row) {
        refresh();
        return;
    }
    refresh();
    let Some(track) = TRACKS.with(|t| t.borrow().get(row).cloned()) else {
        return;
    };
    status(&weak, "Carregando a prévia…");
    std::thread::spawn(move || {
        let file = std::env::temp_dir().join(format!("concat-musica-{}.mp3", track.id));
        let got = if file.exists() { Ok(()) } else { download(&track.url, &file) };
        // shortcut: the preview plays through macOS's afplay, not the engine;
        // on another system, route it through the app's own audio output.
        let child = got.and_then(|()| Command::new("afplay").arg(&file).spawn().map_err(|e| e.to_string()));
        let _ = slint::invoke_from_event_loop(move || {
            match child {
                Ok(child) => {
                    stop();
                    PLAYER.with(|p| *p.borrow_mut() = Some((row, child)));
                    status(&weak, "");
                }
                Err(error) => status(&weak, &format!("Não deu para tocar: {error}")),
            }
            refresh();
        });
    });
}

/// curl and not an HTTP crate: it is on every Mac, and it falls back from
/// IPv6 to IPv4 where Python's client hung on Mixkit's file server.
fn curl(args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("curl")
        .args(["-sfL", "-m", "120", "-A", "Mozilla/5.0"])
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!("curl saiu com {}", out.status))
    }
}

/// Into a `.part` first, so a cut connection never leaves a file that looks whole.
fn download(url: &str, to: &Path) -> Result<(), String> {
    let part = to.with_extension("part");
    curl(&["-o", &*part.to_string_lossy(), url])?;
    std::fs::rename(&part, to).map_err(|e| e.to_string())
}

fn fetch(track: &Track, dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let name: String = format!("{} - {} - mixkit {}", track.title, track.author, track.id)
        .chars()
        .filter(|c| c.is_alphanumeric() || " -".contains(*c))
        .collect();
    let path = dir.join(format!("{name}.mp3"));
    if !path.exists() {
        // shortcut: Mixkit's music mp3s carry no cover art (checked 19/10/2026);
        // a source that embeds one needs it stripped, or the bin takes it as video.
        download(&track.url, &path)?;
        use std::io::Write;
        let line = format!(
            "- **{}** · {} · Mixkit Stock Music Free License, sem crédito obrigatório (https://mixkit.co/license/#musicFree) · {}\n",
            track.title, track.author, track.url
        );
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("ORIGEM.md"))
            .and_then(|mut f| f.write_all(line.as_bytes()))
            .map_err(|e| e.to_string())?;
    }
    Ok(path)
}

/// Mixkit's own search (`/discover/<words>/`), which answers in a second and
/// lands on the genre or mood page when the word is one.
fn search(query: &str) -> Result<Vec<Track>, String> {
    let slug = query
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let url = if slug.is_empty() {
        "https://mixkit.co/free-stock-music/".to_string()
    } else {
        format!("https://mixkit.co/free-stock-music/discover/{slug}/")
    };
    let first = page(&url, 1)?;
    // Every page of the listing, not only the first 36: the rest fetched
    // side by side, so a mood of 250 tracks still answers in a second or two.
    let pages = first
        .split("aria-label=\"Go to page ")
        .skip(1)
        .filter_map(|p| p.split('"').next()?.parse::<usize>().ok())
        .max()
        .unwrap_or(1)
        .min(MAX_PAGES);
    let rest: Vec<_> = (2..=pages)
        .map(|n| {
            let url = url.clone();
            std::thread::spawn(move || page(&url, n))
        })
        .collect();
    let mut tracks = cards(&first);
    for handle in rest {
        if let Ok(Ok(html)) = handle.join() {
            tracks.extend(cards(&html));
        }
    }
    Ok(tracks)
}

/// 360 tracks. The page draws every row it is given (no virtual list), and a
/// word with more than this is too broad to browse anyway.
const MAX_PAGES: usize = 10;

fn page(url: &str, n: usize) -> Result<String, String> {
    Ok(String::from_utf8_lossy(&curl(&[format!("{url}?page={n}").as_str()])?).into_owned())
}

fn cards(html: &str) -> Vec<Track> {
    html.split("<div class=\"item-grid__item\">").skip(1).filter_map(card).collect()
}

fn between<'a>(s: &'a str, from: &str, to: &str) -> Option<&'a str> {
    let i = s.find(from)? + from.len();
    Some(&s[i..i + s[i..].find(to)?])
}

fn unescape(s: &str) -> String {
    // &amp; last, so an escaped entity stays what it was.
    s.trim().replace("&#39;", "'").replace("&quot;", "\"").replace("&amp;", "&")
}

/// One result card of a Mixkit listing.
fn card(c: &str) -> Option<Track> {
    let author = unescape(between(c, "music-preview__author\">", "</p>")?);
    let author = author.strip_prefix("by ").unwrap_or(&author).to_string();
    let duration = between(c, "data-test-id=\"duration\">", "</div>").map(unescape).unwrap_or_default();
    let tags = c
        .split("class=\"meta-links__link\"")
        .skip(1)
        .filter_map(|t| between(t, ">", "</a>"))
        .map(unescape)
        .take(3);
    let meta = [author.clone(), duration]
        .into_iter()
        .chain(tags)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    Some(Track {
        id: between(c, "item-id-value=\"", "\"")?.to_string(),
        url: between(c, "preview-url-value=\"", "\"")?.to_string(),
        title: unescape(between(c, "item-grid-card__title\">", "</h2>")?),
        author,
        meta,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mixkit_card_reads_into_a_track() {
        let html = r#"<div class="item-grid__item">
  data-audio-player-preview-url-value="https://assets.mixkit.co/music/21/21.mp3"
  data-audio-player-item-id-value="21"
      <h2 class="item-grid-card__title">
          Indian Meditations
      </h2>
          <p class="item-grid-music-preview__author">
      by Ahjay Stelino
    </p>
      <a class="meta-links__link" data-global--meta-links-target="link" href="/free-stock-music/world/">World</a>
      <a class="meta-links__link" data-global--meta-links-target="link" href="/x/">R&amp;B</a>
      <span class="meta-links__link meta-links__link--ellipsis"></span>
      <div class="item-grid-music-preview__meta-time" data-test-id="duration">
        2:48
      </div>"#;
        let tracks = cards(html);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, "21");
        assert_eq!(tracks[0].title, "Indian Meditations");
        assert_eq!(tracks[0].author, "Ahjay Stelino");
        assert_eq!(tracks[0].meta, "Ahjay Stelino · 2:48 · World · R&B");
    }
}
