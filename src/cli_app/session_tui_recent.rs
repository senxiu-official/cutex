//! Native recent-thread catalog workspace for the session TUI.
//!
//! The catalog worker owns its app-server connection for the lifetime of one
//! shell (including ordinary page switches). It deliberately never inspects
//! Codex provider storage. Request generations fence superseded replies.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;

use super::session_tui_view::{AgentSessionView, Observation, SubjectRef};
use cutex::catalog::{
    CatalogClient, CatalogError, CatalogThread, SortDirection, ThreadListParams, ThreadPage,
    ThreadSortKey,
};
use cutex::session::model::CutexSessionStore;
use cutex::session::service::cutex_session_is_managed;

const PAGE_SIZE: u32 = 50;
const MAX_DISPLAY_TEXT: usize = 160;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecentThreadState {
    Unmanaged,
    Managed,
    Retired,
    MissingCwd,
    Ambiguous,
}

impl RecentThreadState {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Unmanaged => "unmanaged",
            Self::Managed => "managed",
            Self::Retired => "retired",
            Self::MissingCwd => "cwd unavailable",
            Self::Ambiguous => "ambiguous mapping",
        }
    }

    pub(super) fn can_adopt(self) -> bool {
        matches!(self, Self::Unmanaged)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RecentThreadRow {
    pub(super) view: AgentSessionView,
    /// The native `thread/list` id. This is the only identity used for
    /// adoption; `session_id` is intentionally not retained as an identity.
    pub(super) thread_id: String,
    pub(super) owned_runtime_id: Option<String>,
    pub(super) title: String,
    /// Stable managed identity, populated only by an exact native-session
    /// join to Agent Management (or a durable session-id fallback). Native
    /// title remains available separately for unmanaged adoption review.
    pub(super) managed_name: Option<String>,
    pub(super) cwd: Option<String>,
    pub(super) provider: String,
    pub(super) source: String,
    pub(super) project_id: Option<String>,
    pub(super) recency_at: Option<i64>,
    pub(super) state: RecentThreadState,
}

impl RecentThreadRow {
    #[cfg(test)]
    pub(super) fn primary_label(&self) -> &str {
        self.managed_name.as_deref().unwrap_or(&self.title)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RecentLoadState {
    Loading,
    Ready,
    Empty,
    ProviderIncompatible(String),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecentCommand {
    LoadMore,
    Retry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RecentAdoptionRequest {
    pub(super) action_id: String,
    pub(super) formal_name: String,
    pub(super) thread_id: String,
    pub(super) title: String,
    pub(super) cwd: String,
}

#[derive(Debug, Clone)]
struct AdoptionReview {
    name: tui_input::Input,
    name_focused: bool,
    action_id: String,
    thread_id: String,
    confirmed: bool,
}

#[derive(Debug)]
enum CatalogCommand {
    CloseOwned { request: u64, id: String, owner: String },
    Archive { request: u64, id: String },
    CloseNative { request: u64, id: String },
    Load {
        request: u64,
        cursor: Option<String>,
        retry: bool,
    },
}

#[derive(Debug)]
pub(super) enum CatalogReply {
    ClosedOwned { id: String, result: Result<(), String> },
    Archived { id: String, result: Result<(), String> },
    Page {
        cursor: Option<String>,
        result: Result<ThreadPage, CatalogError>,
    },
}

/// One asynchronous, serial catalog connection. `Drop` closes the command
/// sender, allowing the worker and its owned app-server process to exit.
#[derive(Debug)]
pub(super) struct RecentCatalog {
    commands: Sender<CatalogCommand>,
    replies: Receiver<(u64, CatalogReply)>,
    request: Cell<u64>,
    disconnected: Cell<bool>,
}

impl RecentCatalog {
    pub(super) fn spawn() -> anyhow::Result<Self> {
        let (commands, command_receiver) = mpsc::channel();
        let (reply_sender, replies) = mpsc::channel();
        thread::Builder::new()
            .name("cutex-tui-recent-catalog".to_string())
            .spawn(move || catalog_worker(command_receiver, reply_sender))
            .map_err(|error| anyhow::anyhow!("failed to start recent catalog worker: {error}"))?;
        commands
            .send(CatalogCommand::Load {
                request: 0,
                cursor: None,
                retry: false,
            })
            .map_err(|_| anyhow::anyhow!("recent catalog worker stopped before loading"))?;
        Ok(Self {
            commands,
            replies,
            request: Cell::new(0),
            disconnected: Cell::new(false),
        })
    }

    pub(super) fn request(&self, command: RecentCommand, cursor: Option<String>) -> bool {
        let request = self.request.get().wrapping_add(1);
        self.request.set(request);
        self.commands
            .send(CatalogCommand::Load {
                request,
                cursor,
                retry: command == RecentCommand::Retry,
            })
            .is_ok()
    }

    pub(super) fn archive(&self, id: String) -> bool {
        let request = self.request.get().wrapping_add(1);
        self.request.set(request);
        self.commands.send(CatalogCommand::Archive { request, id }).is_ok()
    }

    pub(super) fn close_native(&self, id: String) -> bool {
        let request = self.request.get().wrapping_add(1);
        self.request.set(request);
        self.commands.send(CatalogCommand::CloseNative { request, id }).is_ok()
    }

    pub(super) fn close_owned(&self, id: String, owner: String) -> bool {
        let request = self.request.get().wrapping_add(1);
        self.request.set(request);
        self.commands.send(CatalogCommand::CloseOwned { request, id, owner }).is_ok()
    }

    pub(super) fn poll(&self) -> Option<CatalogReply> {
        loop {
            match self.replies.try_recv() {
                Ok((request, reply)) if request == self.request.get() => return Some(reply),
                Ok(_) => continue,
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => {
                    return if self.disconnected.replace(true) {
                        None
                    } else {
                        Some(CatalogReply::Page {
                            cursor: None,
                            result: Err(CatalogError::Transport(
                                "Recent catalog worker stopped; reopen TUI to reconnect".into(),
                            )),
                        })
                    }
                }
            }
        }
    }
}

fn catalog_worker(commands: Receiver<CatalogCommand>, replies: Sender<(u64, CatalogReply)>) {
    let mut client = CatalogClient::spawn_local();
    while let Ok(command) = commands.recv() {
        let (request, cursor, retry) = match command {
            CatalogCommand::CloseNative { request, id } => {
                let result = cutex::catalog::native_archive::close_and_restore(&id, None)
                    .map(|_| ()).map_err(|error| format!("{error:#}"));
                if replies.send((request, CatalogReply::ClosedOwned { id, result })).is_err() { break; }
                // Archive also affects descendants. Refresh even after partial failure.
                let result = match &mut client {
                    Ok(client) => client.thread_list(thread_list_params(None)),
                    Err(error) => Err(error.clone()),
                };
                if replies.send((request, CatalogReply::Page { cursor: None, result })).is_err() { break; }
                continue;
            }
            CatalogCommand::CloseOwned { request, id, owner } => {
                let result = (|| -> anyhow::Result<()> {
                    let store = cutex::session::store::load_cutex_session_store()?;
                    anyhow::ensure!(store.sessions.get(&owner).is_some_and(|r|
                        r.is_owned_session() && r.app_server_thread_id() == Some(id.as_str())),
                        "session runtime mapping changed; refresh before closing");
                    if store.sessions.get(&owner).is_some_and(|r|r.runtime_backend==cutex::session::model::CutexSessionRuntimeBackend::ClaudeSubscription){super::claude_runtime::lifecycle(&owner,false).map(|_|())}else{super::session_runtime::cmd_session_close_and_wait_quiet(&owner).map(|_| ())}
                })().map_err(|error| format!("{error:#}"));
                if replies.send((request, CatalogReply::ClosedOwned { id, result })).is_err() { break; }
                continue;
            }
            CatalogCommand::Archive { request, id } => {
                let result = cutex::catalog::native_archive::change(&id, false, None)
                    .map(|_| ()).map_err(|error| format!("{error:#}"));
                let succeeded = result.is_ok();
                if replies.send((request, CatalogReply::Archived { id, result })).is_err() { break; }
                // Native archive includes descendants; refresh the full first page,
                // rather than only removing the selected parent from the UI.
                if succeeded {
                    let result = match &mut client {
                        Ok(client) => client.thread_list(thread_list_params(None)),
                        Err(error) => Err(error.clone()),
                    };
                    if replies.send((request, CatalogReply::Page { cursor: None, result })).is_err() { break; }
                }
                continue;
            }
            CatalogCommand::Load { request, cursor, retry } => (request, cursor, retry),
        };
        if retry && client.is_err() {
            client = CatalogClient::spawn_local();
        }
        let result = match &mut client {
            Ok(client) => client.thread_list(thread_list_params(cursor.clone())),
            Err(error) => Err(error.clone()),
        };
        // A connected app-server can still lose its transport or time out
        // while serving a page. Both leave the owned endpoint unusable, so an
        // explicit retry must reconnect it.
        if should_recreate_catalog_client_after(&result) {
            let error = result.as_ref().expect_err("transport result is an error");
            client = Err(error.clone());
        }
        if replies
            .send((request, CatalogReply::Page { cursor, result }))
            .is_err()
        {
            break;
        }
    }
}

fn should_recreate_catalog_client_after(result: &Result<ThreadPage, CatalogError>) -> bool {
    matches!(
        result,
        Err(CatalogError::Transport(_)) | Err(CatalogError::Timeout { .. })
    )
}

fn thread_list_params(cursor: Option<String>) -> ThreadListParams {
    ThreadListParams {
        cursor,
        limit: Some(PAGE_SIZE),
        sort_key: Some(ThreadSortKey::RecencyAt),
        sort_direction: Some(SortDirection::Desc),
        ..ThreadListParams::default()
    }
}

#[derive(Debug, Clone)]
pub(super) struct RecentSessionsWorkspace {
    rows: Vec<RecentThreadRow>,
    selected: usize,
    next_cursor: Option<String>,
    failed_cursor: Option<String>,
    loading: bool,
    load_state: RecentLoadState,
    review: Option<AdoptionReview>,
    query: tui_input::Input,
    filter_focused: bool,
    pub(super) facets: super::session_tui_filters::Facets,
    pub(super) sort_order: cutex::profiles::list_preferences::ListSort,
}

impl Default for RecentSessionsWorkspace {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            selected: 0,
            next_cursor: None,
            failed_cursor: None,
            loading: true,
            load_state: RecentLoadState::Loading,
            review: None,
            query: tui_input::Input::default(),
            filter_focused: false,
            facets: Default::default(),
            sort_order: Default::default(),
        }
    }
}

impl RecentSessionsWorkspace {
    pub(super) fn replace_remote(&mut self,rows:Vec<RecentThreadRow>) {let selected=self.selected_row().map(|r|r.thread_id.clone());self.rows.retain(|r|!r.thread_id.starts_with("remote/"));self.rows.extend(rows);if let Some(id)=selected {if let Some(i)=self.rows.iter().position(|r|r.thread_id==id){self.selected=i;}}self.normalize_visible_selection();}

    pub(super) fn all_views(&self)->Vec<AgentSessionView> {self.rows.iter().map(|r|r.view.clone()).collect()}

    pub(super) fn enrich_views(&mut self, managed: &[AgentSessionView]) {
        for row in &mut self.rows {
            if let Some(view) = managed.iter().find(|view| {
                view.subject == row.view.subject
                    && view.native_thread.as_deref() == Some(row.thread_id.as_str())
            }) {
                let mut current = view.clone();
                current.native_title = Some(row.title.clone());
                current.native_workspace = row.project_id.clone();
                current.updated = row.view.updated.clone();
                row.managed_name = Some(current.name.clone());
                row.view = current;
            }
        }
    }
    pub(super) fn selected_row(&self)->Option<&RecentThreadRow>{self.visible_indices().contains(&self.selected).then(||self.rows.get(self.selected)).flatten()}
    pub(super) fn rows(&self) -> &[RecentThreadRow] {
        &self.rows
    }
    pub(super) fn visible_rows(&self) -> Vec<&RecentThreadRow> {
        self.visible_indices()
            .into_iter()
            .filter_map(|index| self.rows.get(index))
            .collect()
    }
    pub(super) fn selected_visible(&self) -> usize {
        self.visible_indices()
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0)
    }
    pub(super) fn query(&self) -> &str {
        self.query.value()
    }
    pub(super) fn filter_input(&self) -> &tui_input::Input {
        &self.query
    }
    pub(super) fn filter_input_mut(&mut self) -> &mut tui_input::Input {
        &mut self.query
    }
    pub(super) fn filter_edited(&mut self) {
        self.select_first_visible();
    }
    pub(super) fn edit_filter(&mut self, request: tui_input::InputRequest) {
        self.query.handle(request);
        self.select_first_visible();
    }
    pub(super) fn filter_focused(&self) -> bool {
        self.filter_focused
    }
    pub(super) fn focus_filter(&mut self) {
        self.filter_focused = true;
    }
    pub(super) fn blur_filter(&mut self) {
        self.filter_focused = false;
    }
    pub(super) fn push_filter(&mut self, character: char) {
        self.edit_filter(tui_input::InputRequest::InsertChar(character));
    }
    pub(super) fn pop_filter(&mut self) {
        self.edit_filter(tui_input::InputRequest::DeletePrevChar);
    }
    pub(super) fn clear_filter(&mut self) {
        self.edit_filter(tui_input::InputRequest::DeleteLine);
    }
    pub(super) fn loading(&self) -> bool {
        self.loading
    }
    pub(super) fn load_state(&self) -> &RecentLoadState {
        &self.load_state
    }
    pub(super) fn next_cursor(&self) -> Option<String> {
        self.next_cursor.clone()
    }
    pub(super) fn cursor_for(&self, command: RecentCommand) -> Option<String> {
        match command {
            RecentCommand::LoadMore => self.next_cursor.clone(),
            RecentCommand::Retry => self.failed_cursor.clone(),
        }
    }
    pub(super) fn mark_loading(&mut self) {
        self.loading = true;
        self.load_state = RecentLoadState::Loading;
    }
    pub(super) fn review(&self) -> Option<&RecentThreadRow> {
        let review = self.review.as_ref()?;
        self.rows
            .iter()
            .find(|row| row.thread_id == review.thread_id)
    }
    pub(super) fn review_confirmed(&self) -> bool {
        self.review.as_ref().is_some_and(|review| review.confirmed)
    }

    #[cfg(test)]
    pub(super) fn receive(&mut self, reply: CatalogReply, store: &CutexSessionStore) {
        self.receive_with_managed_names(reply, store, &HashMap::new());
    }

    pub(super) fn remove_archived(&mut self, id: &str) {
        self.rows.retain(|row| row.thread_id != id);
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        self.loading = false;
    }

    pub(super) fn receive_with_managed_names(
        &mut self,
        reply: CatalogReply,
        store: &CutexSessionStore,
        managed_names: &HashMap<String, String>,
    ) {
        let CatalogReply::Page { cursor, result } = reply else { return; };
        self.loading = false;
        if result.is_err() && cursor.is_none() {
            merge_owned_rows(&mut self.rows,store,managed_names);
            self.normalize_visible_selection();
        }
        match result {
            Ok(page) => {
                let selected_id = self
                    .rows
                    .get(self.selected)
                    .map(|row| row.thread_id.clone());
                self.failed_cursor = None;
                let append = cursor.is_some();
                let mut incoming = page
                    .data
                    .into_iter()
                    .map(|thread| recent_row(thread, store, managed_names))
                    .collect::<Vec<_>>();
                if !append {
                    merge_owned_rows(&mut incoming,store,managed_names);
                }
                incoming.sort_by(|left, right| {
                    right
                        .recency_at
                        .cmp(&left.recency_at)
                        .then_with(|| left.thread_id.cmp(&right.thread_id))
                });
                let mut page_ids = HashSet::new();
                incoming.retain(|row| page_ids.insert(row.thread_id.clone()));
                if append {
                    let known = self
                        .rows
                        .iter()
                        .map(|row| row.thread_id.clone())
                        .collect::<HashSet<_>>();
                    self.rows.extend(
                        incoming
                            .into_iter()
                            .filter(|row| !known.contains(&row.thread_id)),
                    );
                } else {
                    let selected_id = self
                        .rows
                        .get(self.selected)
                        .map(|row| row.thread_id.clone());
                    self.rows = incoming;
                    self.selected = selected_id
                        .and_then(|id| self.rows.iter().position(|row| row.thread_id == id))
                        .unwrap_or(self.selected);
                }
                self.rows.sort_by(|left, right| {
                    right
                        .recency_at
                        .cmp(&left.recency_at)
                        .then_with(|| left.thread_id.cmp(&right.thread_id))
                });
                self.selected = selected_id
                    .and_then(|id| self.rows.iter().position(|row| row.thread_id == id))
                    .unwrap_or(self.selected);
                self.next_cursor = page.next_cursor;
                self.load_state = if self.rows.is_empty() {
                    RecentLoadState::Empty
                } else {
                    RecentLoadState::Ready
                };
                self.normalize_visible_selection();
            }
            Err(CatalogError::ProviderIncompatible(message)) => {
                self.failed_cursor = cursor;
                self.load_state = RecentLoadState::ProviderIncompatible(bound(&message));
            }
            Err(error) => {
                self.failed_cursor = cursor;
                self.load_state = RecentLoadState::Failed(bound(&error.to_string()));
            }
        }
    }

    /// Catalog data is never applied without a read-only durable-store
    /// reconciliation. A store read failure must leave the UI retryable rather
    /// than indefinitely showing its previous loading state.
    pub(super) fn reconciliation_failed(&mut self, reply: CatalogReply, message: String) {
        let CatalogReply::Page { cursor, .. } = reply else { return; };
        self.loading = false;
        self.failed_cursor = cursor;
        self.load_state = RecentLoadState::Failed(bound(&message));
    }

    pub(super) fn reproject(&mut self, store: &CutexSessionStore) {
        for row in &mut self.rows {
            row.state = thread_state(&row.thread_id, row.cwd.is_some(), store);
            let known_managed_name = row.managed_name.clone();
            row.managed_name = managed_primary_label(
                &row.thread_id,
                row.state,
                store,
                known_managed_name.as_deref(),
            );
            if matches!(
                row.state,
                RecentThreadState::Managed | RecentThreadState::Retired
            ) {
                if let Some(record) = store
                    .sessions
                    .values()
                    .find(|r| r.codex_session_id.as_deref() == Some(row.thread_id.as_str()))
                {
                    row.view.subject = SubjectRef::Managed(record.cutex_session_id.clone());
                    row.view.configured_profile = record.profile.clone();
                    row.view.name = record
                        .formal_agent_name
                        .clone()
                        .or_else(|| row.managed_name.clone())
                        .unwrap_or_else(|| record.cutex_session_id.clone());
                }
            } else {
                row.view.subject = SubjectRef::Native {
                    catalog: "paired-local-app-server".into(),
                    thread: row.thread_id.clone(),
                };
                row.view.name = row.title.clone();
                row.view.project =
                    Observation::Unavailable("durable mapping absent or ambiguous".into());
                row.view.effective_profile =
                    Observation::Unavailable("effective profile not observed".into());
                let owner = store.sessions.values().find(|r| r.is_owned_session()
                    && r.app_server_thread_id() == Some(row.thread_id.as_str()));
                row.owned_runtime_id = owner.map(|r|r.cutex_session_id.clone());
                row.view.configured_profile = owner.and_then(|r| r.profile.clone());
                if let Some(record) = owner {
                    row.view.runtime = if record.app_server_runtime.is_none() && record.runtime_pid.is_none() {
                        Observation::Known("Offline".into())
                    } else { Observation::Stale("Online".into(), "saved runtime binding".into()) };
                }
            }
        }
        self.normalize_visible_selection();
    }

    pub(super) fn move_selection(&mut self, direction: isize) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        let position = visible
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        let next = position
            .saturating_add_signed(direction)
            .min(visible.len() - 1);
        self.selected = visible[next];
    }

    pub(super) fn select_edge(&mut self, last: bool) {
        let visible = self.visible_indices();
        if let Some(index) = if last {
            visible.last()
        } else {
            visible.first()
        } {
            self.selected = *index;
        }
    }

    pub(super) fn begin_review(&mut self) -> bool {
        if !self.visible_indices().contains(&self.selected) {
            return false;
        }
        let Some(row) = self.rows.get(self.selected) else {
            return false;
        };
        if !row.state.can_adopt() {
            return false;
        }
        self.review = Some(AdoptionReview {
            name: tui_input::Input::default(),
            name_focused: true,
            action_id: format!("human-adopt-{}", uuid::Uuid::new_v4()),
            thread_id: row.thread_id.clone(),
            confirmed: false,
        });
        true
    }

    pub(super) fn set_review_confirmed(&mut self, confirmed: bool) {
        if let Some(review) = &mut self.review {
            review.confirmed = confirmed;
        }
    }
    pub(super) fn adoption_name(&self) -> Option<&tui_input::Input> {
        self.review.as_ref().map(|r| &r.name)
    }
    pub(super) fn adoption_name_focused(&self) -> bool {
        self.review.as_ref().is_some_and(|r| r.name_focused)
    }
    pub(super) fn focus_adoption_name(&mut self) {
        if let Some(r) = self.review.as_mut() {
            r.name_focused = true;
            r.confirmed = false;
        }
    }
    pub(super) fn adoption_name_input(&mut self) -> Option<&mut tui_input::Input> {
        self.review
            .as_mut()
            .filter(|r| r.name_focused)
            .map(|r| &mut r.name)
    }
    pub(super) fn blur_adoption_name(&mut self) -> bool {
        if let Some(review) = self.review.as_mut().filter(|r| r.name_focused) {
            review.name_focused = false;
            return true;
        }
        false
    }

    pub(super) fn cancel_review(&mut self) {
        self.review = None;
    }

    pub(super) fn adoption_request(&self) -> Option<RecentAdoptionRequest> {
        let review = self.review.as_ref()?;
        if !review.confirmed || review.name.value().trim().is_empty() {
            return None;
        }
        let row = self
            .rows
            .iter()
            .find(|row| row.thread_id == review.thread_id)?;
        (row.state == RecentThreadState::Unmanaged).then(|| RecentAdoptionRequest {
            action_id: review.action_id.clone(),
            formal_name: review.name.value().trim().to_string(),
            thread_id: row.thread_id.clone(),
            title: row.title.clone(),
            cwd: row.cwd.clone().expect("unmanaged native thread has cwd"),
        })
    }

    pub(super) fn adoption_succeeded(&mut self, store: &CutexSessionStore) {
        self.reproject(store);
        self.review = None;
    }

    fn visible_indices(&self) -> Vec<usize> {
        let query = self.query.value().trim().to_lowercase();
        let mut indices: Vec<usize> = self.rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                if row.state == RecentThreadState::Retired || !self.facets.matches(&row.view.host,row.view.project_id.as_deref()) {
                    return None;
                }
                let matches = query.is_empty()
                    || row.title.to_lowercase().contains(&query)
                    || row.thread_id.to_lowercase().contains(&query)
                    || row.owned_runtime_id.as_deref().is_some_and(|id| id.to_lowercase().contains(&query))
                    || matches!(&row.view.subject, SubjectRef::Managed(id) if id.to_lowercase().contains(&query))
                    || row.managed_name.as_deref().is_some_and(|name|name.to_lowercase().contains(&query));
                matches.then_some(index)
            })
            .collect();
        use cutex::profiles::list_preferences::{ListSort, compare_projects};
        indices.sort_by(|a,b| {
            let left = &self.rows[*a]; let right = &self.rows[*b];
            match self.sort_order {
                ListSort::Default | ListSort::Pinned => right.recency_at.cmp(&left.recency_at).then_with(|| left.thread_id.cmp(&right.thread_id)),
                ListSort::NameAsc | ListSort::NameDesc => self.sort_order.compare_names(&left.view.name, &right.view.name).then_with(|| left.thread_id.cmp(&right.thread_id)),
                ListSort::Project => compare_projects(left.view.project_id.as_deref().map(|id| left.view.project.known().map(String::as_str).unwrap_or(id)), right.view.project_id.as_deref().map(|id| right.view.project.known().map(String::as_str).unwrap_or(id)))
                    .then_with(|| self.sort_order.compare_names(&left.view.name, &right.view.name)).then_with(|| left.thread_id.cmp(&right.thread_id)),
            }
        });
        indices
    }

    fn normalize_visible_selection(&mut self) {
        if !self.visible_indices().contains(&self.selected) { self.select_first_visible(); }
    }

    fn select_first_visible(&mut self) {
        if let Some(index) = self.visible_indices().first() {
            self.selected = *index;
        }
    }
}

fn merge_owned_rows(incoming:&mut Vec<RecentThreadRow>,store:&CutexSessionStore,managed_names:&HashMap<String,String>) {
                    for record in store.sessions.values().filter(|r| r.is_owned_session() && !r.is_retired()) {
                        let Some(native) = record.app_server_thread_id() else { continue; };
                        incoming.retain(|row| row.thread_id != native);
                        let thread: CatalogThread = serde_json::from_value(serde_json::json!({
                            "id":native, "sessionId":native, "projectId":null,
                            "cwd":record.cwd, "name":cutex::session::metadata::cutex_session_display_name(record),
                            "updatedAt":chrono::DateTime::parse_from_rfc3339(&record.updated_at).ok().map(|t|t.timestamp()),
                            "source":if record.claude_session.is_some(){"cutex-claude-session"}else{"cutex-session"}
                        })).expect("owned session catalog fields have fixed types");
                        incoming.push(recent_row(thread, store, managed_names));
                    }
}

fn recent_row(
    thread: CatalogThread,
    store: &CutexSessionStore,
    managed_names: &HashMap<String, String>,
) -> RecentThreadRow {
    let cwd = thread
        .cwd
        .map(|path| path.display().to_string())
        .filter(|cwd| !cwd.trim().is_empty());
    let title = thread
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(thread.preview);
    let provider = if thread.model_provider.trim().is_empty() {
        "native".to_string()
    } else {
        bound(&thread.model_provider)
    };
    let source = thread
        .source
        .as_str()
        .map(bound)
        .unwrap_or_else(|| "native".to_string());
    let state = thread_state(&thread.id, cwd.is_some(), store);
    let managed_name = managed_primary_label(
        &thread.id,
        state,
        store,
        managed_names.get(&thread.id).map(String::as_str),
    );
    let records: Vec<_> = store
        .sessions
        .values()
        .filter(|record| {
            record.codex_session_id.as_deref() == Some(thread.id.as_str())
                && cutex_session_is_managed(record)
        })
        .collect();
    let owner = store.sessions.values().find(|record| record.is_owned_session()
        && record.app_server_thread_id() == Some(thread.id.as_str()));
    let mut view = AgentSessionView {
        remote_provider: None, remote_profile_style: None,
            host: cutex::platform::host::current_host_name(),
        badge: None,
        project_id: None,
        subject: if records.len() == 1 && state != RecentThreadState::Ambiguous {
            SubjectRef::Managed(records[0].cutex_session_id.clone())
        } else {
            SubjectRef::Native {
                catalog: "paired-local-app-server".into(),
                thread: thread.id.clone(),
            }
        },
        name: managed_name.clone().unwrap_or_else(|| bound(&title)),
        native_title: Some(bound(&title)),
        native_thread: Some(thread.id.clone()),
        native_workspace: thread.project_id.clone(),
        compaction_count: None,
        runtime: Observation::Unavailable("runtime not observed by native catalog".into()),
        runtime_version: None,
        runtime_version_latest: None,
        runtime_version_recorded_at: None,
        project: Observation::Unavailable(
            "Cutex membership not observed; native workspace is not membership".into(),
        ),
        configured_profile: records
            .first()
            .filter(|_| records.len() == 1 && state != RecentThreadState::Ambiguous)
            .and_then(|r| r.profile.clone()),
        effective_profile: Observation::Unavailable("effective profile not observed".into()),
        role: String::new(),
        activity: String::new(),
        activity_details: None,
        updated: thread
            .recency_at
            .or(thread.updated_at)
            .or(thread.created_at)
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .map(|t| t.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "Unavailable".into()),
        cwd: cwd.clone().unwrap_or_else(|| "Unavailable".into()),
        retirement_note: (state == RecentThreadState::Ambiguous)
            .then(|| "Ambiguous durable/native mapping; not joined".into()),
    };
    if let Some(record) = owner {
        view.configured_profile = record.profile.clone();
        view.runtime = if record.app_server_runtime.is_none() && record.runtime_pid.is_none() {
            Observation::Known("Offline".into())
        } else { Observation::Stale("Online".into(), "saved runtime binding; refresh for current status".into()) };
    }
    RecentThreadRow {
        owned_runtime_id: owner.map(|r|r.cutex_session_id.clone()),
        view,
        thread_id: thread.id,
        title: bound(&title),
        managed_name,
        cwd,
        provider,
        source,
        project_id: thread.project_id.map(|project| bound(&project)),
        recency_at: thread
            .recency_at
            .or(thread.updated_at)
            .or(thread.created_at),
        state,
    }
}

fn managed_primary_label(
    thread_id: &str,
    state: RecentThreadState,
    store: &CutexSessionStore,
    authoritative_name: Option<&str>,
) -> Option<String> {
    matches!(
        state,
        RecentThreadState::Managed | RecentThreadState::Retired
    )
    .then(|| {
        authoritative_name
            .filter(|name| !name.trim().is_empty())
            .map(bound)
            .or_else(|| {
                store.sessions.values().find_map(|record| {
                    (record.codex_session_id.as_deref() == Some(thread_id)).then(|| {
                        record
                            .formal_agent_name
                            .clone()
                            .unwrap_or_else(|| record.cutex_session_id.clone())
                    })
                })
            })
            .unwrap_or_else(|| thread_id.to_string())
    })
}

fn thread_state(thread_id: &str, has_cwd: bool, store: &CutexSessionStore) -> RecentThreadState {
    if store
        .sessions
        .values()
        .filter(|r| r.codex_session_id.as_deref() == Some(thread_id))
        .count()
        > 1
    {
        return RecentThreadState::Ambiguous;
    }
    if store.sessions.values().any(|record| {
        record.codex_session_id.as_deref() == Some(thread_id)
            && record.is_retired()
            && cutex_session_is_managed(record)
    }) {
        RecentThreadState::Retired
    } else if store.sessions.values().any(|record| {
        record.codex_session_id.as_deref() == Some(thread_id) && cutex_session_is_managed(record)
    }) {
        RecentThreadState::Managed
    } else if !has_cwd {
        RecentThreadState::MissingCwd
    } else {
        RecentThreadState::Unmanaged
    }
}

fn bound(value: &str) -> String {
    let mut text = value
        .trim()
        .chars()
        .take(MAX_DISPLAY_TEXT)
        .collect::<String>();
    if value.trim().chars().count() > MAX_DISPLAY_TEXT {
        text.push('…');
    }
    if text.is_empty() {
        "(unnamed thread)".to_string()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ui_contract_c_d03_v07_recent_exact_mapping_retirement_and_pagination() {
        let mut store = store_with("mapped", true, false);
        let record = store.sessions.values_mut().next().unwrap();
        record.formal_agent_name = Some("Formal Agent".into());
        record.thread_name = Some("never formal".into());
        let mapped = recent_row(
            thread("mapped", "irrelevant-session", 1),
            &store,
            &HashMap::new(),
        );
        assert_eq!(mapped.view.name, "Formal Agent");
        assert_eq!(
            mapped.view.subject,
            SubjectRef::Managed("cutex-test".into())
        );
        assert!(matches!(mapped.view.project, Observation::Unavailable(_)));
        assert_eq!(mapped.view.native_workspace.as_deref(), Some("project-a"));
        let mut duplicate = store.sessions.values().next().unwrap().clone();
        duplicate.cutex_session_id = "other-id".into();
        store.sessions.insert("other-id".into(), duplicate);
        let ambiguous = recent_row(thread("mapped", "other", 1), &store, &HashMap::new());
        assert_eq!(ambiguous.state, RecentThreadState::Ambiguous);
        assert!(!ambiguous.state.can_adopt());
        assert!(matches!(ambiguous.view.subject, SubjectRef::Native { .. }));
        let retired = store_with("retired", true, true);
        let mut workspace = RecentSessionsWorkspace::default();
        workspace.receive(
            CatalogReply::Page {
                cursor: None,
                result: Ok(ThreadPage {
                    data: vec![
                        thread("retired", "x", 3),
                        thread("same", "x", 2),
                        thread("same", "x", 1),
                    ],
                    next_cursor: Some("next".into()),
                    backwards_cursor: None,
                }),
            },
            &retired,
        );
        assert_eq!(workspace.rows.len(), 2);
        assert_eq!(workspace.visible_rows().len(), 1);
        assert!(workspace.begin_review());
        assert_eq!(workspace.review().unwrap().thread_id, "same");
        workspace.cancel_review();
        workspace.receive(
            CatalogReply::Page {
                cursor: Some("next".into()),
                result: Ok(ThreadPage {
                    data: vec![thread("new", "x", 4)],
                    next_cursor: None,
                    backwards_cursor: None,
                }),
            },
            &retired,
        );
        assert_eq!(workspace.rows[workspace.selected].thread_id, "same");
    }
    #[test]
    fn ui_contract_b2_failed_pages_preserve_rows_cursor_and_selection() {
        let mut workspace = RecentSessionsWorkspace::default();
        let store = CutexSessionStore::default();
        workspace.receive(
            CatalogReply::Page {
                cursor: None,
                result: Ok(ThreadPage {
                    data: vec![thread("selected", "native", 1)],
                    next_cursor: Some("next".into()),
                    backwards_cursor: None,
                }),
            },
            &store,
        );
        for cursor in [Some("next".to_string()), None] {
            workspace.mark_loading();
            workspace.receive(
                CatalogReply::Page {
                    cursor: cursor.clone(),
                    result: Err(CatalogError::Transport("fixture failed".into())),
                },
                &store,
            );
            assert!(!workspace.loading());
            assert_eq!(workspace.rows.len(), 1);
            assert_eq!(workspace.cursor_for(RecentCommand::Retry), cursor);
            assert_eq!(workspace.next_cursor().as_deref(), Some("next"));
        }
        workspace.receive(
            CatalogReply::Page {
                cursor: Some("next".into()),
                result: Ok(ThreadPage {
                    data: vec![thread("newer", "native-2", 2)],
                    next_cursor: None,
                    backwards_cursor: None,
                }),
            },
            &store,
        );
        assert_eq!(workspace.rows[workspace.selected].thread_id, "selected");
    }
    #[test]
    fn ui_contract_b2_catalog_request_fence_and_disconnect() {
        let (commands, _command_receiver) = mpsc::channel();
        let (sender, replies) = mpsc::channel();
        let catalog = RecentCatalog {
            commands,
            replies,
            request: Cell::new(0),
            disconnected: Cell::new(false),
        };
        assert!(catalog.request(RecentCommand::Retry, None));
        sender
            .send((
                0,
                CatalogReply::Page {
                    cursor: None,
                    result: Err(CatalogError::Transport("stale".into())),
                },
            ))
            .unwrap();
        sender
            .send((
                1,
                CatalogReply::Page {
                    cursor: Some("retry-cursor".into()),
                    result: Err(CatalogError::Transport("current".into())),
                },
            ))
            .unwrap();
        assert!(
            matches!(catalog.poll(), Some(CatalogReply::Page { cursor: Some(cursor), .. }) if cursor == "retry-cursor")
        );
        assert!(catalog.poll().is_none());
        drop(sender);
        assert!(matches!(
            catalog.poll(),
            Some(CatalogReply::Page { result: Err(_), .. })
        ));
        assert!(catalog.poll().is_none());
    }
    #[test]
    fn claude_owned_session_appears_without_codex_history_and_reprojects_same_owner() {
        let mut record=CutexSessionRecord::new("cutex.018e0000-0000-7000-8000-0000c0004902".into(),None,"local".into(),"/workspace".into(),Some("claude".into())).unwrap();
        record.registration_class=AgentRegistrationClass::LocalOnly;
        record.agent_enabled=false;
        record.codex_session_id=None;
        record.runtime_backend=cutex::session::model::CutexSessionRuntimeBackend::ClaudeSubscription;
        record.claude_session=Some(serde_json::from_value(serde_json::json!({
            "deployment":{"python":"/python","adapter_source":"/adapter","claude_cli":"/claude","claude_home":"/home","tui_binary":"/tui"},
            "thread_id":"claude-thread","state_dir":"/state","model":"sonnet"
        })).unwrap());
        let mut store=CutexSessionStore::default();store.sessions.insert(record.cutex_session_id.clone(),record.clone());
        let mut workspace=RecentSessionsWorkspace::default();
        workspace.receive(CatalogReply::Page{cursor:None,result:Ok(ThreadPage{data:vec![],next_cursor:None,backwards_cursor:None})},&store);
        let mut without_catalog=RecentSessionsWorkspace::default();
        without_catalog.receive(CatalogReply::Page{cursor:None,result:Err(CatalogError::Transport("Codex is not installed".into()))},&store);
        assert_eq!(without_catalog.rows.len(),1);
        assert_eq!(without_catalog.rows[0].owned_runtime_id.as_deref(),Some(record.cutex_session_id.as_str()));
        assert_eq!(workspace.rows.len(),1);
        assert_eq!(workspace.rows[0].thread_id,"claude-thread");
        assert_eq!(workspace.rows[0].owned_runtime_id.as_deref(),Some(record.cutex_session_id.as_str()));
        workspace.reproject(&store);
        assert_eq!(workspace.rows[0].owned_runtime_id.as_deref(),Some(record.cutex_session_id.as_str()));
    }

    #[test]
    fn owned_session_remains_native_and_is_listed_without_shared_catalog_history() {
        let native = "018e0000-0000-7000-8000-0000c0004901";
        let mut record = CutexSessionRecord::new(format!("cutex.{native}"), Some(native.into()),
            "local".into(), "/workspace".into(), Some("test-profile".into())).unwrap();
        record.registration_class = AgentRegistrationClass::LocalOnly;
        record.agent_enabled = false;
        record.explicit_launch = Some(cutex::agent_management::ExplicitLaunchContract {
            version:4, migration_action_id:None, native_id:native.into(),
            native_home:"/private/native".into(), bundle_manifest:"/private/bundle.json".into(), bundle_sha256:cutex::role_revision::Sha256::new("a".repeat(64)).unwrap(),
        });
        let mut store = CutexSessionStore::default();
        store.sessions.insert(record.cutex_session_id.clone(), record.clone());
        let mut workspace = RecentSessionsWorkspace::default();
        workspace.receive(CatalogReply::Page { cursor:None, result:Ok(ThreadPage {
            data:Vec::new(), next_cursor:None, backwards_cursor:None,
        })}, &store);
        assert_eq!(workspace.rows.len(), 1);
        let row = &workspace.rows[0];
        assert_eq!(row.owned_runtime_id.as_deref(), Some(record.cutex_session_id.as_str()));
        assert!(matches!(row.view.subject, SubjectRef::Native { .. }));
        assert_eq!(row.view.configured_profile.as_deref(), Some("test-profile"));
        workspace.reproject(&store);
        assert_eq!(workspace.rows[0].view.configured_profile.as_deref(), Some("test-profile"));
        assert_eq!(workspace.rows[0].view.runtime.known().map(String::as_str), Some("Offline"));
        // The ordinary owner ID is distinct from the native thread ID and display name.
        for query in [record.cutex_session_id.clone(), record.cutex_session_id.to_uppercase()] {
            workspace.clear_filter();
            for c in query.chars() { workspace.push_filter(c); }
            assert_eq!(workspace.visible_rows().len(), 1);
            assert_eq!(workspace.visible_rows()[0].owned_runtime_id.as_deref(), Some(record.cutex_session_id.as_str()));
        }

    }

    use super::*;
    use cutex::agent_bus::model::AgentRegistrationClass;
    use cutex::session::model::CutexSessionRecord;
    use serde_json::json;

    fn thread(id: &str, session_id: &str, recency: i64) -> CatalogThread {
        CatalogThread {
            id: id.to_string(),
            session_id: session_id.to_string(),
            project_id: Some("project-a".to_string()),
            parent_thread_id: None,
            preview: "a bounded preview".to_string(),
            model_provider: "openai".to_string(),
            created_at: Some(recency),
            updated_at: Some(recency),
            recency_at: Some(recency),
            cwd: Some("/work".into()),
            name: None,
            status: json!({}),
            source: json!("cli"),
            additional_fields: Default::default(),
        }
    }

    fn store_with(id: &str, managed: bool, retired: bool) -> CutexSessionStore {
        let mut store = CutexSessionStore::default();
        let mut record = CutexSessionRecord::new(
            "cutex-test".to_string(),
            Some(id.to_string()),
            "host".to_string(),
            "/work".to_string(),
            None,
        )
        .unwrap();
        record.registration_class = if managed {
            AgentRegistrationClass::Persistent
        } else {
            AgentRegistrationClass::LocalOnly
        };
        if retired {
            record.archive_state = cutex::session::model::CutexSessionArchiveState::Retired;
        }
        store.sessions.insert("cutex-test".to_string(), record);
        store
    }

    #[test]
    fn session_sort_is_stable_across_loaded_pages_and_keeps_selected_identity() {
        use cutex::profiles::list_preferences::ListSort;
        let mut workspace = RecentSessionsWorkspace::default();
        let mut first = thread("thread-z", "tree-z", 20); first.name = Some("Zulu".into());
        let mut second = thread("thread-a", "tree-a", 10); second.name = Some("alpha".into());
        workspace.receive(CatalogReply::Page { cursor: None, result: Ok(ThreadPage { data: vec![first, second], next_cursor: Some("next".into()), backwards_cursor: None }) }, &CutexSessionStore::default());
        let selected = workspace.selected_row().unwrap().thread_id.clone();
        workspace.sort_order = ListSort::NameAsc;
        assert_eq!(workspace.visible_rows().iter().map(|r|r.thread_id.as_str()).collect::<Vec<_>>(), ["thread-a", "thread-z"]);
        assert_eq!(workspace.selected_row().unwrap().thread_id, selected);
        let mut third = thread("thread-b", "tree-b", 5); third.name = Some("Beta".into());
        workspace.receive(CatalogReply::Page { cursor: Some("next".into()), result: Ok(ThreadPage { data: vec![third], next_cursor: None, backwards_cursor: None }) }, &CutexSessionStore::default());
        assert_eq!(workspace.visible_rows().iter().map(|r|r.thread_id.as_str()).collect::<Vec<_>>(), ["thread-a", "thread-b", "thread-z"]);
        assert_eq!(workspace.selected_row().unwrap().thread_id, selected);
        let target = workspace.rows.iter_mut().find(|r|r.thread_id == "thread-z").unwrap();
        target.view.project_id = Some("project-z".into()); target.view.project = Observation::Known("Project Z".into());
        workspace.sort_order = ListSort::Project;
        assert_eq!(workspace.visible_rows()[0].thread_id, "thread-z");
    }

    #[test]
    fn visual_restoration_recent_badge_requires_durable_and_native_match() {
        let mut workspace = RecentSessionsWorkspace::default();
        workspace.receive(
            CatalogReply::Page {
                cursor: None,
                result: Ok(ThreadPage {
                    data: vec![thread("thread-1", "native title", 1)],
                    next_cursor: None,
                    backwards_cursor: None,
                }),
            },
            &store_with("thread-1", true, false),
        );
        let mut managed = workspace.rows[0].view.clone();
        managed.badge = Some(super::super::session_tui_view::ProjectBadge {
            label: "CX".into(),
            color: cutex::agent_management::ProjectPaletteColor::Cyan,
        });
        managed.native_thread = Some("wrong-native".into());
        workspace.enrich_views(&[managed.clone()]);
        assert!(workspace.rows[0].view.badge.is_none());
        managed.native_thread = Some("thread-1".into());
        workspace.enrich_views(&[managed]);
        assert_eq!(workspace.rows[0].view.badge.as_ref().unwrap().label, "CX");
    }

    #[test]
    fn native_thread_id_not_tree_session_id_controls_duplicate_detection() {
        let store = store_with("thread-1", true, false);
        assert_eq!(
            thread_state("thread-1", true, &store),
            RecentThreadState::Managed
        );
        assert_eq!(
            thread_state("tree-session-1", true, &store),
            RecentThreadState::Unmanaged
        );
    }

    #[test]
    fn retired_native_thread_cannot_be_adopted() {
        let store = store_with("thread-1", true, true);
        assert_eq!(
            thread_state("thread-1", true, &store),
            RecentThreadState::Retired
        );
    }

    #[test]
    fn page_is_sorted_by_native_recency_and_deduplicated_on_load_more() {
        let mut workspace = RecentSessionsWorkspace::default();
        workspace.receive(
            CatalogReply::Page {
                cursor: None,
                result: Ok(ThreadPage {
                    data: vec![thread("older", "tree-a", 1), thread("newer", "tree-b", 2)],
                    next_cursor: Some("next".to_string()),
                    backwards_cursor: None,
                }),
            },
            &CutexSessionStore::default(),
        );
        assert_eq!(workspace.rows[0].thread_id, "newer");
        workspace.receive(
            CatalogReply::Page {
                cursor: Some("next".to_string()),
                result: Ok(ThreadPage {
                    data: vec![
                        thread("newer", "other-tree", 2),
                        thread("oldest", "tree-c", 0),
                    ],
                    next_cursor: None,
                    backwards_cursor: None,
                }),
            },
            &CutexSessionStore::default(),
        );
        assert_eq!(workspace.rows.len(), 3);
    }

    #[test]
    fn name_filter_preserves_load_more_cursor_and_does_not_match_provider() {
        let mut workspace = RecentSessionsWorkspace::default();
        let mut other = thread("other", "tree-b", 2);
        other.project_id = Some("project-b".to_string());
        other.model_provider = "local".to_string();
        other.source = json!("ide");
        workspace.receive(
            CatalogReply::Page {
                cursor: None,
                result: Ok(ThreadPage {
                    data: vec![thread("openai", "tree-a", 1), other],
                    next_cursor: Some("next".to_string()),
                    backwards_cursor: None,
                }),
            },
            &CutexSessionStore::default(),
        );

        workspace.focus_filter();
        for character in "openai".chars() {
            workspace.push_filter(character);
        }
        assert_eq!(workspace.visible_rows().len(), 1);
        assert_eq!(workspace.visible_rows()[0].thread_id, "openai");
        assert_eq!(workspace.next_cursor().as_deref(), Some("next"));
        workspace.clear_filter();
        for character in "other".chars() {
            workspace.push_filter(character);
        }
        assert_eq!(workspace.visible_rows()[0].thread_id, "other");
        workspace.clear_filter();
        for character in "no-match".chars() {
            workspace.push_filter(character);
        }
        assert!(!workspace.begin_review());
    }

    #[test]
    fn recent_search_matches_managed_identity_independently_of_native_id() {
        let store = store_with("native-thread", true, false);
        let mut workspace = RecentSessionsWorkspace::default();
        workspace.receive(CatalogReply::Page { cursor: None, result: Ok(ThreadPage {
            data: vec![thread("native-thread", "other-session", 1)],
            next_cursor: Some("next".into()), backwards_cursor: None,
        })}, &store);
        for c in "cutex-test".chars() { workspace.push_filter(c); }
        assert_eq!(workspace.visible_rows().len(), 1);
        assert_eq!(workspace.visible_rows()[0].thread_id, "native-thread");
        assert_eq!(workspace.next_cursor().as_deref(), Some("next"));
    }

    #[test]
    fn adoption_requires_an_explicit_confirmation_and_escape_is_safe() {
        let mut workspace = RecentSessionsWorkspace::default();
        workspace.receive(
            CatalogReply::Page {
                cursor: None,
                result: Ok(ThreadPage {
                    data: vec![thread("thread-1", "tree", 1)],
                    next_cursor: None,
                    backwards_cursor: None,
                }),
            },
            &CutexSessionStore::default(),
        );
        assert!(workspace.begin_review());
        assert!(workspace.adoption_request().is_none());
        workspace.cancel_review();
        assert!(workspace.adoption_request().is_none());
        assert!(workspace.begin_review());
        workspace.set_review_confirmed(true);
        *workspace.adoption_name_input().unwrap() = tui_input::Input::new("Explicit name".into());
        assert_eq!(workspace.adoption_request().unwrap().thread_id, "thread-1");
    }

    #[test]
    fn thread_list_requests_native_recency_descending_in_bounded_pages() {
        let params = thread_list_params(None);
        assert_eq!(params.limit, Some(PAGE_SIZE));
        assert_eq!(params.sort_key, Some(ThreadSortKey::RecencyAt));
        assert_eq!(params.sort_direction, Some(SortDirection::Desc));
    }

    #[test]
    fn native_cwd_is_retained_exactly_while_empty_cwd_is_not_adoptable() {
        let long_cwd = format!("/work/{}", "segment".repeat(80));
        let mut long_thread = thread("thread-1", "tree", 1);
        long_thread.cwd = Some(long_cwd.clone().into());
        let row = recent_row(long_thread, &CutexSessionStore::default(), &HashMap::new());
        assert_eq!(row.cwd.as_deref(), Some(long_cwd.as_str()));

        let mut workspace = RecentSessionsWorkspace::default();
        workspace.rows = vec![row];
        assert!(workspace.begin_review());
        *workspace.adoption_name_input().unwrap() = tui_input::Input::new("Explicit name".into());
        workspace.set_review_confirmed(true);
        assert_eq!(workspace.adoption_request().unwrap().cwd, long_cwd);

        let mut empty_thread = thread("thread-2", "tree", 1);
        empty_thread.cwd = Some("".into());
        let empty = recent_row(empty_thread, &CutexSessionStore::default(), &HashMap::new());
        assert_eq!(empty.cwd, None);
        assert_eq!(empty.state, RecentThreadState::MissingCwd);
    }

    #[test]
    fn only_unmanaged_recent_rows_promote_the_native_thread_title() {
        let mut managed_thread = thread("thread-1", "tree", 1);
        managed_thread.name = Some("Generated conversation title".to_string());
        let managed = recent_row(
            managed_thread,
            &store_with("thread-1", true, false),
            &HashMap::from([("thread-1".to_string(), "Stable Managed Name".to_string())]),
        );
        assert_eq!(managed.primary_label(), "Stable Managed Name");
        assert_eq!(managed.title, "Generated conversation title");

        let mut unmanaged_thread = thread("thread-2", "tree", 1);
        unmanaged_thread.name = Some("Unmanaged conversation title".to_string());
        let unmanaged = recent_row(
            unmanaged_thread,
            &CutexSessionStore::default(),
            &HashMap::new(),
        );
        assert_eq!(unmanaged.primary_label(), "Unmanaged conversation title");
    }

    #[test]
    fn durable_reprojection_preserves_an_authoritative_managed_name() {
        let mut managed_thread = thread("thread-1", "tree", 1);
        managed_thread.name = Some("Generated conversation title".to_string());
        let store = store_with("thread-1", true, false);
        let managed = recent_row(
            managed_thread,
            &store,
            &HashMap::from([("thread-1".to_string(), "Stable Managed Name".to_string())]),
        );
        let mut workspace = RecentSessionsWorkspace::default();
        workspace.rows = vec![managed];

        workspace.reproject(&store);

        assert_eq!(workspace.rows[0].primary_label(), "Stable Managed Name");
        assert_eq!(workspace.rows[0].title, "Generated conversation title");
    }

    #[test]
    fn transport_failure_from_a_connected_catalog_requires_client_recreation() {
        let result = Err(CatalogError::Transport("connection closed".to_string()));
        assert!(should_recreate_catalog_client_after(&result));
        let timeout = Err(CatalogError::Timeout {
            method: "thread/list".to_string(),
        });
        assert!(should_recreate_catalog_client_after(&timeout));
        let other = Err(CatalogError::Protocol("bad response".to_string()));
        assert!(!should_recreate_catalog_client_after(&other));
    }

    #[test]
    fn store_reconciliation_failure_leaves_a_non_loading_retryable_state() {
        let mut workspace = RecentSessionsWorkspace::default();
        workspace.reconciliation_failed(
            CatalogReply::Page {
                cursor: Some("next-page".to_string()),
                result: Ok(ThreadPage {
                    data: vec![thread("thread-1", "tree", 1)],
                    next_cursor: None,
                    backwards_cursor: None,
                }),
            },
            "durable store unavailable".to_string(),
        );
        assert!(!workspace.loading());
        assert_eq!(
            workspace.cursor_for(RecentCommand::Retry).as_deref(),
            Some("next-page")
        );
        assert!(
            matches!(workspace.load_state(), RecentLoadState::Failed(message) if message.contains("durable store unavailable"))
        );
    }
}
