//! The hosts panel (`Space h`): its state, its drawing and its keys.
//!
//! One file per modal, the shape `prefs.rs` already has. The panel used to be
//! spread across four — state in `mod.rs`, the four `draw_host_*` in `draw.rs`,
//! two handlers in `keys.rs`, persistence in `hosts.rs` — which meant reading it
//! end to end meant holding all of them open. Persistence stays where it is:
//! [`super::hosts`] is the `hosts.json` model, shared with the backend
//! reconcile, and is not part of this popup.
//!
//! The panel has **no Save step** (§9): every mutation persists as it happens,
//! so `Esc` has to be a real cancel — which is what [`RowEdit`] exists to make
//! possible, and why it carries a snapshot rather than a flag.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};

use crate::backend::{ConnState, VitalsView};
use crate::config;
use crate::state::HostId;

use super::draw::{one_line, vitals_spinner_glyph, wrap_ranges};
use super::format::{ICON_SLOT_WIDTH, centered_rect, clear_overlay, hint_pair};
use super::picker::{TextInput, TextInputEvent};
use super::{Action, App};
use super::{hosts, picker};

/// Active hosts popup (`input_mode == InputMode::HostEdit`). List and details
/// share a selected row. Only a row editor carries uncommitted changes; Enter
/// applies that draft and Escape restores its snapshot.
#[derive(Debug)]
pub(crate) struct HostEditState {
    pub(in crate::app) rows: Vec<HostRow>,
    pub(in crate::app) message: Option<String>,
    /// Selected row (`0..rows.len()`), or `rows.len()` for the "+ add" line.
    pub(in crate::app) cursor: usize,
    pub(in crate::app) view: HostView,
    pub(in crate::app) detail_scroll: usize,
    pub(in crate::app) detail_rows: usize,
    /// `Some` while the selected row's fields have the keyboard — see
    /// [`RowEdit`]. `None` in the list. Drawn as a card over the list rather
    /// than inside it, so this is what dims the panel behind it too.
    pub(in crate::app) edit: Option<RowEdit>,
    /// The row a `d` press is asking about — the removal confirm (§9). `None`
    /// when nothing is pending.
    pub(in crate::app) pending_remove: Option<usize>,
    /// What a `u` press put on screen — a question to answer, or a refusal to
    /// acknowledge. Kept beside `pending_remove` rather than folded into the
    /// global [`PendingConfirm`] because that one switches `InputMode`, which
    /// would tear this panel down mid-question.
    pub(in crate::app) pending_upgrade: Option<UpgradePrompt>,
    /// The connection log open over the list (`l`). `Some` replaces the list
    /// view entirely — it wants the whole popup, since the text it exists to
    /// show is what didn't fit on a row.
    pub(in crate::app) log_view: Option<HostLogView>,
    pub(in crate::app) forward_view: Option<super::port_forwards::ForwardView>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::app) enum HostView {
    #[default]
    List,
    Details,
    Help {
        from_details: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostSection {
    Connection,
    Codex,
    Services,
}

impl HostSection {
    const ALL: [Self; 3] = [Self::Connection, Self::Codex, Self::Services];

    fn label(self) -> &'static str {
        match self {
            Self::Connection => "Connection",
            Self::Codex => "Codex",
            Self::Services => "Services",
        }
    }
}

/// The hosts panel's row editor: which field has the keyboard, and what `Esc`
/// puts back.
///
/// One `Option` rather than an `editing` flag beside a focus and a snapshot: an
/// entry point that set two of the three and forgot the third would compile,
/// and the one it would forget is the snapshot — which is the difference
/// between `Esc` restoring a mistyped target and losing the old one. There are
/// three entry points (`a`, `e`/`Enter`, and the `^`-key that opens the editor
/// on a named field), so that is a live risk rather than a hypothetical one.
#[derive(Debug)]
pub(in crate::app) struct RowEdit {
    pub(in crate::app) focus: HostField,
    pub(in crate::app) origin: EditOrigin,
}

/// What `Esc` undoes in the hosts panel's row editor.
///
/// The panel has no Save step — a commit persists immediately (§9) — so its
/// counterpart has to be a real cancel, and a cancel needs the pre-edit
/// contents from somewhere. A row the edit *created* has none: abandoning it
/// removes it again, which is also what stops a half-typed `(unnamed)` row from
/// lingering in the list until the panel is reopened.
#[derive(Debug)]
pub(in crate::app) enum EditOrigin {
    Existing(Box<HostRow>),
    Added,
}

impl HostEditState {
    /// Only committed, usable rows participate in the persisted order.
    pub(in crate::app) fn host_order(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|r| r.is_local || r.config().is_some())
            .map(|r| r.host().0)
            .collect()
    }

    /// Start editing the selected row on `focus`, recording what `Esc` restores.
    pub(in crate::app) fn begin_edit(&mut self, focus: HostField) {
        let Some(row) = self.rows.get(self.cursor) else {
            return;
        };
        let focus = if row.is_local {
            HostField::CodexMode
        } else {
            focus
        };
        self.edit = Some(RowEdit {
            focus,
            origin: EditOrigin::Existing(Box::new(row.clone())),
        });
    }

    /// Append a blank row and edit it from the Label field. `Esc` removes it
    /// again — an empty row is not a host, and never became one on disk
    /// ([`App::apply_host_edits`] filters it), so leaving it in the list would
    /// only be a lie about what is configured.
    pub(in crate::app) fn begin_new_row(&mut self) {
        self.rows.push(HostRow::default());
        self.cursor = self.rows.len() - 1;
        self.edit = Some(RowEdit {
            focus: HostField::Label,
            origin: EditOrigin::Added,
        });
    }

    /// Abandon the edit in progress, restoring what was there before it.
    /// Persists nothing: no mutation reaches disk between `begin_edit` and the
    /// commit, so putting the row back is the whole of the undo.
    pub(in crate::app) fn cancel_edit(&mut self) {
        let Some(edit) = self.edit.take() else {
            return;
        };
        match edit.origin {
            EditOrigin::Existing(row) => {
                if let Some(slot) = self.rows.get_mut(self.cursor) {
                    *slot = *row;
                }
            }
            EditOrigin::Added => {
                if self.cursor < self.rows.len() {
                    self.rows.remove(self.cursor);
                }
                self.cursor = self.cursor.min(self.rows.len());
            }
        }
    }

    /// The field with the keyboard, or `None` in the list.
    pub(in crate::app) fn focus(&self) -> Option<HostField> {
        self.edit.as_ref().map(|e| e.focus)
    }
}

/// The line a `u` press leaves in the hosts panel.
///
/// One type for both outcomes because they render identically and are dismissed
/// identically; only `actionable` decides whether `y` does anything. Keeping the
/// refusal on screen matters — this panel has no status line (its footer is key
/// hints), so a message set anywhere else would surface stale, after the panel
/// closed, or not at all.
#[derive(Debug)]
pub(crate) struct UpgradePrompt {
    pub(in crate::app) row: usize,
    pub(in crate::app) text: String,
    /// `false` for a refusal: any key dismisses it and nothing happens.
    pub(in crate::app) actionable: bool,
}

/// One rendered line of a host's connection log — see [`App::host_log_lines`].
#[derive(Debug, Clone)]
pub(crate) struct HostLogLine {
    /// How long ago the entry happened, on its **first** line only; `None` on
    /// the continuation lines of a multi-line entry.
    pub(crate) age: Option<String>,
    pub(crate) error: bool,
    pub(crate) text: String,
}

/// The hosts panel's connection-log view (`l`), scrolled over one host's
/// [`ConnLogEntry`](crate::backend::ConnLogEntry) list.
#[derive(Debug)]
pub(crate) struct HostLogView {
    pub(in crate::app) host: HostId,
    /// First visible line, counted in *physical* lines — a host's multi-line
    /// refusal scrolls like the paragraph it is, not as one indivisible entry.
    pub(in crate::app) scroll: usize,
    /// Content rows the last draw had. Recorded there because `G` and PageDown
    /// need a viewport height, and the popup's size is only known while
    /// rendering; 0 until the first frame, which just makes those keys no-ops
    /// for one frame.
    pub(in crate::app) rows: usize,
}

/// One editable host row in the popup.
///
/// The text fields are [`TextInput`](picker::TextInput)s rather than bare
/// `String`s. They hold ssh targets and argument lines long enough that fixing a
/// typo in the middle has to be possible, which needs a cursor — and the widget
/// that has one already backs every picker's query and the directory-mark
/// editor's icon field, so the readline keys are the same ones here.
#[derive(Debug, Clone, Default)]
pub(crate) struct HostRow {
    pub(in crate::app) is_local: bool,
    pub(in crate::app) codex: Option<cm_core::agents::codex::CodexConfig>,
    pub(in crate::app) codex_error: Option<String>,
    pub(in crate::app) codex_endpoint: picker::TextInput,
    pub(in crate::app) label: picker::TextInput,
    /// ssh target (`user@host`) or, when `is_socket`, a socket path.
    pub(in crate::app) target: picker::TextInput,
    pub(in crate::app) is_socket: bool,
    /// Per-host emoji shown beside the workdir icon, picked with the same
    /// searchable picker as the workdir marks. Empty = derive one from the label.
    pub(in crate::app) icon: picker::TextInput,
    /// Suspended — see [`hosts::HostConfig::disabled`]. Toggled with `c`.
    pub(in crate::app) disabled: bool,
    /// Advanced SSH arguments, parsed without shell expansion.
    pub(in crate::app) options: picker::TextInput,
    pub(in crate::app) shell_command: picker::TextInput,
    pub(in crate::app) forwards: Vec<crate::ssh_forward::Rule>,
    /// Offer this host the clipboard — see [`hosts::HostConfig::clipboard`].
    /// A form field, toggled with `Space`: the panel's plain letters are for
    /// things you do *to* a row (connect, delete, upgrade), and this is part of
    /// what a host **is**, like its options. Being a field also means it shows its
    /// own state — `[off]` is visible in Services, where a list key was only
    /// discoverable from the footer.
    pub(in crate::app) clipboard: bool,
    pub(in crate::app) forward_agent: bool,
}

impl HostRow {
    /// Localhost is synthetic; incomplete rows and the reserved local label
    /// never become remote connection records.
    pub(in crate::app) fn config(&self) -> Option<hosts::HostConfig> {
        let label = self.label.text().trim();
        let target = self.target.text().trim();
        if self.is_local
            || label.is_empty()
            || target.is_empty()
            || label.eq_ignore_ascii_case("local")
        {
            return None;
        }
        let icon = self.icon.text().trim();
        let mut config = hosts::HostConfig {
            label: label.to_string(),
            icon: (!icon.is_empty()).then(|| icon.to_string()),
            socket: self.is_socket.then(|| target.to_string()),
            ssh: (!self.is_socket).then(|| target.to_string()),
            disabled: self.disabled,
            clipboard: self.clipboard,
            forward_agent: self.forward_agent,
            options: hosts::split_options(self.options.text()),
            shell_command: (!self.shell_command.text().trim().is_empty())
                .then(|| self.shell_command.text().to_string()),
            forwards: self.forwards.clone(),
        };
        config.migrate_forwards();
        Some(config)
    }

    /// The `HostId` this row configures — its label, trimmed exactly as
    /// [`App::apply_host_edits`] trims it on the way to disk, so a lookup
    /// against the live backends matches a row still being typed.
    pub(in crate::app) fn host(&self) -> HostId {
        if self.is_local {
            HostId::local()
        } else {
            HostId(self.label.text().trim().to_string())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
/// One editable field of a host's row in the hosts panel. The order here is the
/// order Tab walks them in.
pub(crate) enum HostField {
    Label,
    Target,
    Options,
    Icon,
    /// A toggle — see [`HostRow::clipboard`].
    Clipboard,
    SshAgent,
    CodexMode,
    CodexEndpoint,
    Forwards,
    ShellCommand,
}

impl HostField {
    /// Form order. Tab walks all supported fields in section order, switching
    /// tabs as needed; each tab renders only its own fields.
    const ORDER: [HostField; 10] = [
        HostField::Label,
        HostField::Target,
        HostField::Options,
        HostField::Icon,
        HostField::CodexMode,
        HostField::CodexEndpoint,
        HostField::Clipboard,
        HostField::SshAgent,
        HostField::Forwards,
        HostField::ShellCommand,
    ];

    fn section(self) -> HostSection {
        match self {
            Self::Label | Self::Target | Self::Options | Self::Icon => HostSection::Connection,
            Self::CodexMode | Self::CodexEndpoint => HostSection::Codex,
            Self::Clipboard | Self::SshAgent | Self::Forwards | Self::ShellCommand => {
                HostSection::Services
            }
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Label => "Label",
            Self::Target => "Target",
            Self::Options => "Advanced SSH options",
            Self::ShellCommand => "Work tab command",
            Self::Forwards => "Port forwards",
            Self::Icon => "Icon",
            Self::Clipboard => "Clipboard",
            Self::SshAgent => "SSH agent",
            Self::CodexMode => "Codex connection",
            Self::CodexEndpoint => "Codex endpoint",
        }
    }

    fn visible_for(self, row: &HostRow) -> bool {
        match self {
            Self::Forwards => !row.is_local && !row.is_socket && row.config().is_some(),
            Self::Options | Self::ShellCommand | Self::SshAgent => !row.is_local && !row.is_socket,
            Self::CodexMode => row.codex.is_some(),
            Self::CodexEndpoint => row
                .codex
                .as_ref()
                .is_some_and(|config| !config.mode.is_native()),
            _ => !row.is_local,
        }
    }

    /// The next field, forwards or back. Wraps: the form is a ring, so
    /// overshooting the last field costs one more press either way.
    pub(in crate::app) fn step(self, forward: bool) -> Self {
        let n = Self::ORDER.len();
        let i = Self::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        let next = if forward { i + 1 } else { i + n - 1 };
        Self::ORDER[next % n]
    }
}

// =============================================================================
// Drawing
// =============================================================================

fn utilisation_style(percent: f32, ui: &config::UiColors) -> Style {
    if percent >= 90.0 {
        Style::default().fg(ui.error_fg).bold()
    } else if percent >= 80.0 {
        Style::default().fg(ui.attention_fg).bold()
    } else {
        Style::default().dim()
    }
}

impl App {
    /// The list contains only comparable values. Narrative and configuration
    /// belong to the details view, so a long target cannot crowd out readings.
    fn host_list_values(&self, row: &HostRow, narrow: bool) -> [Span<'static>; 6] {
        let cfg = config::get();
        let ui = &cfg.colors.ui;
        let dim = Style::default().dim();
        let missing = || Span::styled("—", dim);
        let backend = self.backend_for(&row.host()).filter(|_| !row.disabled);
        let connection = backend.map(|b| b.conn_state());
        let (label, style) = if row.disabled {
            (if narrow { "paused" } else { "suspended" }, dim)
        } else {
            match connection {
                Some(ConnState::Connected) => (
                    if narrow { "up" } else { "connected" },
                    Style::default().fg(Color::Green),
                ),
                Some(ConnState::Connecting) => (if narrow { "dialing" } else { "connecting" }, dim),
                Some(ConnState::Failed(_)) => ("failed", Style::default().fg(ui.error_fg)),
                _ => (if narrow { "down" } else { "offline" }, dim),
            }
        };
        let mut values = [
            Span::styled(label, style),
            missing(),
            missing(),
            missing(),
            missing(),
            missing(),
        ];
        let Some(backend) = backend.filter(|b| b.conn_state().is_connected()) else {
            return values;
        };
        values[1] = Span::raw(self.host_session_counts(&row.host()).0.to_string());
        match backend.vitals() {
            Some(VitalsView::Reading(vitals)) => {
                for (index, percent) in [
                    vitals.cpu_percent,
                    vitals.mem_percent(),
                    vitals.disk_percent(),
                ]
                .into_iter()
                .enumerate()
                {
                    values[index + 2] = match percent.filter(|value| value.is_finite()) {
                        Some(value) => {
                            Span::styled(format!("{value:.0}%"), utilisation_style(value, ui))
                        }
                        None => Span::styled("n/a", dim),
                    };
                }
                if let Some(rtt) = backend.latency() {
                    values[5] = Span::raw(format!("{} ms", rtt.as_millis()));
                }
            }
            Some(VitalsView::Loading) => {
                for value in &mut values[2..] {
                    *value = Span::styled(vitals_spinner_glyph(), dim);
                }
            }
            Some(VitalsView::Unavailable) => {
                for value in &mut values[2..] {
                    *value = Span::styled("n/a", Style::default().fg(ui.attention_fg));
                }
            }
            None => {}
        }
        if row.is_local {
            values[5] = Span::styled("local", dim);
        }
        values
    }

    fn hosts_popup(area: Rect) -> Rect {
        let width = area.width.saturating_sub(6).min(72);
        let height = area.height.saturating_sub(2).min(18);
        Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        )
    }

    /// The hosts popup: the host list, or — while `l` is open — one host's
    /// connection log in its place. The row editor is a card over the list
    /// ([`Self::draw_host_form`]), so both are drawn.
    pub(super) fn draw_host_edit(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(state) = self.host_edit.as_ref() else {
            return;
        };
        if state.forward_view.is_some() {
            self.draw_port_forwards(frame, area);
        } else if state.log_view.is_some() {
            self.draw_host_log(frame, area);
        } else {
            match state.view {
                HostView::List => self.draw_host_list(frame, area),
                HostView::Details => self.draw_host_details(frame, area),
                HostView::Help { .. } => self.draw_host_help(frame, area),
            }
            self.draw_host_form(frame, area);
            self.draw_host_prompt(frame, area);
        }
    }

    /// One host's connection narrative, oldest first — everything the panel row
    /// had to cut, plus the steps that led to it.
    ///
    /// Takes `&mut self` only to record the viewport height, which `G` and the
    /// page keys need and which nothing but a render knows.
    pub(super) fn draw_host_log(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(view) = self.host_edit.as_ref().and_then(|s| s.log_view.as_ref()) else {
            return;
        };
        let host = view.host.clone();
        let scroll = view.scroll;
        // Wider and taller than the list: these lines are quoted host output,
        // and wrapping a loader error at 72 cells helps nobody.
        let popup = centered_rect(88, 76, area);
        clear_overlay(frame, popup);
        let block = Block::default().borders(Borders::ALL).title(Span::styled(
            format!(" {host} \u{00b7} connection log ", host = host.0),
            Style::default().bold(),
        ));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        let lines = self.host_log_lines(&host);
        let rows = inner.height as usize;
        let cfg = config::get();
        let ui = &cfg.colors.ui;
        let rendered: Vec<Line> = if lines.is_empty() {
            vec![Line::from(Span::styled(
                // Two ways to get here, and they aren't the same thing.
                if self.backend_for(&host).is_some() {
                    "(nothing logged yet)"
                } else {
                    "(this host isn't connected — add or edit it first)"
                },
                Style::default().add_modifier(Modifier::DIM),
            ))]
        } else {
            lines
                .iter()
                .skip(scroll)
                .take(rows)
                .map(|l| {
                    // The age column is fixed-width so the text starts on one
                    // margin; a continuation line pays the same indent and so
                    // reads as part of the entry above it.
                    let age = Span::styled(
                        format!("{:>5} ", l.age.as_deref().unwrap_or("")),
                        Style::default().add_modifier(Modifier::DIM),
                    );
                    let style = if l.error {
                        Style::default().fg(ui.attention_fg)
                    } else {
                        Style::default()
                    };
                    Line::from(vec![age, Span::styled(l.text.clone(), style)])
                })
                .collect()
        };
        frame.render_widget(Paragraph::new(rendered), inner);

        // Record what the keys need, and re-clamp: the log grows underneath a
        // parked scroll offset, and the popup resizes with the terminal.
        if let Some(view) = self.host_edit.as_mut().and_then(|s| s.log_view.as_mut()) {
            view.rows = rows;
            view.scroll = view.scroll.min(lines.len().saturating_sub(rows));
        }
    }

    pub(super) fn draw_host_list(&self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(state) = self.host_edit.as_ref() else {
            return;
        };
        let popup = Self::hosts_popup(area);
        clear_overlay(frame, popup);
        frame.render_widget(
            Block::default().borders(Borders::ALL).title(" Hosts "),
            popup,
        );
        if popup.width < 10 || popup.height < 7 {
            return;
        }
        let narrow = popup.width < 64;
        // Offsets are terminal cells, including each column's reserved gap.
        // Numeric columns share their right edge with the matching heading.
        let (name, name_width, state_x, ends) = if narrow {
            (4, 7, 13, [22, 28, 34, 40, 49])
        } else {
            (4, 10, 17, [32, 39, 46, 53, 66])
        };
        let cfg = config::get();
        let ui = &cfg.colors.ui;
        let dim = Style::default().dim();
        let left = |frame: &mut ratatui::Frame, offset: u16, y, width, span: Span<'static>| {
            let available = popup.width.saturating_sub(offset + 1).min(width);
            frame.render_widget(
                Paragraph::new(span),
                Rect::new(popup.x + offset, y, available, 1),
            );
        };
        let right = |frame: &mut ratatui::Frame, end: u16, y, width: u16, span: Span<'static>| {
            let start = end.saturating_add(1).saturating_sub(width);
            let available = popup.width.saturating_sub(start + 1).min(width);
            frame.render_widget(
                Paragraph::new(span).alignment(Alignment::Right),
                Rect::new(popup.x + start, y, available, 1),
            );
        };
        let header_y = popup.y + 2;
        left(frame, name, header_y, name_width, Span::styled("HOST", dim));
        left(
            frame,
            state_x,
            header_y,
            if narrow { 6 } else { 10 },
            Span::styled("STATE", dim),
        );
        for ((end, width), label) in ends.into_iter().zip([4, 4, 4, 4, 7]).zip([
            if narrow { "SES" } else { "SESS" },
            "CPU",
            "MEM",
            "DISK",
            "LATENCY",
        ]) {
            right(frame, end, header_y, width, Span::styled(label, dim));
        }
        let visible = popup
            .height
            .saturating_sub(if state.message.is_some() { 8 } else { 6 })
            as usize;
        if visible == 0 {
            return;
        }
        let scroll = state.cursor.saturating_sub(visible - 1);
        for index in scroll..(scroll + visible).min(state.rows.len() + 1) {
            let y = popup.y + 4 + (index - scroll) as u16;
            let selected = index == state.cursor;
            if selected {
                frame.buffer_mut().set_style(
                    Rect::new(popup.x + 1, y, popup.width - 2, 1),
                    Style::default().bg(ui.highlight_bg),
                );
                left(
                    frame,
                    2,
                    y,
                    2,
                    Span::styled(
                        ui.selection_symbol.clone(),
                        Style::default().fg(ui.selection_fg),
                    ),
                );
            }
            let Some(row) = state.rows.get(index) else {
                left(
                    frame,
                    name,
                    y,
                    popup.width.saturating_sub(name + 1),
                    Span::styled("+ add host", dim),
                );
                continue;
            };
            let label = if row.is_local {
                "local"
            } else if row.label.text().trim().is_empty() {
                "(unnamed)"
            } else {
                row.label.text()
            };
            let style = if row.disabled {
                dim
            } else {
                Style::default().fg(ui.title_fg)
            };
            left(
                frame,
                name,
                y,
                name_width,
                Span::styled(one_line(label, name_width as usize), style),
            );
            let [connection, sessions, cpu, mem, disk, latency] =
                self.host_list_values(row, narrow);
            left(frame, state_x, y, if narrow { 6 } else { 10 }, connection);
            for ((end, width), value) in ends
                .into_iter()
                .zip([4, 4, 4, 4, 7])
                .zip([sessions, cpu, mem, disk, latency])
            {
                right(frame, end, y, width, value);
            }
        }
        if let Some(message) = &state.message {
            left(
                frame,
                3,
                popup.y + popup.height - 3,
                popup.width.saturating_sub(6),
                Span::styled(
                    one_line(message, popup.width.saturating_sub(6) as usize),
                    Style::default().fg(ui.attention_fg),
                ),
            );
        }
    }

    fn draw_host_details(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(state) = self.host_edit.as_ref() else {
            return;
        };
        let Some(row) = state.rows.get(state.cursor) else {
            return;
        };
        let mut popup = Self::hosts_popup(area);
        popup.height = area.height.saturating_sub(2).min(24);
        popup.y = area.y + (area.height - popup.height) / 2;
        clear_overlay(frame, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .padding(Padding::horizontal(2))
            .title(format!(
                " {} · details ",
                one_line(row.label.text(), popup.width.saturating_sub(16) as usize)
            ));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let width = inner.width as usize;
        let cfg = config::get();
        let ui = &cfg.colors.ui;
        let dim = Style::default().dim();
        let backend = self.backend_for(&row.host()).filter(|_| !row.disabled);
        let connection = backend.map(|b| b.conn_state());
        let mut lines = Vec::new();
        let append = |lines: &mut Vec<Line>, text: String, style| {
            let text = crate::backend::host_text_safe(&text);
            for physical_line in text.split('\n') {
                for range in wrap_ranges(physical_line, width) {
                    lines.push(Line::from(Span::styled(
                        physical_line[range].to_owned(),
                        style,
                    )));
                }
            }
        };
        let (label, style) = match &connection {
            _ if row.disabled => ("Suspended by you", dim),
            Some(ConnState::Connected) => ("Connected", Style::default().fg(Color::Green)),
            Some(ConnState::Connecting) => ("Connecting", dim),
            Some(ConnState::Failed(_)) => ("Connection failed", Style::default().fg(ui.error_fg)),
            _ => ("Not connected", dim),
        };
        append(&mut lines, label.into(), style);
        if let Some(state @ ConnState::Failed(_)) = &connection {
            append(
                &mut lines,
                state.label().to_owned(),
                Style::default().fg(ui.error_fg),
            );
        }
        if connection.as_ref().is_some_and(ConnState::is_connected) {
            let (running, attached) = self.host_session_counts(&row.host());
            append(
                &mut lines,
                format!(
                    "{running} {} / {attached} attached",
                    super::plural_sessions(running)
                ),
                Style::default(),
            );
        }
        append(&mut lines, String::new(), dim);
        append(&mut lines, "Connection".into(), dim);
        append(
            &mut lines,
            if row.is_local {
                "this machine".into()
            } else {
                format!(
                    "{} {}",
                    if row.is_socket { "socket" } else { "ssh" },
                    row.target.text()
                )
            },
            Style::default(),
        );
        if !row.options.text().trim().is_empty() {
            append(&mut lines, row.options.text().to_owned(), dim);
        }
        if let Some(backend) = backend {
            if let Some(version) = backend.daemon_version() {
                let mut server = format!("Server v{version}");
                match backend.upgrade_offer() {
                    Some(offer) => server.push_str(&format!(" → v{} (u upgrade)", offer.version)),
                    None if super::format::version_is_older(
                        &version,
                        env!("CARGO_PKG_VERSION"),
                    ) =>
                    {
                        server.push_str(" (older than ours)")
                    }
                    None => {}
                }
                append(&mut lines, server, dim);
            }
            append(&mut lines, String::new(), dim);
            append(&mut lines, "Resources".into(), dim);
            let values = self.host_list_values(row, false);
            lines.push(Line::from(vec![
                Span::styled("CPU ", dim),
                values[2].clone(),
                Span::styled("   Mem ", dim),
                values[3].clone(),
                Span::styled("   Disk ", dim),
                values[4].clone(),
            ]));
            append(&mut lines, format!("Latency {}", values[5].content), dim);
        }
        append(&mut lines, String::new(), dim);
        append(&mut lines, "Codex".into(), dim);
        append(
            &mut lines,
            row.codex
                .as_ref()
                .map(|c| c.mode.label().to_owned())
                .unwrap_or_else(|| "Unavailable".into()),
            Style::default(),
        );
        if HostField::CodexEndpoint.visible_for(row) {
            append(&mut lines, row.codex_endpoint.text().to_owned(), dim);
        }
        if let Some(error) = &row.codex_error {
            append(
                &mut lines,
                error.clone(),
                Style::default().fg(ui.attention_fg),
            );
        }
        if !row.is_local {
            append(&mut lines, String::new(), dim);
            append(&mut lines, "Services".into(), dim);
            append(
                &mut lines,
                format!(
                    "Clipboard {} / {} enabled port forwards",
                    if row.clipboard { "on" } else { "off" },
                    row.forwards.iter().filter(|f| !f.disabled).count()
                ),
                Style::default(),
            );
            if !row.is_socket {
                append(
                    &mut lines,
                    format!("SSH agent {}", if row.forward_agent { "on" } else { "off" }),
                    Style::default(),
                );
            }
            if !row.shell_command.text().trim().is_empty() {
                append(&mut lines, row.shell_command.text().to_owned(), dim);
            }
        }
        if let Some(message) = &state.message {
            append(&mut lines, String::new(), dim);
            append(
                &mut lines,
                message.clone(),
                Style::default().fg(ui.attention_fg),
            );
        }
        let rows = inner.height as usize;
        let scroll = if state.message.is_some() {
            lines.len().saturating_sub(rows)
        } else {
            state.detail_scroll.min(lines.len().saturating_sub(rows))
        };
        frame.render_widget(
            Paragraph::new(lines).scroll((scroll.min(u16::MAX as usize) as u16, 0)),
            inner,
        );
        if let Some(state) = self.host_edit.as_mut() {
            state.detail_rows = rows;
            state.detail_scroll = scroll;
        }
    }

    fn draw_host_help(&self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(state) = self.host_edit.as_ref() else {
            return;
        };
        let popup = Self::hosts_popup(area);
        clear_overlay(frame, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .padding(Padding::horizontal(2))
            .title(" Host commands ");
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let mut commands = vec![
            ("j/k", "Select a host; scroll details"),
            ("Enter", "Open details / add host"),
            ("e / a", "Edit / add host"),
            ("J / K", "Move down / up; first host is default"),
        ];
        if self.selected_host_has_log() {
            commands.push(("l", "Connection log"));
        }
        if self.selected_host_has_forwards() {
            commands.push(("f", "Port forwards"));
        }
        if state.rows.get(state.cursor).is_some_and(|r| !r.is_local) {
            commands.extend([
                ("c", "Suspend / reconnect"),
                ("d", "Remove host (asks first)"),
                ("Ctrl+t/e", "Edit target / pick icon"),
            ]);
        }
        if self.selected_host_upgrade().is_some() {
            commands.push(("u", "Upgrade server (asks first)"));
        }
        commands.push(("Esc", "Back / cancel"));
        let lines: Vec<_> = commands
            .into_iter()
            .map(|(key, text)| {
                Line::from(vec![
                    Span::styled(
                        format!("{key:<10}"),
                        Style::default().fg(config::get().colors.ui.title_fg),
                    ),
                    Span::raw(one_line(text, inner.width.saturating_sub(10) as usize)),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_host_prompt(&self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(state) = self.host_edit.as_ref() else {
            return;
        };
        let (title, text) = if let Some(index) = state.pending_remove {
            let label = state.rows.get(index).map(|r| r.label.text()).unwrap_or("");
            (
                " Remove host? ",
                format!("Remove host \"{label}\" and its dashboard mirror? [y/N]"),
            )
        } else if let Some(prompt) = &state.pending_upgrade {
            (" Upgrade server ", prompt.text.clone())
        } else {
            return;
        };
        let host_popup = Self::hosts_popup(area);
        let width = host_popup.width.saturating_sub(4);
        let lines: Vec<_> = wrap_ranges(&text, width.saturating_sub(6) as usize)
            .into_iter()
            .map(|r| Line::from(text[r].to_owned()))
            .collect();
        let height = (lines.len() as u16 + 4).min(area.height);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        frame
            .buffer_mut()
            .set_style(host_popup, Style::default().dim());
        clear_overlay(frame, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .padding(Padding::horizontal(2))
            .title(title);
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().fg(config::get().colors.ui.attention_fg)),
            inner,
        );
    }

    /// The selected row's fields, as a card floating over the list.
    ///
    /// Its own popup rather than the form pinned under the list that this was.
    /// Two things were wrong with that: the list lost eight of its rows the
    /// moment you pressed `e` — on a short terminal most of it — and a form
    /// sharing a box with a list it does *not* share a cursor with reads as one
    /// more part of the same view, when in fact every key means something
    /// different while it is up. A card that covers the list, dims it and names
    /// itself says that in the shape of the thing.
    pub(super) fn draw_host_form(&self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(state) = self.host_edit.as_ref() else {
            return;
        };
        if let Some(focus) = state.focus()
            && let Some(r) = state.rows.get(state.cursor)
        {
            let host_popup = Self::hosts_popup(area);
            // Keep every tab the same width, inset two cells per side from
            // the parent panel. Long values wrap within that fixed cell grid.
            let width = host_popup.width.saturating_sub(4);
            // What a field's text has to fit in: the card's inner width — the
            // frame and its padding are two cells a side — less the fixed
            // columns ahead of the value, less one cell for the end-of-text
            // cursor, which needs somewhere to sit on an otherwise full line.
            let label_w = HostField::ORDER
                .iter()
                .filter(|field| field.visible_for(r) && field.section() == focus.section())
                .map(|field| field.label().len())
                .max()
                .unwrap_or(0)
                + 1;
            let value_col = 2 + label_w;
            let value_w = (width as usize).saturating_sub(4 + value_col + 1);
            // A field's rows: the mark and label on the first, continuation lines
            // indented to the value column so a value that wrapped still reads as
            // one field rather than as a nameless new one.
            let field_rows = |field: HostField, values: Vec<Vec<Span<'static>>>| {
                let focused = focus == field;
                let label = field.label();
                values
                    .into_iter()
                    .enumerate()
                    .map(|(i, value)| {
                        let mut spans = if i == 0 {
                            vec![
                                if focused {
                                    Span::styled("\u{276F} ", Style::default().bold())
                                } else {
                                    Span::raw("  ")
                                },
                                // Reserve a gap after the widest visible label.
                                Span::styled(
                                    format!("{label:<label_w$}"),
                                    Style::default().add_modifier(Modifier::DIM),
                                ),
                            ]
                        } else {
                            vec![Span::raw(" ".repeat(value_col))]
                        };
                        spans.extend(value);
                        Line::from(spans)
                    })
                    .collect::<Vec<_>>()
            };
            let tabs = Line::from(
                HostSection::ALL
                    .into_iter()
                    .enumerate()
                    .filter_map(|(i, section)| {
                        let visible = HostField::ORDER
                            .iter()
                            .any(|f| f.section() == section && f.visible_for(r))
                            || (r.is_local && section == HostSection::Codex);
                        visible.then(|| {
                            let active = section == focus.section();
                            Span::styled(
                                if active {
                                    format!("[{} {}]  ", i + 1, section.label())
                                } else {
                                    format!(" {} {}   ", i + 1, section.label())
                                },
                                if active {
                                    Style::default().fg(config::get().colors.ui.title_fg)
                                } else {
                                    Style::default().dim()
                                },
                            )
                        })
                    })
                    .collect::<Vec<_>>(),
            );
            let mut form_lines = Vec::new();
            for field in HostField::ORDER
                .into_iter()
                .filter(|field| field.section() == focus.section() && field.visible_for(r))
            {
                let focused = focus == field;
                let values = match field {
                    HostField::Label => text_field_lines(&r.label, focused, value_w),
                    HostField::Target => {
                        let prefix = format!("[{}] ", if r.is_socket { "socket" } else { "ssh" });
                        let mut lines = text_field_lines(
                            &r.target,
                            focused,
                            value_w.saturating_sub(prefix.len()),
                        );
                        lines[0].insert(0, Span::styled(prefix, Style::default().dim()));
                        lines
                    }
                    HostField::Options => text_field_lines(&r.options, focused, value_w),
                    HostField::Icon => {
                        let mut lines = text_field_lines(&r.icon, focused, value_w);
                        if r.icon.text().trim().is_empty()
                            && let Some(last) = lines.last_mut()
                        {
                            last.push(Span::styled(
                                format!("{} (auto)", self.host_icon(&r.host())),
                                Style::default().dim(),
                            ));
                        }
                        lines
                    }
                    HostField::Clipboard => {
                        vec![vec![Span::raw(if r.clipboard { "[on]" } else { "[off]" })]]
                    }
                    HostField::SshAgent => {
                        vec![vec![Span::raw(if r.forward_agent {
                            "[on]"
                        } else {
                            "[off]"
                        })]]
                    }
                    HostField::CodexMode => vec![vec![Span::raw(format!(
                        "[{}]",
                        r.codex.as_ref().unwrap().mode.label()
                    ))]],
                    HostField::CodexEndpoint => {
                        text_field_lines(&r.codex_endpoint, focused, value_w)
                    }
                    HostField::Forwards => vec![vec![Span::raw(format!(
                        "{} enabled · {} total  [manage]",
                        r.forwards.iter().filter(|f| !f.disabled).count(),
                        r.forwards.len()
                    ))]],
                    HostField::ShellCommand => {
                        let mut lines = text_field_lines(&r.shell_command, focused, value_w);
                        if r.shell_command.text().trim().is_empty()
                            && let Some(last) = lines.last_mut()
                        {
                            last.push(Span::styled("(default shell)", Style::default().dim()));
                        }
                        lines
                    }
                };
                form_lines.extend(field_rows(field, values));
            }
            if r.is_local && r.codex.is_none() {
                form_lines.push(Line::from(
                    r.codex_error
                        .clone()
                        .unwrap_or_else(|| "Loading Codex settings…".into()),
                ));
            }
            // The field rows — one per field until a value wraps — a blank, the
            // hint line (held whether this field has a hint or not, for the same
            // reason the width is), and the two borders. The card grows down as a
            // value wraps rather than the value being cut off at the frame: this
            // is text being *edited*, and what you cannot see you cannot tell you
            // typed twice.
            let height = (form_lines.len() as u16 + 6).min(host_popup.height);
            // A terminal too small to draw a frame around anything. The dashboard
            // as a whole is unusable well before this, so it is a guard against a
            // degenerate `Rect`, not a layout for a narrow screen.
            if width < 8 || height < 5 {
                return;
            }
            let popup = Rect {
                x: host_popup.x + (host_popup.width - width) / 2,
                y: host_popup.y + (host_popup.height - height) / 2,
                width,
                height,
            };
            // The list goes quiet under the card. It is modal — every key belongs
            // to the form while it is up, including the `j`/`k`/`d` that move and
            // delete rows out there — and dimming what stopped listening is the
            // same cue the preview pane uses when it is no longer live.
            frame
                .buffer_mut()
                .set_style(host_popup, Style::default().add_modifier(Modifier::DIM));
            clear_overlay(frame, popup);
            // Which of the two things Esc will do: put a row back, or drop one
            // that was never on disk. The old inline form couldn't say.
            let title = match state.edit.as_ref().map(|e| &e.origin) {
                Some(EditOrigin::Added) => " Add Host ".to_owned(),
                _ => format!(
                    " Edit Host · {} ",
                    one_line(r.label.text(), width.saturating_sub(18) as usize)
                ),
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .padding(Padding::horizontal(1))
                .title(Span::styled(title, Style::default().bold()));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);
            frame.render_widget(
                Paragraph::new(tabs),
                Rect::new(inner.x, inner.y, inner.width, 1),
            );
            let fields_area = Rect::new(
                inner.x,
                inner.y + 2,
                inner.width,
                inner.height.saturating_sub(2),
            );

            // The blank goes in whether or not this field has a hint, so the one
            // line the card reserves for it doesn't shunt the fields up and down.
            form_lines.push(Line::from(""));
            if let Some(hint) = state.message.as_deref().or_else(|| host_field_hint(focus)) {
                form_lines.push(Line::from(Span::styled(
                    hint,
                    Style::default().add_modifier(Modifier::DIM),
                )));
            }
            let focus_line = form_lines
                .iter()
                .rposition(|line| {
                    line.spans
                        .iter()
                        .any(|span| span.style.add_modifier.contains(Modifier::REVERSED))
                })
                .or_else(|| {
                    form_lines.iter().position(|line| {
                        line.spans
                            .first()
                            .is_some_and(|span| span.content.starts_with('❯'))
                    })
                })
                .unwrap_or(0);
            let scroll = focus_line.saturating_sub(fields_area.height.saturating_sub(2) as usize);
            frame.render_widget(
                Paragraph::new(form_lines).scroll((scroll.min(u16::MAX as usize) as u16, 0)),
                fields_area,
            );
        }
    }
}

// =============================================================================
// Keys
// =============================================================================

impl App {
    pub(super) fn host_hints(&self, width: u16) -> Vec<Span<'static>> {
        let Some(state) = &self.host_edit else {
            return Vec::new();
        };
        let pairs: Vec<(&str, &str)> = if state.forward_view.is_some() {
            return vec![Span::raw(self.forward_hints())];
        } else if state.log_view.is_some() {
            vec![("j/k", "scroll"), ("g/G", "top/bottom"), ("Esc", "back")]
        } else if state.pending_remove.is_some() {
            vec![("y", "remove"), ("Esc", "cancel")]
        } else if let Some(prompt) = &state.pending_upgrade {
            if prompt.actionable {
                vec![("y", "upgrade"), ("Esc", "cancel")]
            } else {
                vec![("Any key", "back")]
            }
        } else if matches!(state.view, HostView::Help { .. }) {
            vec![("Esc", "back")]
        } else if let Some(focus) = state.focus() {
            let mut pairs = vec![(if width < 80 { "Tab" } else { "Tab/↑↓" }, "field")];
            if !state.rows[state.cursor].is_local {
                pairs.push((
                    if state.rows[state.cursor].codex.is_some() {
                        "Alt+1/2/3"
                    } else {
                        "Alt+1/3"
                    },
                    "tab",
                ));
            }
            pairs.extend([
                (
                    "Enter",
                    if focus == HostField::Forwards {
                        "manage"
                    } else {
                        "apply"
                    },
                ),
                ("Esc", "cancel"),
            ]);
            pairs
        } else if state.view == HostView::Details {
            let mut pairs = vec![("e", "edit")];
            if self.selected_host_has_log() {
                pairs.push(("l", "log"));
            }
            if self.selected_host_has_forwards() {
                pairs.push(("f", "forwards"));
            }
            pairs.extend([("?", "keys"), ("Esc", "back")]);
            pairs
        } else if width < 80 {
            vec![
                ("Enter", "open"),
                ("J/K", "move"),
                ("e", "edit"),
                ("?", "keys"),
                ("Esc", "close"),
            ]
        } else {
            let mut pairs = vec![("j/k", "select"), ("Enter", "details")];
            if state.cursor < state.rows.len() && state.rows.len() > 1 {
                pairs.push(("J/K", "reorder"));
            }
            pairs.extend([("e", "edit"), ("?", "keys"), ("Esc", "close")]);
            pairs
        };
        pairs
            .into_iter()
            .flat_map(|(key, label)| hint_pair(key, label))
            .collect()
    }

    pub(super) fn selected_host_has_log(&self) -> bool {
        self.host_edit
            .as_ref()
            .and_then(|panel| panel.rows.get(panel.cursor))
            .is_some_and(|row| {
                !row.is_local
                    || self
                        .backend_for(&row.host())
                        .is_some_and(|backend| backend.capabilities().pooled)
            })
    }

    /// The hosts panel (`Space h`). A list view with live per-host state, not a
    /// staged edit form (§9): there is no Save step, because every mutation
    /// persists as it happens — adding a host connects it immediately (so you
    /// watch its state animate in the list), an edit applies when you commit the
    /// row, and a removal takes a `d`-then-`y` confirm.
    pub(super) fn handle_host_edit_key(&mut self, key: KeyEvent) -> Option<Action> {
        let has_log = self.selected_host_has_log();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if let HostView::Help { from_details } = self.host_edit.as_ref()?.view {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q' | '?')) {
                self.host_edit.as_mut()?.view = if from_details {
                    HostView::Details
                } else {
                    HostView::List
                };
            }
            return None;
        }
        if self.host_edit.as_ref()?.forward_view.is_some() {
            self.handle_port_forward_key(key);
            return None;
        }
        if (!ctrl
            && !alt
            && key.code == KeyCode::Char('f')
            && self.host_edit.as_ref()?.edit.is_none()
            && self.host_edit.as_ref()?.log_view.is_none()
            && self.host_edit.as_ref()?.pending_remove.is_none()
            && self.host_edit.as_ref()?.pending_upgrade.is_none())
            || (!ctrl
                && !alt
                && matches!(key.code, KeyCode::Enter | KeyCode::Char(' '))
                && self.host_edit.as_ref()?.focus() == Some(HostField::Forwards))
        {
            self.open_port_forwards();
            return None;
        }
        let editing = self.host_edit.as_ref()?.edit.is_some();

        // The log view owns the keyboard while it's open — it replaces the list,
        // so none of the list's keys are reachable behind it.
        if self.host_edit.as_ref()?.log_view.is_some() {
            self.handle_host_log_key(key);
            return None;
        }

        // A pending upgrade owns the keyboard until answered — or, when it is a
        // refusal rather than a question, until acknowledged.
        if let Some(prompt) = self.host_edit.as_mut()?.pending_upgrade.take() {
            if prompt.actionable && matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                let host = self.host_edit.as_ref()?.rows.get(prompt.row)?.host();
                return Some(Action::UpgradeHost { host });
            }
            return None;
        }

        // A pending removal owns the keyboard until answered.
        if let Some(idx) = self.host_edit.as_ref()?.pending_remove {
            let state = self.host_edit.as_mut()?;
            state.pending_remove = None;
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                if idx < state.rows.len() {
                    state.rows.remove(idx);
                    state.cursor = state.cursor.min(state.rows.len());
                }
                self.apply_host_edits();
                self.host_edit.as_mut()?.view = HostView::List;
            }
            return None;
        }

        // A settings result stays visible until the next interaction, then
        // scrolling follows the selected host again instead of the old message.
        self.host_edit.as_mut()?.message = None;

        // List-mode globals (in field-edit these are text / Esc-back).
        if !editing && matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            if self.host_edit.as_ref()?.view == HostView::Details {
                let state = self.host_edit.as_mut()?;
                state.view = HostView::List;
                state.detail_scroll = 0;
            } else {
                self.close_host_edit();
            }
            return None;
        }
        if !editing && !ctrl && !alt && key.code == KeyCode::Char('?') {
            let state = self.host_edit.as_mut()?;
            state.view = HostView::Help {
                from_details: state.view == HostView::Details,
            };
            return None;
        }

        // Ctrl-E opens the same searchable emoji picker the directory marks use
        // — one affordance, learned once. From the Icon field, and from the list
        // as the shortcut that opens the editor *on* that field: the picker was
        // otherwise five keys away from a row whose emoji you wanted to change.
        if ctrl && matches!(key.code, KeyCode::Char('e')) {
            let state = self.host_edit.as_mut()?;
            let opens_picker = match state.focus() {
                // In the Icon field the picker *is* the editor, so it shadows
                // readline's end-of-line — a field of at most four cells has
                // nowhere to jump to anyway.
                Some(HostField::Icon) => {
                    !state.rows.get(state.cursor).is_some_and(|row| row.is_local)
                }
                // In a text field ^e keeps that readline meaning and falls
                // through to the input below.
                Some(_) => false,
                // From the list, on a row: open the editor on Icon and go
                // straight where the key would have gone from there.
                None => {
                    let on_row =
                        state.cursor < state.rows.len() && !state.rows[state.cursor].is_local;
                    if on_row {
                        state.begin_edit(HostField::Icon);
                    }
                    on_row
                }
            };
            if opens_picker {
                self.open_emoji_picker_for_host();
                return None;
            }
        }

        let state = self.host_edit.as_mut()?;
        if let Some(focus) = state.focus() {
            if alt
                && !ctrl
                && let KeyCode::Char(digit @ '1'..='3') = key.code
            {
                let section = HostSection::ALL[(digit as u8 - b'1') as usize];
                if let Some(field) = HostField::ORDER.into_iter().find(|field| {
                    field.section() == section && field.visible_for(&state.rows[state.cursor])
                }) {
                    state.edit.as_mut()?.focus = field;
                }
                return None;
            }
            // Field focus, by all three idioms the dashboard already uses: Tab
            // walks the form, ↑↓ walk it as the vertical list it looks like, and
            // ^n/^p are what the pickers bind. Backwards matters as much as
            // forwards — a form you can only cycle one way makes overshooting
            // Options cost three more presses.
            let step = match key.code {
                KeyCode::Tab | KeyCode::Down => Some(true),
                KeyCode::BackTab | KeyCode::Up => Some(false),
                KeyCode::Char('n') if ctrl => Some(true),
                KeyCode::Char('p') if ctrl => Some(false),
                _ => None,
            };
            if let Some(forward) = step {
                if let Some(edit) = state.edit.as_mut() {
                    let row = &state.rows[state.cursor];
                    let mut next = focus.step(forward);
                    for _ in 0..HostField::ORDER.len() {
                        if next.visible_for(row) {
                            break;
                        }
                        next = next.step(forward);
                    }
                    edit.focus = next;
                }
                return None;
            }
            match key.code {
                // Committing a row applies it: persist + reconnect right away.
                KeyCode::Enter if !ctrl && !alt => {
                    let row = state.rows.get(state.cursor)?;
                    if let Err(error) = shell_words::split(row.options.text()) {
                        state.message = Some(format!("Invalid SSH options: {error}"));
                        return None;
                    }
                    let config = row.codex.clone().map(|mut config| {
                        config.endpoint = row.codex_endpoint.text().trim().to_owned();
                        config
                    });
                    let original = state.edit.as_ref().and_then(|edit| match &edit.origin {
                        EditOrigin::Existing(row) => row.codex.as_ref(),
                        EditOrigin::Added => None,
                    });
                    let action = config
                        .filter(|config| Some(config) != original)
                        .map(|config| Action::ConfigureCodex {
                            host: row.host(),
                            config,
                        });
                    state.edit = None;
                    self.apply_host_edits();
                    return action;
                }
                // And Esc abandons it — the snapshot the edit carries is what
                // makes that a real cancel rather than a second commit.
                KeyCode::Esc => {
                    state.cancel_edit();
                    return None;
                }
                KeyCode::Char('t') if ctrl && focus == HostField::Target => {
                    if let Some(r) = state.rows.get_mut(state.cursor) {
                        r.is_socket = !r.is_socket;
                    }
                    return None;
                }
                KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right
                    if focus == HostField::CodexMode && !ctrl && !alt =>
                {
                    if let Some(config) = state
                        .rows
                        .get_mut(state.cursor)
                        .and_then(|r| r.codex.as_mut())
                    {
                        use cm_core::agents::codex::CodexMode;
                        config.mode = if config.mode == CodexMode::Native {
                            CodexMode::AppServer
                        } else {
                            CodexMode::Native
                        };
                    }
                    return None;
                }
                // Toggle fields use Space or arrows; Enter commits the draft
                // consistently across the whole form.
                KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right
                    if matches!(focus, HostField::Clipboard | HostField::SshAgent)
                        && !ctrl
                        && !alt =>
                {
                    if let Some(r) = state.rows.get_mut(state.cursor) {
                        match focus {
                            HostField::SshAgent => r.forward_agent = !r.forward_agent,
                            _ => r.clipboard = !r.clipboard,
                        }
                    }
                    return None;
                }
                _ => {}
            }
            if ctrl || alt {
                // TextInput handles readline keys, but modified keys never
                // toggle a setting or commit a draft.
                if matches!(
                    focus,
                    HostField::Clipboard
                        | HostField::SshAgent
                        | HostField::CodexMode
                        | HostField::Forwards
                ) {
                    return None;
                }
            }
            // Everything else is text. The fields are `TextInput`s, so the
            // readline keys, the arrows and Home/End all come for free — and a
            // key none of them claim is simply dropped.
            let r = state.rows.get_mut(state.cursor)?;
            match focus {
                HostField::Label => {
                    r.label.handle_key(key);
                }
                HostField::Target => {
                    r.target.handle_key(key);
                }
                HostField::Options => {
                    r.options.handle_key(key);
                }
                HostField::ShellCommand => {
                    r.shell_command.handle_key(key);
                }
                // Capped like the directory-mark icon, and for the same reason
                // now that the two share one table column: past ~4 cells an
                // "icon" stops reading as a mark and just widens the column for
                // every row. Post-hoc revert rather than a pre-check, so paste
                // and multi-byte input still go through `TextInput` first.
                HostField::Icon => {
                    use unicode_width::UnicodeWidthStr;
                    let prev = r.icon.text().to_string();
                    if matches!(r.icon.handle_key(key), TextInputEvent::Changed)
                        && r.icon.text().width() > ICON_SLOT_WIDTH
                    {
                        r.icon.set_text(prev);
                    }
                }
                // Nothing to type into: its own keys are handled above, and a key
                // none of them claim is dropped rather than falling through to a
                // `TextInput` this field does not have.
                HostField::Clipboard
                | HostField::SshAgent
                | HostField::CodexMode
                | HostField::Forwards => {}
                HostField::CodexEndpoint => {
                    r.codex_endpoint.handle_key(key);
                }
            }
        } else {
            let n = state.rows.len();
            // A modified key never falls through to the plain-letter commands
            // below: a stray `^d` in the list must not reach the removal
            // confirm. What Ctrl *does* mean here is "open the editor on this
            // key's field" — `^e` above, `^t` here — plus the pickers' own
            // ^n/^p, which are the list's ↑↓ under another name.
            if ctrl || alt {
                if ctrl {
                    match key.code {
                        KeyCode::Char('n') if state.view == HostView::Details => {
                            state.detail_scroll += 1
                        }
                        KeyCode::Char('p') if state.view == HostView::Details => {
                            state.detail_scroll = state.detail_scroll.saturating_sub(1)
                        }
                        KeyCode::Char('n') => state.cursor = (state.cursor + 1).min(n),
                        KeyCode::Char('p') => state.cursor = state.cursor.saturating_sub(1),
                        KeyCode::Char('t') if state.cursor < n => {
                            state.begin_edit(HostField::Target)
                        }
                        _ => {}
                    }
                }
                return None;
            }
            if state.view == HostView::Details {
                let page = state.detail_rows.saturating_sub(1).max(1);
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        state.detail_scroll = state.detail_scroll.saturating_sub(1)
                    }
                    KeyCode::Down | KeyCode::Char('j') => state.detail_scroll += 1,
                    KeyCode::PageDown => state.detail_scroll += page,
                    KeyCode::PageUp => {
                        state.detail_scroll = state.detail_scroll.saturating_sub(page)
                    }
                    KeyCode::Home | KeyCode::Char('g') => state.detail_scroll = 0,
                    KeyCode::End | KeyCode::Char('G') => state.detail_scroll = usize::MAX,
                    _ => {}
                }
            }
            match key.code {
                KeyCode::Up | KeyCode::Char('k') if state.view == HostView::List => {
                    state.cursor = state.cursor.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') if state.view == HostView::List => {
                    state.cursor = (state.cursor + 1).min(n)
                }
                KeyCode::Char('J' | 'K') if state.view == HostView::List && state.cursor < n => {
                    let next = if key.code == KeyCode::Char('J') {
                        state.cursor + 1
                    } else {
                        state.cursor.wrapping_sub(1)
                    };
                    if next < n {
                        state.rows.swap(state.cursor, next);
                        state.cursor = next;
                        self.extra_prefs.host_order = Some(state.host_order());
                        // Ordering changes presentation and launch defaults only.
                        // Keep every existing backend and connection in place.
                        self.save_overrides();
                    }
                }
                KeyCode::Char('a') => {
                    state.view = HostView::List;
                    state.begin_new_row();
                }
                KeyCode::Enter if state.cursor < n => {
                    state.view = HostView::Details;
                    state.detail_scroll = 0;
                }
                KeyCode::Char('e') | KeyCode::Enter => {
                    if state.cursor == n {
                        state.begin_new_row();
                    } else {
                        state.begin_edit(HostField::Label);
                    }
                }
                // Suspend / resume the host. No confirm: unlike `d` it destroys
                // nothing — the row, its target and its icon all stay — and the
                // same key puts it straight back. No status line either: this
                // mode's footer renders key hints — and the row itself
                // answers immediately (dimmed, reading `suspended`, or
                // animating back through `connecting`).
                KeyCode::Char('c') if state.cursor < n && !state.rows[state.cursor].is_local => {
                    let row = &mut state.rows[state.cursor];
                    row.disabled = !row.disabled;
                    // Persists and rebuilds: `disabled` is part of what a backend
                    // is built from, so this drops (or dials) the connection now.
                    self.apply_host_edits();
                }
                // Removal is destructive (it drops the host and its mirror), so
                // it asks first.
                KeyCode::Char('d') if state.cursor < n && !state.rows[state.cursor].is_local => {
                    state.pending_remove = Some(state.cursor);
                }
                // Details and help expose upgrades only when the backend has
                // an offer. The prompt reports its cost or why it is blocked.
                KeyCode::Char('u') if state.cursor < n => {
                    let row = state.cursor;
                    let host = state.rows[row].host();
                    let offer = self.selected_host_upgrade()?;
                    let prompt = match self.upgrade_blocker(&host) {
                        Some(why) => super::UpgradePrompt {
                            row,
                            text: format!("  Cannot upgrade \"{}\": {why}", host.0),
                            actionable: false,
                        },
                        None => {
                            let n = self.host_session_counts(&host).0;
                            super::UpgradePrompt {
                                row,
                                text: format!(
                                    "  Upgrade \"{}\" to {}? {} [y/N]",
                                    host.0,
                                    offer.version,
                                    match n {
                                        0 => "The daemon restarts.".to_string(),
                                        n => format!(
                                            "{n} idle {} restart with it.",
                                            super::plural_sessions(n)
                                        ),
                                    }
                                ),
                                actionable: true,
                            }
                        }
                    };
                    self.host_edit.as_mut()?.pending_upgrade = Some(prompt);
                }
                // Details explain the failure; the log adds the steps before it.
                KeyCode::Char('l') if state.cursor < n && has_log => {
                    let host = state.rows[state.cursor].host();
                    state.log_view = Some(HostLogView {
                        host,
                        scroll: 0,
                        rows: 0,
                    });
                }
                _ => {}
            }
        }
        None
    }

    /// Scroll keys for the connection log (`l`). Reading, not editing, so the
    /// bindings are the pager ones: `j`/`k`, the arrows, page keys, `g`/`G`.
    ///
    /// Everything else is swallowed rather than falling through to the list
    /// underneath — the same rule the `Space` prefix follows, and for the same
    /// reason: a mistyped key here must not reach `d`.
    pub(super) fn handle_host_log_key(&mut self, key: KeyEvent) {
        let Some(view) = self.host_edit.as_mut().and_then(|s| s.log_view.as_mut()) else {
            return;
        };
        // The draw clamps against the live line count; a page is the viewport
        // minus one line of overlap, so you never step over a line unread.
        let page = view.rows.saturating_sub(1).max(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('l') => {
                if let Some(state) = self.host_edit.as_mut() {
                    state.log_view = None;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => view.scroll += 1,
            KeyCode::Up | KeyCode::Char('k') => view.scroll = view.scroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char('f') => view.scroll += page,
            KeyCode::PageUp | KeyCode::Char('b') => view.scroll = view.scroll.saturating_sub(page),
            KeyCode::Char('g') | KeyCode::Home => view.scroll = 0,
            // The draw clamps this down to the real last page — it knows the
            // line count, and it has to re-clamp on every frame anyway.
            KeyCode::Char('G') | KeyCode::End => view.scroll = usize::MAX,
            _ => {}
        }
    }

    // =============================================================================
    // Directory marks
    // =============================================================================
}

// =============================================================================
// Rendering helpers -- host-only, so they live with the panel they serve
// =============================================================================

/// Contextual help for the focused field. The form reserves a hint row even
/// when a field needs no explanation, keeping its fields in the same cells.
fn host_field_hint(field: HostField) -> Option<&'static str> {
    match field {
        // The label is a name. Nothing to explain.
        HostField::Label => None,
        HostField::CodexMode => Some("  Space toggle · affects launches and restarts"),
        HostField::CodexEndpoint => Some("  Unix socket; unix:// uses the default"),
        HostField::Target => Some("  ^t toggle ssh / socket"),
        // Point port setup toward the dedicated manager beside this field.
        HostField::Options => Some("  Quoted SSH arguments; tunnels in Services"),
        HostField::ShellCommand => Some("  Runs in work tabs; empty = default shell"),
        HostField::Forwards => Some("  Enter manage; apply host edits first"),
        HostField::Icon => Some("  ^e pick emoji   empty = auto"),
        // Name whose clipboard is offered, as well as the toggle key.
        HostField::Clipboard => Some("  Space toggle · offer the local clipboard"),
        HostField::SshAgent => Some("  Space toggle · use the local SSH agent on this host"),
    }
}

/// One form field's value, wrapped to `width` cells, with the cursor drawn where
/// it actually is.
///
/// A block parked after the text was honest while a field could only be appended
/// to. Now that the hosts panel's fields are [`TextInput`]s, the cursor is the
/// only thing on screen saying where the next character lands — so the cell
/// under it is reversed, with a reversed space standing in at end-of-text. An
/// unfocused field renders as plain text: two cursors in one form would be a
/// lie about which one the keyboard is in.
///
/// Wrapped rather than truncated because the value is one being *edited*: three
/// `-L` forwards outrun the card, and the tail the frame cut off was still there
/// on save, editable by a cursor nothing on screen could show. Always yields at
/// least one line, so an empty field still has a row. Pure.
pub(super) fn text_field_lines(
    input: &TextInput,
    focused: bool,
    width: usize,
) -> Vec<Vec<Span<'static>>> {
    let text = input.text();
    // `TextInput` keeps the cursor on a char boundary, so no split below can cut
    // a multi-byte glyph.
    let cursor = focused.then(|| input.cursor().min(text.len()));
    let ranges = wrap_ranges(text, width);
    let last = ranges.len() - 1;
    ranges
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let seg = &text[r.start..r.end];
            // The cursor belongs to the line holding its byte, so at a wrap point
            // it draws at the head of the *next* line — which is where the
            // character it is about to insert would be pushed anyway. End-of-text
            // has no next line, so the last one carries it as a reversed space.
            match cursor {
                Some(c) if c >= r.start && (c < r.end || i == last) => {
                    let (head, rest) = seg.split_at(c - r.start);
                    let mut chars = rest.chars();
                    let under = chars.next().map(String::from).unwrap_or_else(|| " ".into());
                    vec![
                        Span::raw(head.to_string()),
                        Span::styled(under, Style::default().add_modifier(Modifier::REVERSED)),
                        Span::raw(chars.as_str().to_string()),
                    ]
                }
                _ => vec![Span::raw(seg.to_string())],
            }
        })
        .collect()
}
