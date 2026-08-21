use std::ops::Range;
use std::sync::Arc;

use collections::HashMap;
use editor::actions::{
    FindAllReferences, GoToDeclaration, GoToDefinition, GoToImplementation, GoToTypeDefinition,
};
use editor::{Editor, EditorSettings, GotoDefinitionKind, Navigated, OpenResultsIn};
use file_icons::FileIcons;
use fuzzy::StringMatchCandidate;
use gpui::{
    AnyElement, App, AppContext, AsyncWindowContext, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, HighlightStyle, StyledText, Subscription, Task, TextStyle, WeakEntity,
    prelude::*,
};
use language::{Buffer, HighlightId, LanguageAwareStyling};
use picker::{Picker, PickerDelegate};
use project::{Location, Project, ProjectPath};
use settings::{GoToDefinitionFallback, Settings as _};
use text::{Anchor, Point};
use theme_settings::ThemeSettings;
use ui::{Divider, FluentBuilder};
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::item::ItemSettings;
use workspace::notifications::NotificationId;
use workspace::{ModalView, Toast, Workspace};

pub fn init(cx: &mut App) {
    cx.observe_new(register).detach();
}

/// Registers handlers for the navigation actions on each full editor. When the
/// action resolves to [`OpenResultsIn::Picker`], we open the filterable picker;
/// otherwise we `cx.propagate()` so the editor's own handler runs and builds a
/// multibuffer.
fn register(editor: &mut Editor, _window: Option<&mut Window>, cx: &mut Context<Editor>) {
    if !editor.mode().is_full() {
        return;
    }
    editor.set_definition_locations_handler(show_definition_locations);
    let handle = cx.entity().downgrade();
    editor
        .register_action({
            let handle = handle.clone();
            move |action: &GoToDefinition, window, cx| {
                handle_nav_action(
                    action.open_results_in,
                    LspPickerKind::Definition,
                    &handle,
                    window,
                    cx,
                );
            }
        })
        .detach();
    editor
        .register_action({
            let handle = handle.clone();
            move |action: &GoToDeclaration, window, cx| {
                handle_nav_action(
                    action.open_results_in,
                    LspPickerKind::Declaration,
                    &handle,
                    window,
                    cx,
                );
            }
        })
        .detach();
    editor
        .register_action({
            let handle = handle.clone();
            move |action: &GoToImplementation, window, cx| {
                handle_nav_action(
                    action.open_results_in,
                    LspPickerKind::Implementation,
                    &handle,
                    window,
                    cx,
                );
            }
        })
        .detach();
    editor
        .register_action({
            let handle = handle.clone();
            move |action: &GoToTypeDefinition, window, cx| {
                handle_nav_action(
                    action.open_results_in,
                    LspPickerKind::TypeDefinition,
                    &handle,
                    window,
                    cx,
                );
            }
        })
        .detach();
    editor
        .register_action(move |action: &FindAllReferences, window, cx| {
            handle_nav_action(
                action.open_results_in,
                LspPickerKind::References,
                &handle,
                window,
                cx,
            );
        })
        .detach();
}

/// Either opens the picker for the editor, or propagates the action so the
/// editor's built-in (multibuffer) handler runs. A `None` argument falls back to
/// the `lsp_results_location` setting.
fn handle_nav_action(
    open_results_in: Option<OpenResultsIn>,
    kind: LspPickerKind,
    editor: &WeakEntity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let open_results_in =
        open_results_in.unwrap_or_else(|| EditorSettings::get_global(cx).lsp_results_location);
    if open_results_in != OpenResultsIn::Picker
        || !LspLocationsPicker::open_for_editor(kind, editor.clone(), window, cx)
    {
        cx.propagate();
    }
}

fn show_definition_locations(
    editor: &mut Editor,
    kind: GotoDefinitionKind,
    locations: &[Location],
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Option<Task<anyhow::Result<Navigated>>> {
    let workspace = editor.workspace()?.downgrade();
    if locations
        .iter()
        .any(|location| location.buffer.read(cx).file().is_none())
    {
        return None;
    }
    let matches = build_location_matches(locations, cx);
    if matches.len() <= 1 {
        return None;
    }
    let kind = match kind {
        GotoDefinitionKind::Symbol => LspPickerKind::Definition,
        GotoDefinitionKind::Type => LspPickerKind::TypeDefinition,
        GotoDefinitionKind::Declaration => LspPickerKind::Declaration,
        GotoDefinitionKind::Implementation => LspPickerKind::Implementation,
    };
    Some(cx.spawn_in(window, async move |editor, cx| {
        let Some(editor) = editor.upgrade() else {
            return Ok(Navigated::No);
        };
        workspace.update_in(cx, |workspace, window, cx| {
            let project = workspace.project().clone();
            workspace.toggle_modal(window, cx, |window, cx| {
                LspLocationsPicker::new(kind, matches, project, editor.downgrade(), window, cx)
            });
            if workspace.active_modal::<LspLocationsPicker>(cx).is_some() {
                Navigated::Yes
            } else {
                Navigated::No
            }
        })
    }))
}

/// Runs the LSP query for `kind` and returns the raw locations. Returns `None`
/// (and reports any error) when there is nothing to query, so the caller stops
/// without an empty-results toast. Deduplication and dropping fileless results
/// happen later in [`build_location_matches`].
async fn run_picker_query(
    kind: LspPickerKind,
    editor: &WeakEntity<Editor>,
    workspace: &WeakEntity<Workspace>,
    project: &Entity<Project>,
    cx: &mut AsyncWindowContext,
) -> Option<Vec<Location>> {
    let query = editor
        .update(cx, |editor, cx| kind.run_query(editor, project, cx))
        .ok()
        .flatten()?;
    match query.await {
        Ok(locations) => Some(locations),
        Err(error) => {
            log::error!("LSP {kind:?} query failed: {error:#}");
            workspace
                .update(cx, |workspace, cx| workspace.show_error(error, cx))
                .log_err();
            None
        }
    }
}

/// Runs the query for `kind` and builds the displayable, deduped matches.
async fn run_picker_matches(
    kind: LspPickerKind,
    editor: &WeakEntity<Editor>,
    workspace: &WeakEntity<Workspace>,
    project: &Entity<Project>,
    cx: &mut AsyncWindowContext,
) -> Option<Vec<LocationMatch>> {
    let locations = run_picker_query(kind, editor, workspace, project, cx).await?;
    editor
        .update(cx, |_, cx| build_location_matches(&locations, cx))
        .ok()
}

fn show_no_results_toast(
    workspace: &WeakEntity<Workspace>,
    kind: LspPickerKind,
    cx: &mut AsyncWindowContext,
) {
    workspace
        .update(cx, |workspace, cx| {
            struct NoLspResults;
            workspace.show_toast(
                Toast::new(
                    NotificationId::unique::<NoLspResults>(),
                    kind.empty_message(),
                )
                .autohide(),
                cx,
            );
        })
        .log_err();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LspPickerKind {
    References,
    Definition,
    Declaration,
    Implementation,
    TypeDefinition,
}

impl LspPickerKind {
    fn placeholder(self) -> &'static str {
        match self {
            LspPickerKind::References => "Filter references…",
            LspPickerKind::Definition => "Filter definitions…",
            LspPickerKind::Declaration => "Filter declarations…",
            LspPickerKind::Implementation => "Filter implementations…",
            LspPickerKind::TypeDefinition => "Filter type definitions…",
        }
    }

    /// Message shown when the query produces no results, so the command does not
    /// appear to silently do nothing.
    fn empty_message(self) -> &'static str {
        match self {
            LspPickerKind::References => "No references found",
            LspPickerKind::Definition => "No definitions found",
            LspPickerKind::Declaration => "No declarations found",
            LspPickerKind::Implementation => "No implementations found",
            LspPickerKind::TypeDefinition => "No type definitions found",
        }
    }

    /// Runs the query for this kind against the active editor, returning the raw
    /// locations to populate the picker.
    fn run_query(
        self,
        editor: &mut Editor,
        project: &Entity<Project>,
        cx: &mut Context<Editor>,
    ) -> Option<Task<anyhow::Result<Vec<Location>>>> {
        match self {
            LspPickerKind::References => editor.find_all_references_locations(project, cx),
            LspPickerKind::Definition => {
                editor.definition_locations_of_kind(GotoDefinitionKind::Symbol, cx)
            }
            LspPickerKind::Declaration => {
                editor.definition_locations_of_kind(GotoDefinitionKind::Declaration, cx)
            }
            LspPickerKind::Implementation => {
                editor.definition_locations_of_kind(GotoDefinitionKind::Implementation, cx)
            }
            LspPickerKind::TypeDefinition => {
                editor.definition_locations_of_kind(GotoDefinitionKind::Type, cx)
            }
        }
    }
}

pub struct LspLocationsPicker {
    picker: Entity<Picker<LspLocationsDelegate>>,
    _subscription: Subscription,
}

impl LspLocationsPicker {
    fn open_for_editor(
        kind: LspPickerKind,
        editor: WeakEntity<Editor>,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let Some(editor) = editor.upgrade() else {
            return false;
        };
        let Some(workspace) = editor.read(cx).workspace() else {
            return false;
        };
        workspace.update(cx, |workspace, cx| {
            Self::open(kind, editor, workspace, window, cx);
        });
        true
    }

    /// Opens the picker for `kind`: runs a fresh LSP query and shows the
    /// results. An empty definitions query falls back to references when the
    /// `go_to_definition_fallback` setting calls for it, matching
    /// [`Editor::go_to_definition`].
    fn open(
        kind: LspPickerKind,
        editor: Entity<Editor>,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let project = workspace.project().clone();
        let fallback = EditorSettings::get_global(cx).go_to_definition_fallback;
        let editor = editor.downgrade();
        cx.spawn_in(window, async move |workspace, cx| {
            // The kind the user invoked, kept for user-facing messages even if
            // the query below falls back to references.
            let invoked_kind = kind;
            let mut kind = kind;

            // Count on the built matches (not raw locations): they are deduped by
            // range and exclude fileless results, so a single distinct result
            // jumps directly and a fileless-only result reports "no results"
            // instead of opening a blank picker.
            let Some(mut matches) =
                run_picker_matches(kind, &editor, &workspace, &project, cx).await
            else {
                return;
            };

            if matches.is_empty()
                && kind == LspPickerKind::Definition
                && fallback == GoToDefinitionFallback::FindAllReferences
            {
                kind = LspPickerKind::References;
                let Some(references) =
                    run_picker_matches(kind, &editor, &workspace, &project, cx).await
                else {
                    return;
                };
                matches = references;
            }

            if matches.is_empty() {
                show_no_results_toast(&workspace, invoked_kind, cx);
                return;
            }

            if matches.len() == 1 {
                if let Some(location_match) = matches.into_iter().next() {
                    let location = Location {
                        buffer: location_match.buffer,
                        range: location_match.anchor_range,
                    };
                    if let Ok(task) = editor.update_in(cx, |editor, window, cx| {
                        editor.open_location(location, false, window, cx)
                    }) {
                        task.await.log_err();
                    }
                }
                return;
            }

            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.toggle_modal(window, cx, |window, cx| {
                        Self::new(kind, matches, project, editor, window, cx)
                    });
                })
                .log_err();
        })
        .detach();
    }

    fn new(
        kind: LspPickerKind,
        matches: Vec<LocationMatch>,
        project: Entity<Project>,
        editor: WeakEntity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let preview = picker_preview::editor_preview(project.clone(), window, cx);
        let delegate = LspLocationsDelegate::new(kind, matches, project, editor);
        let picker = cx.new(|cx| Picker::list_with_preview(delegate, preview, window, cx));
        let subscription = cx.subscribe(&picker, |_, _, _: &DismissEvent, cx| {
            cx.emit(DismissEvent);
        });
        Self {
            picker,
            _subscription: subscription,
        }
    }
}

impl ModalView for LspLocationsPicker {}

impl EventEmitter<DismissEvent> for LspLocationsPicker {}

impl Focusable for LspLocationsPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for LspLocationsPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().child(self.picker.clone())
    }
}

struct LocationMatch {
    path: ProjectPath,
    buffer: Entity<Buffer>,
    anchor_range: Range<Anchor>,
    range: Range<usize>,
    display_text: String,
    syntax_highlights: Vec<(Range<usize>, HighlightId)>,
    match_range: Range<usize>,
    line_number: u32,
}

/// A row in the grouped display list: a non-selectable file header, a match, or
/// a separator between file groups. `selected_index` indexes into this list.
enum Entry {
    Header(ProjectPath),
    Match(usize),
    Separator,
}

struct LspLocationsDelegate {
    kind: LspPickerKind,
    project: Entity<Project>,
    editor: WeakEntity<Editor>,
    all_matches: Vec<LocationMatch>,
    candidates: Arc<[StringMatchCandidate]>,
    matches: Vec<usize>,
    entries: Vec<Entry>,
    selected_index: usize,
    max_line_number: u32,
}

impl LspLocationsDelegate {
    fn new(
        kind: LspPickerKind,
        all_matches: Vec<LocationMatch>,
        project: Entity<Project>,
        editor: WeakEntity<Editor>,
    ) -> Self {
        // Match against the line text and the file path, mirroring the fuzzy
        // matching every other Zed picker uses.
        let candidates = all_matches
            .iter()
            .enumerate()
            .map(|(index, location_match)| {
                StringMatchCandidate::new(
                    index,
                    &format!(
                        "{} {}",
                        location_match.display_text,
                        location_match.path.path.as_unix_str()
                    ),
                )
            })
            .collect();
        let matches = (0..all_matches.len()).collect();
        let mut this = Self {
            kind,
            project,
            editor,
            all_matches,
            candidates,
            matches,
            entries: Vec::new(),
            selected_index: 0,
            max_line_number: 0,
        };
        this.rebuild_entries();
        this
    }

    /// Rebuilds the grouped [`Self::entries`] from the filtered [`Self::matches`]:
    /// one header per file, its matches, and a separator before every group
    /// after the first. Selection snaps to the first selectable row.
    fn rebuild_entries(&mut self) {
        let mut entries = Vec::with_capacity(self.matches.len());
        let mut last_path: Option<&ProjectPath> = None;
        let mut max_line_number = 0;
        for &match_index in &self.matches {
            let location_match = &self.all_matches[match_index];
            if last_path != Some(&location_match.path) {
                if last_path.is_some() {
                    entries.push(Entry::Separator);
                }
                entries.push(Entry::Header(location_match.path.clone()));
                last_path = Some(&location_match.path);
            }
            max_line_number = max_line_number.max(location_match.line_number);
            entries.push(Entry::Match(match_index));
        }
        self.entries = entries;
        self.max_line_number = max_line_number;
        self.selected_index = self.first_selectable_index().unwrap_or(0);
    }

    fn first_selectable_index(&self) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| matches!(entry, Entry::Match(_)))
    }

    fn selected_location_match(&self) -> Option<&LocationMatch> {
        match self.entries.get(self.selected_index)? {
            Entry::Match(match_index) => self.all_matches.get(*match_index),
            Entry::Header(_) | Entry::Separator => None,
        }
    }

    fn open_selected(&mut self, split: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(location_match) = self.selected_location_match() else {
            return;
        };
        let location = Location {
            buffer: location_match.buffer.clone(),
            range: location_match.anchor_range.clone(),
        };
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        editor
            .update(cx, |editor, cx| {
                editor.open_location(location, split, window, cx)
            })
            .detach_and_log_err(cx);
        cx.emit(DismissEvent);
    }
}

fn build_location_matches(locations: &[Location], cx: &App) -> Vec<LocationMatch> {
    use gpui::EntityId;
    let mut snapshots: HashMap<EntityId, language::BufferSnapshot> = HashMap::default();
    let mut matches = Vec::with_capacity(locations.len());

    for location in locations {
        let snapshot = snapshots
            .entry(location.buffer.entity_id())
            .or_insert_with(|| location.buffer.read(cx).snapshot());

        let Some(file) = snapshot.file() else {
            continue;
        };
        let path = ProjectPath {
            worktree_id: file.worktree_id(cx),
            path: file.path().clone(),
        };

        let start_offset: usize = snapshot.summary_for_anchor(&location.range.start);
        let end_offset: usize = snapshot.summary_for_anchor(&location.range.end);
        let row = snapshot.offset_to_point(start_offset).row;
        let line_start = snapshot.point_to_offset(Point::new(row, 0));
        let line_end = snapshot.point_to_offset(Point::new(row, snapshot.line_len(row)));
        let full_line: String = snapshot.text_for_range(line_start..line_end).collect();

        // The row shows the line with leading indentation trimmed. Offsets below
        // are relative to that displayed text.
        let display_text = full_line.trim_start().to_string();
        let visible_start = line_end.saturating_sub(display_text.len());
        let visible_end = line_end;

        // Precompute syntax highlights for the displayed text so rendering a row
        // never re-snapshots the buffer or re-runs highlighting.
        let mut syntax_highlights = Vec::new();
        let mut offset = 0;
        for chunk in snapshot.chunks(
            visible_start..visible_end,
            LanguageAwareStyling {
                tree_sitter: true,
                diagnostics: false,
            },
        ) {
            let chunk_len = chunk.text.len();
            if let Some(id) = chunk.syntax_highlight_id {
                syntax_highlights.push((offset..offset + chunk_len, id));
            }
            offset += chunk_len;
        }

        // The match span, clamped into the displayed text. `clamp` bounds each
        // endpoint to the line; `min`/`max` then keep the range well-ordered even
        // for a malformed/inverted LSP range (clamping alone preserves bounds but
        // not `start <= end`).
        let clamped_start = start_offset.clamp(visible_start, visible_end) - visible_start;
        let clamped_end = end_offset.clamp(visible_start, visible_end) - visible_start;
        let match_range = clamped_start.min(clamped_end)..clamped_start.max(clamped_end);

        matches.push(LocationMatch {
            path,
            buffer: location.buffer.clone(),
            anchor_range: location.range.clone(),
            range: start_offset..end_offset,
            display_text,
            syntax_highlights,
            match_range,
            line_number: row + 1,
        });
    }

    // Group by file and order by position so the grouped display list is stable,
    // then drop exact-duplicate ranges a server may report more than once.
    matches.sort_by(|a, b| a.path.cmp(&b.path).then(a.range.start.cmp(&b.range.start)));
    matches.dedup_by(|a, b| a.path == b.path && a.range == b.range);
    matches
}

impl PickerDelegate for LspLocationsDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "lsp locations picker"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> std::sync::Arc<str> {
        self.kind.placeholder().into()
    }

    fn match_count(&self) -> usize {
        self.entries.len()
    }

    fn can_select(&self, ix: usize, _window: &mut Window, _cx: &mut Context<Picker<Self>>) -> bool {
        matches!(self.entries.get(ix), Some(Entry::Match(_)))
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn select_on_hover(&self) -> bool {
        false
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_owned();
        let candidates = self.candidates.clone();
        cx.spawn(async move |picker, cx| {
            let matches = if query.is_empty() {
                (0..candidates.len()).collect()
            } else {
                let string_matches = fuzzy::match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    candidates.len(),
                    &Default::default(),
                    cx.background_executor().clone(),
                )
                .await;
                let mut indices = string_matches
                    .into_iter()
                    .map(|string_match| string_match.candidate_id)
                    .collect::<Vec<_>>();
                // Restore the file-grouped, positional order (fuzzy returns by score).
                indices.sort_unstable();
                indices
            };
            picker
                .update(cx, |picker, cx| {
                    picker.delegate.matches = matches;
                    picker.delegate.rebuild_entries();
                    cx.notify();
                })
                .ok();
        })
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.open_selected(secondary, window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn try_get_preview_data_for_match(&self, _cx: &App) -> Option<picker::PreviewUpdate> {
        let location_match = self.selected_location_match()?;
        Some(picker::PreviewUpdate::from_buffer(
            location_match.buffer.clone(),
            picker::MatchLocation {
                anchor_range: location_match.anchor_range.clone(),
                range: location_match.range.clone(),
            },
        ))
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        match self.entries.get(ix)? {
            Entry::Separator => Some(
                div()
                    .py(DynamicSpacing::Base04.rems(cx))
                    .child(Divider::horizontal())
                    .into_any_element(),
            ),
            Entry::Header(path) => {
                let path_style = self.project.read(cx).path_style(cx);
                let file_name = path
                    .path
                    .file_name()
                    .map(|name| name.to_string())
                    .unwrap_or_default();
                let directory = path
                    .path
                    .parent()
                    .map(|parent| parent.display(path_style))
                    .map(SharedString::new)
                    .unwrap_or_default();
                let file_icon = ItemSettings::get_global(cx)
                    .file_icons
                    .then(|| FileIcons::get_icon(path.path.as_std_path(), cx))
                    .flatten()
                    .map(|icon| {
                        Icon::from_path(icon)
                            .color(Color::Muted)
                            .size(IconSize::Small)
                    });
                Some(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .px(DynamicSpacing::Base06.rems(cx))
                        .py_1()
                        .gap_1p5()
                        .children(file_icon)
                        .child(
                            h_flex()
                                .gap_1()
                                .child(Label::new(file_name).size(LabelSize::Small))
                                .when(!directory.is_empty(), |this| {
                                    this.child(
                                        Label::new(directory)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .truncate_start(),
                                    )
                                }),
                        )
                        .into_any_element(),
                )
            }
            Entry::Match(match_index) => {
                let location_match = self.all_matches.get(*match_index)?;
                Some(
                    ListItem::new(ix)
                        .spacing(ListItemSpacing::Sparse)
                        .inset(true)
                        .toggle_state(selected)
                        .child(
                            h_flex()
                                .w_full()
                                .min_w_0()
                                .gap_2p5()
                                .text_sm()
                                .child(
                                    h_flex()
                                        .w(rems(
                                            (self.max_line_number.max(1).ilog10() + 1) as f32 * 0.5,
                                        ))
                                        .justify_end()
                                        .child(
                                            Label::new(location_match.line_number.to_string())
                                                .color(Color::Custom(
                                                    cx.theme().colors().text_muted.opacity(0.5),
                                                )),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .child(render_matched_line(location_match, cx)),
                                ),
                        )
                        .into_any_element(),
                )
            }
        }
    }
}

/// Renders the precomputed displayed line, resolving the stored syntax highlight
/// ids against the current theme and overlaying the match with a highlighted
/// background and bold weight.
fn render_matched_line(location_match: &LocationMatch, cx: &App) -> StyledText {
    let settings = ThemeSettings::get_global(cx);
    let text_style = TextStyle {
        color: cx.theme().colors().text,
        font_family: settings.buffer_font.family.clone(),
        font_features: settings.buffer_font.features.clone(),
        font_fallbacks: settings.buffer_font.fallbacks.clone(),
        font_size: settings.buffer_font_size(cx).into(),
        font_weight: settings.buffer_font.weight,
        line_height: relative(1.),
        ..Default::default()
    };

    let syntax_theme = cx.theme().syntax();
    let syntax_highlights = location_match
        .syntax_highlights
        .iter()
        .filter_map(|(range, id)| Some((range.clone(), syntax_theme.get(*id).copied()?)))
        .collect::<Vec<_>>();

    let match_style = HighlightStyle {
        background_color: Some(cx.theme().colors().search_match_background),
        font_weight: Some(gpui::FontWeight::BOLD),
        ..Default::default()
    };
    let match_highlight = (location_match.match_range.clone(), match_style);

    let highlights = gpui::combine_highlights(syntax_highlights, [match_highlight]);
    StyledText::new(location_match.display_text.clone())
        .with_default_highlights(&text_style, highlights)
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::HighlightKey;
    use editor::test::editor_lsp_test_context::EditorLspTestContext;
    use gpui::{Modifiers, TestAppContext};
    use indoc::indoc;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;
    use workspace::Item as _;

    gpui::actions!(lsp_locations_test, [TestPickerNavigation]);

    const SOURCE: &str = indoc! {r#"
        fn main() {
            let aˇbc = 123;
            let xyz = abc;
        }
    "#};

    #[gpui::test]
    async fn test_multiple_references_open_picker(cx: &mut TestAppContext) {
        let mut cx = rust_cx(
            lsp::ServerCapabilities {
                references_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        cx.set_state(SOURCE);
        cx.lsp
            .set_request_handler::<lsp::request::References, _, _>(async move |params, _| {
                let uri = params.text_document_position.text_document.uri;
                Ok(Some(references(uri, &[(1, 8, 11), (2, 14, 17)])))
            });

        open(&mut cx, LspPickerKind::References);

        assert!(
            active_picker(&mut cx).is_some(),
            "multiple references should open the picker"
        );
    }

    #[gpui::test]
    async fn test_single_result_jumps_without_picker(cx: &mut TestAppContext) {
        let mut cx = rust_cx(
            lsp::ServerCapabilities {
                references_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        cx.set_state(SOURCE);
        cx.lsp
            .set_request_handler::<lsp::request::References, _, _>(async move |params, _| {
                let uri = params.text_document_position.text_document.uri;
                Ok(Some(references(uri, &[(2, 14, 17)])))
            });

        open(&mut cx, LspPickerKind::References);

        assert!(
            active_picker(&mut cx).is_none(),
            "a single result should jump directly instead of opening the picker"
        );
        // The lone result at row 2 should be selected directly, moving the
        // cursor off its starting position on row 1.
        cx.assert_editor_state(indoc! {r#"
            fn main() {
                let abc = 123;
                let xyz = «abcˇ»;
            }
        "#});
    }

    #[gpui::test]
    async fn test_no_results_does_not_open_picker(cx: &mut TestAppContext) {
        let mut cx = rust_cx(
            lsp::ServerCapabilities {
                references_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        cx.set_state(SOURCE);
        cx.lsp
            .set_request_handler::<lsp::request::References, _, _>(async move |_params, _| {
                Ok(Some(Vec::new()))
            });

        open(&mut cx, LspPickerKind::References);

        assert!(
            active_picker(&mut cx).is_none(),
            "an empty result should not open the picker"
        );
    }

    #[gpui::test]
    async fn test_definition_falls_back_to_references_picker(cx: &mut TestAppContext) {
        let mut cx = rust_cx(
            lsp::ServerCapabilities {
                definition_provider: Some(lsp::OneOf::Left(true)),
                references_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        cx.set_state(SOURCE);
        cx.lsp
            .set_request_handler::<lsp::request::GotoDefinition, _, _>(async move |_params, _| {
                Ok(None)
            });
        cx.lsp
            .set_request_handler::<lsp::request::References, _, _>(async move |params, _| {
                let uri = params.text_document_position.text_document.uri;
                Ok(Some(references(uri, &[(1, 8, 11), (2, 14, 17)])))
            });

        open(&mut cx, LspPickerKind::Definition);

        assert!(
            active_picker(&mut cx).is_some(),
            "an empty definition query should fall back to the references picker"
        );
    }

    #[gpui::test]
    async fn test_fuzzy_filter_matches_subsequence(cx: &mut TestAppContext) {
        let mut cx = rust_cx(
            lsp::ServerCapabilities {
                references_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        cx.set_state(SOURCE);
        cx.lsp
            .set_request_handler::<lsp::request::References, _, _>(async move |params, _| {
                let uri = params.text_document_position.text_document.uri;
                Ok(Some(references(uri, &[(1, 8, 11), (2, 14, 17)])))
            });

        open(&mut cx, LspPickerKind::References);
        let modal = active_picker(&mut cx).expect("multiple references should open the picker");
        let picker = cx.update(|_window, cx| modal.read(cx).picker.clone());

        let matches = |cx: &mut EditorLspTestContext, query: &str| -> usize {
            cx.update(|window, cx| {
                picker.update(cx, |picker, cx| picker.set_query(query, window, cx));
            });
            cx.run_until_parked();
            cx.update(|_window, cx| picker.read(cx).delegate.matches.len())
        };

        // "lx" is a subsequence of "let xyz" but not a substring of either line,
        // so it only matches with fuzzy matching.
        assert_eq!(matches(&mut cx, "lx"), 1);
        assert_eq!(matches(&mut cx, "zzzz"), 0);
        assert_eq!(matches(&mut cx, ""), 2);
    }

    #[gpui::test]
    async fn test_cmd_click_fallback_honors_lsp_results_location(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let mut cx = rust_cx(
            lsp::ServerCapabilities {
                definition_provider: Some(lsp::OneOf::Left(true)),
                references_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        set_picker(&mut cx);
        cx.set_state(SOURCE);
        cx.lsp
            .set_request_handler::<lsp::request::GotoDefinition, _, _>(async move |_params, _| {
                Ok(None)
            });
        cx.lsp
            .set_request_handler::<lsp::request::References, _, _>(async move |params, _| {
                let uri = params.text_document_position.text_document.uri;
                Ok(Some(references(uri, &[(1, 8, 11), (2, 14, 17)])))
            });

        let screen_coord = cx
            .editor(|editor, _, cx| editor.pixel_position_of_cursor(cx))
            .unwrap();
        cx.simulate_click(screen_coord, gpui::Modifiers::secondary_key());
        cx.run_until_parked();

        assert!(
            active_picker(&mut cx).is_some(),
            "the cmd-click go-to-definition fallback should open the references picker when lsp_results_location is picker"
        );
    }

    #[gpui::test]
    async fn test_type_definition_honors_lsp_results_location(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let mut cx = rust_cx(
            lsp::ServerCapabilities {
                type_definition_provider: Some(lsp::TypeDefinitionProviderCapability::Simple(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        cx.update(|_window, cx| {
            cx.update_global::<settings::SettingsStore, _>(|settings, cx| {
                settings.update_user_settings(cx, |settings| {
                    settings.editor.lsp_results_location = Some(OpenResultsIn::Picker);
                });
            });
        });
        cx.set_state(indoc! {r#"
            fn main() {
                struct Foo<T>(T);
                struct Bar;
                let fˇoo: Foo<Bar>;
            }
        "#});
        cx.lsp
            .set_request_handler::<lsp::request::GotoTypeDefinition, _, _>(
                async move |params, _| {
                    let uri = params.text_document_position_params.text_document.uri;
                    Ok(Some(lsp::GotoDefinitionResponse::Array(references(
                        uri,
                        &[(1, 11, 14), (2, 11, 14)],
                    ))))
                },
            );

        cx.dispatch_action(GoToTypeDefinition::default());

        assert!(
            active_picker(&mut cx).is_some(),
            "type definition should open the picker when lsp_results_location is picker"
        );
    }

    #[gpui::test]
    async fn test_declaration_honors_lsp_results_location(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let mut cx = rust_cx(
            lsp::ServerCapabilities {
                declaration_provider: Some(lsp::DeclarationCapability::Simple(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        cx.update(|_window, cx| {
            cx.update_global::<settings::SettingsStore, _>(|settings, cx| {
                settings.update_user_settings(cx, |settings| {
                    settings.editor.lsp_results_location = Some(OpenResultsIn::Picker);
                });
            });
        });
        cx.set_state(indoc! {r#"
            fn main() {
                let foo = ();
                let foo = ();
                let bar = fˇoo;
            }
        "#});
        cx.lsp
            .set_request_handler::<lsp::request::GotoDeclaration, _, _>(async move |params, _| {
                let uri = params.text_document_position_params.text_document.uri;
                Ok(Some(lsp::GotoDefinitionResponse::Array(references(
                    uri,
                    &[(1, 8, 11), (2, 8, 11)],
                ))))
            });

        cx.dispatch_action(GoToDeclaration::default());

        assert!(
            active_picker(&mut cx).is_some(),
            "declaration should open the picker when lsp_results_location is picker"
        );
    }

    #[gpui::test]
    async fn test_cmd_click_multiple_definitions_honors_lsp_results_location(
        cx: &mut TestAppContext,
    ) {
        assert_cached_definition_picker(GotoDefinitionKind::Symbol, cx).await;
    }

    #[gpui::test]
    async fn test_cmd_shift_click_type_definitions_honor_lsp_results_location(
        cx: &mut TestAppContext,
    ) {
        assert_cached_definition_picker(GotoDefinitionKind::Type, cx).await;
    }

    #[gpui::test]
    async fn test_cmd_click_no_definition_respects_disabled_fallback(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let mut cx = rust_cx(
            lsp::ServerCapabilities {
                definition_provider: Some(lsp::OneOf::Left(true)),
                references_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        set_picker(&mut cx);
        cx.update(|_window, cx| {
            cx.update_global::<settings::SettingsStore, _>(|settings, cx| {
                settings.update_user_settings(cx, |settings| {
                    settings.editor.go_to_definition_fallback = Some(GoToDefinitionFallback::None);
                });
            });
        });
        cx.set_state(SOURCE);
        cx.lsp
            .set_request_handler::<lsp::request::GotoDefinition, _, _>(async move |_params, _| {
                Ok(None)
            });
        cx.lsp
            .set_request_handler::<lsp::request::References, _, _>(async move |_params, _| {
                panic!(
                    "cmd-click should not request references when go_to_definition_fallback is none"
                )
            });

        let screen_coord = cx
            .editor(|editor, _, cx| editor.pixel_position_of_cursor(cx))
            .unwrap();
        cx.simulate_click(screen_coord, gpui::Modifiers::secondary_key());
        cx.run_until_parked();

        assert!(
            active_picker(&mut cx).is_none(),
            "cmd-click should not open the references picker when go_to_definition_fallback is none"
        );
    }

    #[gpui::test]
    async fn test_cmd_click_cached_single_definition_preserves_native_navigation(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::init);
        for kind in [GotoDefinitionKind::Symbol, GotoDefinitionKind::Type] {
            let mut cx = definition_cx(cx).await;
            let requests =
                track_definition_requests(&mut cx, kind, &[(2, 14, 17), (1, 8, 11), (2, 14, 17)]);
            let position = cx.pixel_position(SOURCE);
            let modifiers = definition_modifiers(kind);
            cx.simulate_mouse_move(position, None, modifiers);
            cx.run_until_parked();
            assert_definition_highlight(&mut cx);
            assert_eq!(
                *requests.lock().expect("requests"),
                [lsp::Position::new(1, 9)]
            );

            cx.simulate_click(position, modifiers);
            cx.run_until_parked();

            assert!(active_picker(&mut cx).is_none());
            cx.assert_editor_state(indoc! {r#"
                fn main() {
                    let abc = 123;
                    let xyz = «abcˇ»;
                }
            "#});
            assert_hover_cleared(&mut cx);
            assert_eq!(
                *requests.lock().expect("requests"),
                [lsp::Position::new(1, 9)]
            );
        }
    }

    #[gpui::test]
    async fn test_cmd_click_cold_definition_uses_click_position(cx: &mut TestAppContext) {
        cx.update(crate::init);
        for kind in [GotoDefinitionKind::Symbol, GotoDefinitionKind::Type] {
            let mut cx = definition_cx(cx).await;
            let requests =
                track_definition_requests(&mut cx, kind, &[(2, 14, 17), (1, 8, 11), (0, 3, 7)]);
            let position = cx.pixel_position(indoc! {r#"
                fn main() {
                    let abc = 123;
                    let xyz = aˇbc;
                }
            "#});
            cx.assert_editor_state(SOURCE);
            assert_hover_cleared(&mut cx);

            cx.simulate_click(position, definition_modifiers(kind));
            cx.run_until_parked();

            assert_eq!(
                *requests.lock().expect("requests"),
                [lsp::Position::new(2, 15)]
            );
            assert_picker_locations(
                &mut cx,
                if kind == GotoDefinitionKind::Type {
                    LspPickerKind::TypeDefinition
                } else {
                    LspPickerKind::Definition
                },
                &[(0, 3, 7), (1, 8, 11)],
            );
            assert_hover_cleared(&mut cx);
        }
    }

    #[gpui::test]
    async fn test_cmd_shift_click_empty_types_respects_references_fallback(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::init);
        for fallback in [
            GoToDefinitionFallback::FindAllReferences,
            GoToDefinitionFallback::None,
        ] {
            let mut cx = definition_cx(cx).await;
            cx.update(|_, cx| {
                cx.update_global::<settings::SettingsStore, _>(|settings, cx| {
                    settings.update_user_settings(cx, |settings| {
                        settings.editor.go_to_definition_fallback = Some(fallback);
                    });
                });
            });
            let definitions = track_definition_requests(&mut cx, GotoDefinitionKind::Type, &[]);
            let references = Arc::new(Mutex::new(Vec::new()));
            cx.lsp
                .set_request_handler::<lsp::request::References, _, _>({
                    let references = references.clone();
                    let uri = cx.buffer_lsp_url.clone();
                    move |params, _| {
                        assert_eq!(params.text_document_position.text_document.uri, uri);
                        references
                            .lock()
                            .expect("requests")
                            .push(params.text_document_position.position);
                        let result = self::references(uri.clone(), &[(1, 8, 11), (2, 14, 17)]);
                        async move { Ok(Some(result)) }
                    }
                });
            let position = cx.pixel_position(SOURCE);

            cx.simulate_click(position, definition_modifiers(GotoDefinitionKind::Type));
            cx.run_until_parked();

            assert_eq!(
                *definitions.lock().expect("requests"),
                [lsp::Position::new(1, 9)]
            );
            if fallback == GoToDefinitionFallback::FindAllReferences {
                assert_eq!(
                    *references.lock().expect("requests"),
                    [lsp::Position::new(1, 9)]
                );
                assert_picker_locations(
                    &mut cx,
                    LspPickerKind::References,
                    &[(1, 8, 11), (2, 14, 17)],
                );
            } else {
                assert_eq!(
                    *references.lock().expect("requests"),
                    Vec::<lsp::Position>::new()
                );
                assert!(active_picker(&mut cx).is_none());
            }
            cx.assert_editor_state(SOURCE);
            assert_hover_cleared(&mut cx);
        }
    }

    #[gpui::test]
    async fn test_cmd_shift_click_during_type_hover_ignores_cached_symbol(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let mut cx = definition_cx(cx).await;
        let symbols = track_definition_requests(&mut cx, GotoDefinitionKind::Symbol, &[(0, 3, 7)]);
        let types = Arc::new(Mutex::new(Vec::new()));
        cx.lsp
            .set_request_handler::<lsp::request::GotoTypeDefinition, _, _>({
                let types = types.clone();
                let uri = cx.buffer_lsp_url.clone();
                move |params, cx| {
                    assert_eq!(params.text_document_position_params.text_document.uri, uri);
                    let request_count = {
                        let mut types = types.lock().expect("requests");
                        types.push(params.text_document_position_params.position);
                        types.len()
                    };
                    let result = definitions(uri.clone(), &[(2, 8, 11), (2, 14, 17)]);
                    async move {
                        if request_count == 1 {
                            cx.background_executor().timer(Duration::from_secs(1)).await;
                        }
                        Ok(Some(result))
                    }
                }
            });
        let position = cx.pixel_position(SOURCE);
        cx.simulate_mouse_move(position, None, Modifiers::secondary_key());
        cx.run_until_parked();
        assert_definition_highlight(&mut cx);
        assert_eq!(
            *symbols.lock().expect("requests"),
            [lsp::Position::new(1, 9)]
        );
        let modifiers = definition_modifiers(GotoDefinitionKind::Type);
        cx.simulate_modifiers_change(modifiers);
        cx.run_until_parked();
        assert_eq!(*types.lock().expect("requests"), [lsp::Position::new(1, 9)]);
        assert!(active_picker(&mut cx).is_none());

        cx.simulate_click(position, modifiers);
        cx.run_until_parked();

        assert_picker_locations(
            &mut cx,
            LspPickerKind::TypeDefinition,
            &[(2, 8, 11), (2, 14, 17)],
        );
        assert_hover_cleared(&mut cx);
        assert_eq!(
            *symbols.lock().expect("requests"),
            [lsp::Position::new(1, 9)]
        );
        assert_eq!(
            *types.lock().expect("requests"),
            [lsp::Position::new(1, 9), lsp::Position::new(1, 9)],
        );
        cx.background_executor.advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_picker_locations(
            &mut cx,
            LspPickerKind::TypeDefinition,
            &[(2, 8, 11), (2, 14, 17)],
        );
        assert_hover_cleared(&mut cx);
    }

    #[gpui::test]
    async fn test_cmd_click_during_new_symbol_hover_ignores_cached_symbol(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let mut cx = definition_cx(cx).await;
        let requests = Arc::new(Mutex::new(Vec::new()));
        cx.lsp
            .set_request_handler::<lsp::request::GotoDefinition, _, _>({
                let requests = requests.clone();
                let uri = cx.buffer_lsp_url.clone();
                move |params, cx| {
                    assert_eq!(params.text_document_position_params.text_document.uri, uri);
                    let position = params.text_document_position_params.position;
                    let request_count = {
                        let mut requests = requests.lock().expect("requests");
                        requests.push(position);
                        requests.len()
                    };
                    let result = match (position.line, position.character) {
                        (1, 9) => definitions(uri.clone(), &[(0, 3, 7)]),
                        (2, 9) => definitions(uri.clone(), &[(1, 8, 11), (2, 14, 17)]),
                        _ => panic!("unexpected definition position: {position:?}"),
                    };
                    async move {
                        if request_count == 2 {
                            cx.background_executor().timer(Duration::from_secs(1)).await;
                        }
                        Ok(Some(result))
                    }
                }
            });
        let first_position = cx.pixel_position(SOURCE);
        let second_position = cx.pixel_position(indoc! {r#"
            fn main() {
                let abc = 123;
                let xˇyz = abc;
            }
        "#});
        let modifiers = Modifiers::secondary_key();
        cx.simulate_mouse_move(first_position, None, modifiers);
        cx.run_until_parked();
        assert_definition_highlight(&mut cx);
        assert_eq!(
            *requests.lock().expect("requests"),
            [lsp::Position::new(1, 9)]
        );

        cx.simulate_mouse_move(second_position, None, modifiers);
        cx.run_until_parked();
        assert_eq!(
            *requests.lock().expect("requests"),
            [lsp::Position::new(1, 9), lsp::Position::new(2, 9)],
        );
        cx.assert_editor_state(SOURCE);
        assert!(active_picker(&mut cx).is_none());

        cx.simulate_click(second_position, modifiers);
        cx.run_until_parked();

        assert_eq!(
            *requests.lock().expect("requests"),
            [
                lsp::Position::new(1, 9),
                lsp::Position::new(2, 9),
                lsp::Position::new(2, 9)
            ],
        );
        assert_picker_locations(
            &mut cx,
            LspPickerKind::Definition,
            &[(1, 8, 11), (2, 14, 17)],
        );
        assert_hover_cleared(&mut cx);
        cx.background_executor.advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(
            *requests.lock().expect("requests"),
            [
                lsp::Position::new(1, 9),
                lsp::Position::new(2, 9),
                lsp::Position::new(2, 9)
            ],
        );
        assert_picker_locations(
            &mut cx,
            LspPickerKind::Definition,
            &[(1, 8, 11), (2, 14, 17)],
        );
        assert_hover_cleared(&mut cx);
    }

    #[gpui::test]
    async fn test_cmd_click_default_and_split_preserve_native_navigation(cx: &mut TestAppContext) {
        cx.update(crate::init);
        for (open_results_in, split) in [
            (OpenResultsIn::MultiBuffer, false),
            (OpenResultsIn::Picker, true),
        ] {
            for kind in [GotoDefinitionKind::Symbol, GotoDefinitionKind::Type] {
                let mut cx = definition_cx(cx).await;
                cx.update(|_, cx| {
                    cx.update_global::<settings::SettingsStore, _>(|settings, cx| {
                        settings.update_user_settings(cx, |settings| {
                            settings.editor.lsp_results_location = Some(open_results_in);
                        });
                    });
                });
                let requests = track_definition_requests(&mut cx, kind, &[(0, 3, 7), (2, 14, 17)]);
                let position = cx.pixel_position(SOURCE);
                let modifiers = Modifiers {
                    alt: split,
                    ..definition_modifiers(kind)
                };
                cx.simulate_mouse_move(position, None, modifiers);
                cx.run_until_parked();
                assert_definition_highlight(&mut cx);

                cx.simulate_click(position, modifiers);
                cx.run_until_parked();

                assert!(active_picker(&mut cx).is_none());
                assert_hover_cleared(&mut cx);
                assert_eq!(
                    *requests.lock().expect("requests"),
                    [lsp::Position::new(1, 9)]
                );
                let workspace = cx.workspace.clone();
                let source_editor = cx.editor.clone();
                cx.update(|_, cx| {
                    let workspace = workspace.read(cx);
                    assert_eq!(workspace.panes().len(), if split { 2 } else { 1 });
                    let editor = workspace
                        .active_item_as::<Editor>(cx)
                        .expect("active editor");
                    if split {
                        assert_ne!(editor, source_editor);
                    } else {
                        assert_eq!(editor, source_editor);
                    }
                    editor.update(cx, |editor, cx| {
                        assert_eq!(
                            editor
                                .selections
                                .all::<Point>(&editor.display_snapshot(cx))
                                .into_iter()
                                .map(|selection| selection.range())
                                .collect::<Vec<_>>(),
                            [
                                Point::new(0, 3)..Point::new(0, 7),
                                Point::new(2, 14)..Point::new(2, 17)
                            ],
                        );
                    });
                });
            }
        }
    }

    #[gpui::test]
    async fn test_definition_locations_handler_declines_empty_single_and_fileless(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::init);
        let mut cx = definition_cx(cx).await;
        let locations = cx.update_editor(|editor, _, cx| {
            let buffer = editor
                .buffer()
                .read(cx)
                .as_singleton()
                .expect("singleton buffer");
            let snapshot = buffer.read(cx).snapshot();
            [3..7, 45..48]
                .into_iter()
                .map(|range| Location {
                    buffer: buffer.clone(),
                    range: snapshot.anchor_before(range.start)..snapshot.anchor_after(range.end),
                })
                .collect::<Vec<_>>()
        });
        let fileless = cx.update(|_, cx| {
            let buffer = cx.new(|cx| Buffer::local("abc", cx));
            let snapshot = buffer.read(cx).snapshot();
            Location {
                range: snapshot.anchor_before(0)..snapshot.anchor_after(3),
                buffer,
            }
        });
        for locations in [
            Vec::new(),
            vec![locations[0].clone()],
            vec![locations[0].clone(), locations[0].clone()],
            vec![fileless.clone()],
            vec![locations[0].clone(), fileless, locations[1].clone()],
        ] {
            let task = cx.update_editor(|editor, window, cx| {
                show_definition_locations(
                    editor,
                    GotoDefinitionKind::Symbol,
                    &locations,
                    window,
                    cx,
                )
            });
            assert!(task.is_none());
        }
        cx.run_until_parked();
        assert!(active_picker(&mut cx).is_none());
        cx.assert_editor_state(SOURCE);
    }

    #[gpui::test]
    async fn test_picker_navigation_without_workspace_propagates(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let mut cx = definition_cx(cx).await;
        let standalone = cx.update(|window, cx| {
            let buffer = cx.new(|cx| Buffer::local("abc", cx));
            cx.new(|cx| Editor::for_buffer(buffer, None, window, cx))
        });
        cx.update(|window, cx| {
            standalone.update(cx, |editor, cx| {
                assert!(editor.workspace().is_none());
                let buffer = editor
                    .buffer()
                    .read(cx)
                    .as_singleton()
                    .expect("singleton buffer");
                let snapshot = buffer.read(cx).snapshot();
                let locations = [0..1, 2..3].map(|range| Location {
                    buffer: buffer.clone(),
                    range: snapshot.anchor_before(range.start)..snapshot.anchor_after(range.end),
                });
                assert!(
                    show_definition_locations(
                        editor,
                        GotoDefinitionKind::Symbol,
                        &locations,
                        window,
                        cx
                    )
                    .is_none()
                );
            })
        });
        let propagated = Arc::new(AtomicUsize::new(0));
        cx.update_editor(|editor, _, cx| {
            let standalone = standalone.downgrade();
            editor
                .register_action(move |_: &TestPickerNavigation, window, cx| {
                    handle_nav_action(
                        Some(OpenResultsIn::Picker),
                        LspPickerKind::Definition,
                        &standalone,
                        window,
                        cx,
                    );
                })
                .detach();
            App::on_action::<TestPickerNavigation>(cx, {
                let propagated = propagated.clone();
                move |_, _| {
                    propagated.fetch_add(1, Ordering::SeqCst);
                }
            });
        });

        cx.dispatch_action(TestPickerNavigation);

        assert_eq!(propagated.load(Ordering::SeqCst), 1);
        assert!(active_picker(&mut cx).is_none());
    }

    #[gpui::test]
    async fn test_definition_locations_handler_does_not_present_after_editor_dropped(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::init);
        let mut cx = definition_cx(cx).await;
        let workspace = cx.workspace.clone();
        let buffer = cx.editor(|editor, _, cx| {
            editor
                .buffer()
                .read(cx)
                .as_singleton()
                .expect("singleton buffer")
        });
        let source_editor = cx.update(|window, cx| {
            let source_editor = cx.new(|cx| Editor::for_buffer(buffer, None, window, cx));
            workspace.update(cx, |workspace, cx| {
                source_editor.update(cx, |editor, cx| {
                    editor.added_to_workspace(workspace, window, cx);
                });
            });
            source_editor
        });
        cx.run_until_parked();
        let weak_editor = source_editor.downgrade();
        let task = cx.update(|window, cx| {
            let task = source_editor.update(cx, |editor, cx| {
                let buffer = editor
                    .buffer()
                    .read(cx)
                    .as_singleton()
                    .expect("singleton buffer");
                let snapshot = buffer.read(cx).snapshot();
                let locations = [3..7, 45..48].map(|range| Location {
                    buffer: buffer.clone(),
                    range: snapshot.anchor_before(range.start)..snapshot.anchor_after(range.end),
                });
                show_definition_locations(
                    editor,
                    GotoDefinitionKind::Symbol,
                    &locations,
                    window,
                    cx,
                )
                .expect("presenter task")
            });
            drop(source_editor);
            assert!(weak_editor.upgrade().is_none());
            assert!(
                workspace
                    .read(cx)
                    .active_modal::<LspLocationsPicker>(cx)
                    .is_none()
            );
            task
        });

        assert_eq!(task.await.expect("presenter result"), Navigated::No);

        cx.run_until_parked();
        assert!(weak_editor.upgrade().is_none());
        assert!(active_picker(&mut cx).is_none());
        cx.assert_editor_state(SOURCE);
    }

    async fn rust_cx(
        capabilities: lsp::ServerCapabilities,
        cx: &mut TestAppContext,
    ) -> EditorLspTestContext {
        EditorLspTestContext::new_rust(capabilities, cx).await
    }

    fn open(cx: &mut EditorLspTestContext, kind: LspPickerKind) {
        let editor = cx.editor.clone();
        let workspace = cx.workspace.clone();
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                LspLocationsPicker::open(kind, editor, workspace, window, cx);
            });
        });
        cx.run_until_parked();
    }

    fn active_picker(cx: &mut EditorLspTestContext) -> Option<Entity<LspLocationsPicker>> {
        let workspace = cx.workspace.clone();
        cx.update(|_window, cx| workspace.read(cx).active_modal::<LspLocationsPicker>(cx))
    }

    fn set_picker(cx: &mut EditorLspTestContext) {
        cx.update(|_window, cx| {
            cx.update_global::<settings::SettingsStore, _>(|settings, cx| {
                settings.update_user_settings(cx, |settings| {
                    settings.editor.lsp_results_location = Some(OpenResultsIn::Picker);
                });
            });
        });
    }

    fn definitions(uri: lsp::Uri, ranges: &[(u32, u32, u32)]) -> lsp::GotoDefinitionResponse {
        lsp::GotoDefinitionResponse::Array(references(uri, ranges))
    }

    fn references(uri: lsp::Uri, ranges: &[(u32, u32, u32)]) -> Vec<lsp::Location> {
        ranges
            .iter()
            .map(|&(row, start, end)| lsp::Location {
                uri: uri.clone(),
                range: lsp::Range::new(
                    lsp::Position::new(row, start),
                    lsp::Position::new(row, end),
                ),
            })
            .collect()
    }

    async fn definition_cx(cx: &mut TestAppContext) -> EditorLspTestContext {
        let mut cx = rust_cx(
            lsp::ServerCapabilities {
                definition_provider: Some(lsp::OneOf::Left(true)),
                type_definition_provider: Some(lsp::TypeDefinitionProviderCapability::Simple(true)),
                references_provider: Some(lsp::OneOf::Left(true)),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await;
        set_picker(&mut cx);
        cx.set_state(SOURCE);
        cx
    }

    fn definition_modifiers(kind: GotoDefinitionKind) -> Modifiers {
        Modifiers {
            shift: kind == GotoDefinitionKind::Type,
            ..Modifiers::secondary_key()
        }
    }

    fn track_definition_requests(
        cx: &mut EditorLspTestContext,
        kind: GotoDefinitionKind,
        ranges: &[(u32, u32, u32)],
    ) -> Arc<Mutex<Vec<lsp::Position>>> {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler = {
            let requests = requests.clone();
            let uri = cx.buffer_lsp_url.clone();
            let result = definitions(uri.clone(), ranges);
            move |params: lsp::GotoDefinitionParams, _| {
                assert_eq!(params.text_document_position_params.text_document.uri, uri);
                requests
                    .lock()
                    .expect("requests")
                    .push(params.text_document_position_params.position);
                let result = result.clone();
                async move { Ok(Some(result)) }
            }
        };
        match kind {
            GotoDefinitionKind::Symbol => {
                cx.lsp
                    .set_request_handler::<lsp::request::GotoDefinition, _, _>(handler);
            }
            GotoDefinitionKind::Type => {
                cx.lsp
                    .set_request_handler::<lsp::request::GotoTypeDefinition, _, _>(handler);
            }
            _ => panic!("unexpected definition kind: {kind:?}"),
        }
        requests
    }

    async fn assert_cached_definition_picker(kind: GotoDefinitionKind, cx: &mut TestAppContext) {
        cx.update(crate::init);
        let mut cx = definition_cx(cx).await;
        let requests = track_definition_requests(
            &mut cx,
            kind,
            &[(2, 14, 17), (1, 8, 11), (0, 3, 7), (2, 14, 17)],
        );
        let position = cx.pixel_position(SOURCE);
        let modifiers = definition_modifiers(kind);
        cx.simulate_mouse_move(position, None, modifiers);
        cx.run_until_parked();
        assert_definition_highlight(&mut cx);
        assert_eq!(
            *requests.lock().expect("requests"),
            [lsp::Position::new(1, 9)]
        );
        assert!(active_picker(&mut cx).is_none());

        cx.simulate_click(position, modifiers);
        cx.run_until_parked();

        assert_eq!(
            *requests.lock().expect("requests"),
            [lsp::Position::new(1, 9)]
        );
        assert_picker_locations(
            &mut cx,
            if kind == GotoDefinitionKind::Type {
                LspPickerKind::TypeDefinition
            } else {
                LspPickerKind::Definition
            },
            &[(0, 3, 7), (2, 14, 17)],
        );
        assert_hover_cleared(&mut cx);
    }

    fn assert_definition_highlight(cx: &mut EditorLspTestContext) {
        cx.assert_editor_text_highlights(
            HighlightKey::HoveredLinkState,
            indoc! {r#"
            fn main() {
                let «abc» = 123;
                let xyz = abc;
            }
        "#},
        );
    }

    fn assert_hover_cleared(cx: &mut EditorLspTestContext) {
        cx.editor(|editor, _, cx| {
            assert_eq!(
                editor
                    .text_highlights(HighlightKey::HoveredLinkState, cx)
                    .map(|(_, ranges)| ranges.len()),
                None
            );
        });
    }

    fn assert_picker_locations(
        cx: &mut EditorLspTestContext,
        kind: LspPickerKind,
        ranges: &[(u32, u32, u32)],
    ) {
        let modal = active_picker(cx).expect("locations picker");
        cx.update(|_, cx| {
            let delegate = &modal.read(cx).picker.read(cx).delegate;
            assert_eq!(delegate.kind, kind);
            let actual = delegate
                .all_matches
                .iter()
                .map(|location| {
                    let snapshot = location.buffer.read(cx).snapshot();
                    let start = snapshot.summary_for_anchor::<Point>(&location.anchor_range.start);
                    let end = snapshot.summary_for_anchor::<Point>(&location.anchor_range.end);
                    assert_eq!(snapshot.offset_to_point(location.range.start), start);
                    assert_eq!(snapshot.offset_to_point(location.range.end), end);
                    (location.path.path.as_unix_str().to_string(), start..end)
                })
                .collect::<Vec<_>>();
            let expected = ranges
                .iter()
                .map(|&(row, start, end)| {
                    (
                        "dir/file.rs".to_string(),
                        Point::new(row, start)..Point::new(row, end),
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(actual, expected);
        });
    }
}
