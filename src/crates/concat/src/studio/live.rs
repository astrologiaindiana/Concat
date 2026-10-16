// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer and Concat contributors

//! Astrologia Indiana: the API edits the project on screen, live.
//!
//! Upstream, a caller on the socket edits projects of its own and is refused
//! the one the window has open. Here the server's hub hands every call about
//! that project to the window instead ([`route`]): the call is posted onto
//! the event loop and applied to the window's own session, the way a click
//! is, so the person watching sees each edit land, can undo it with the
//! keyboard, and the window's autosave keeps it.
//!
//! Calls that only read the project to render it - a frame, an export - stay
//! on the API's thread, so the window never stalls on a render: the window
//! saves first, and the API reads what was saved (see `ensure_readable` in
//! concat-api).

use std::sync::{Arc, mpsc};
use std::time::Duration;

use concat_api::{ApiError, Done, ErrorCode, Holder, OpenProjects, Reply, Request, Response};
use concat_host::{media, projects};
use concat_project::Command;

use super::Studio;
use crate::host::Shell;

/// How long a call waits for the window before it gives up: a window busy
/// for this long is a window in trouble, and the caller hears so.
const PATIENCE: Duration = Duration::from_secs(60);

/// The hub's way into the window, for the server the window starts.
pub fn route(open: OpenProjects) -> concat_server::WindowRoute {
    concat_server::WindowRoute(Arc::new(move |request: Request| {
        let Some(path) = path_of(&request) else {
            return Err(request);
        };
        if open.holder(path) != Some(Holder::Window) {
            return Err(request);
        }
        match request {
            // Read on the API's thread, from what the window saves now.
            Request::PreviewFrame { .. } | Request::ExportRun { .. } => {
                match on_window(|studio| match studio.save_for_remote() {
                    Ok(()) => Ok(Reply::Done(Done {})),
                    Err(error) => Err(ApiError::failed(error)),
                }) {
                    Response::Error(error) => Ok(Response::Error(error)),
                    _ => Err(request),
                }
            }
            // The probe is slow; it runs here, off the window's thread, and
            // the window only adds what it found.
            Request::MediaImport { file, .. } => {
                let item = match media::probe(&file) {
                    Ok(summary) => summary.to_new_media(),
                    Err(error) => return Ok(Response::Error(ApiError::failed(error))),
                };
                Ok(on_window(move |studio| {
                    studio.apply_remote(Command::AddMedia { item })
                }))
            }
            request => Ok(on_window(move |studio| studio.remote(request))),
        }
    }))
}

/// Runs `body` on the event loop with the window's state, refreshes the
/// window, and waits for the answer.
fn on_window(
    body: impl FnOnce(&mut Studio) -> Result<Reply, ApiError> + Send + 'static,
) -> Response {
    let (answer, wait) = mpsc::channel::<Response>();
    let posted = slint::invoke_from_event_loop(move || {
        let mut response = None;
        Shell::with(|shell, app| {
            let result = body(&mut shell.studio.borrow_mut());
            shell.studio.borrow().publish(&app, &shell.models);
            response = Some(result.into());
        });
        let _ = answer.send(response.unwrap_or_else(|| {
            Response::Error(ApiError::failed("the window is closing"))
        }));
    });
    if posted.is_err() {
        return Response::Error(ApiError::failed("the window is closing"));
    }
    wait.recv_timeout(PATIENCE).unwrap_or_else(|_| {
        Response::Error(ApiError::new(
            ErrorCode::Busy,
            "the window did not answer in time",
        ))
    })
}

/// The project folder a call is about, for the calls the window can serve.
fn path_of(request: &Request) -> Option<&str> {
    match request {
        Request::ProjectOpen { path }
        | Request::ProjectClose { path, .. }
        | Request::ProjectGet { path }
        | Request::ProjectDocument { path }
        | Request::ProjectSave { path, .. }
        | Request::ProjectSetVideo { path, .. }
        | Request::EditApply { path, .. }
        | Request::EditUndo { path }
        | Request::EditRedo { path }
        | Request::MediaImport { path, .. }
        | Request::PreviewFrame { path, .. }
        | Request::ExportRun { path, .. } => Some(path),
        _ => None,
    }
}

impl Studio {
    /// One call about the project on screen.
    fn remote(&mut self, request: Request) -> Result<Reply, ApiError> {
        match request {
            // Open already, here; the caller gets the window's state.
            Request::ProjectOpen { .. } | Request::ProjectGet { .. } => self.remote_view(),
            // The window keeps it; a caller letting go changes nothing.
            Request::ProjectClose { .. } => Ok(Reply::Done(Done {})),
            Request::ProjectDocument { .. } => Ok(Reply::Document(self.remote_session()?.document())),
            Request::ProjectSave { .. } => {
                self.save_for_remote().map_err(ApiError::failed)?;
                Ok(Reply::Done(Done {}))
            }
            Request::ProjectSetVideo { video, .. } => {
                self.flush_commit();
                self.echo = None;
                let session = self.session.as_mut().ok_or_else(nothing_on_screen)?;
                session
                    .set_video(video)
                    .map_err(|error| ApiError::new(ErrorCode::Refused, error))?;
                self.after_change();
                self.remote_view()
            }
            Request::EditApply { command, .. } => self.apply_remote(*command),
            Request::EditUndo { .. } => {
                self.undo();
                self.remote_view()
            }
            Request::EditRedo { .. } => {
                self.redo();
                self.remote_view()
            }
            _ => Err(ApiError::new(
                ErrorCode::Refused,
                "that call is not served on the project on screen",
            )),
        }
    }

    /// [`Studio::apply`], answering the caller instead of a toast: the same
    /// bookkeeping, and a refusal comes back as the sentence the window
    /// would have shown.
    fn apply_remote(&mut self, command: Command) -> Result<Reply, ApiError> {
        self.end_burst();
        self.flush_commit();
        self.echo = None;
        self.last_commit = None;
        log::info!("edit (remote): {}", super::command_name(&command));
        let session = self.session.as_mut().ok_or_else(nothing_on_screen)?;
        let view = session
            .apply(command)
            .map_err(|error| ApiError::new(ErrorCode::Refused, error))?;
        self.after_change();
        Ok(Reply::View(Box::new(view)))
    }

    /// The project as it stands, for the caller.
    fn remote_view(&self) -> Result<Reply, ApiError> {
        Ok(Reply::View(Box::new(self.remote_session()?.view())))
    }

    fn remote_session(&self) -> Result<&concat_host::Session, ApiError> {
        self.session.as_ref().ok_or_else(nothing_on_screen)
    }

    /// Writes the document now, on this thread: a caller about to read the
    /// project from disk must find this very state there.
    fn save_for_remote(&mut self) -> Result<(), String> {
        let Some(session) = self.session.as_mut() else {
            return Err("there is no project on screen".to_owned());
        };
        self.autosave.stop();
        let (path, document) = session.prepare_save(None);
        projects::save(&path, &document)?;
        self.dirty = false;
        Ok(())
    }
}

fn nothing_on_screen() -> ApiError {
    ApiError::new(ErrorCode::NotOpen, "there is no project on screen")
}
