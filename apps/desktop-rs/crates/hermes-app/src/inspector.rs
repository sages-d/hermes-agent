//! Right-side Workspace Inspector pane for Milestone 2:
//! - `Terminal`: live native PTY + VT100 emulator (`hermes-terminal`), kept alive when hidden.
//! - `Git`: branch/worktree status, one-click Stage/Unstage, and color-coded unified diffs (`hermes-git`).
//! - `Files`: workspace directory browser + `syntect` syntax-highlighted file viewer.

use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, ClipboardItem, Context, FontWeight, KeyDownEvent, SharedString, div, prelude::*,
    px, rgba,
};
use hermes_git::{
    ChangedFile, DiffLine, DiffLineKind, FileChangeKind, RepoStatus, WorktreeEntry, file_diff,
    list_worktrees, repo_status, stage_file, unstage_file,
};
use hermes_terminal::{NativeTerminal, TerminalSnapshot};

use crate::highlight::highlighted_code;
use crate::theme::HermesTheme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectorTab {
    Terminal,
    Git,
    Files,
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

#[derive(Debug, Clone)]
pub struct FilePreview {
    pub path: PathBuf,
    pub extension: String,
    pub content: String,
}

pub struct InspectorState {
    pub open: bool,
    pub tab: InspectorTab,
    pub workspace_dir: PathBuf,

    // Native PTY terminal (lazily spawned on first open, preserved across hide/show)
    pub terminal: Option<NativeTerminal>,
    pub terminal_snap: TerminalSnapshot,
    pub terminal_error: Option<String>,

    // Git review panel
    pub git_status: Option<RepoStatus>,
    pub git_worktrees: Vec<WorktreeEntry>,
    pub git_error: Option<String>,
    pub selected_diff_file: Option<(String, bool)>,
    pub diff_lines: Vec<DiffLine>,

    // Workspace file explorer
    pub browse_dir: PathBuf,
    pub entries: Vec<FileEntry>,
    pub preview: Option<FilePreview>,
}

impl InspectorState {
    pub fn new(workspace_dir: PathBuf) -> Self {
        let mut st = Self {
            open: false,
            tab: InspectorTab::Terminal,
            browse_dir: workspace_dir.clone(),
            workspace_dir,
            terminal: None,
            terminal_snap: TerminalSnapshot::default(),
            terminal_error: None,
            git_status: None,
            git_worktrees: Vec::new(),
            git_error: None,
            selected_diff_file: None,
            diff_lines: Vec::new(),
            entries: Vec::new(),
            preview: None,
        };
        st.refresh_files();
        st
    }

    pub fn refresh_git(&mut self) {
        match repo_status(&self.workspace_dir) {
            Ok(status) => {
                self.git_worktrees = list_worktrees(&status.root).unwrap_or_default();
                self.git_error = None;
                // Re-hydrate currently selected diff if still relevant.
                if let Some((ref path, staged)) = self.selected_diff_file {
                    let untracked = status
                        .files
                        .iter()
                        .any(|f| &f.path == path && f.kind == FileChangeKind::Untracked);
                    self.diff_lines =
                        file_diff(&status.root, path, staged, untracked).unwrap_or_default();
                } else if let Some(first) = status.files.first() {
                    let untracked = first.kind == FileChangeKind::Untracked;
                    self.selected_diff_file = Some((first.path.clone(), first.staged));
                    self.diff_lines =
                        file_diff(&status.root, &first.path, first.staged, untracked)
                            .unwrap_or_default();
                } else {
                    self.diff_lines.clear();
                }
                self.git_status = Some(status);
            }
            Err(err) => {
                self.git_status = None;
                self.git_worktrees.clear();
                self.diff_lines.clear();
                self.git_error = Some(err.to_string());
            }
        }
    }

    pub fn select_diff(&mut self, path: String, staged: bool, untracked: bool) {
        self.selected_diff_file = Some((path.clone(), staged));
        if let Some(st) = &self.git_status {
            self.diff_lines = file_diff(&st.root, &path, staged, untracked).unwrap_or_default();
        }
    }

    pub fn toggle_stage(&mut self, path: &str, currently_staged: bool) {
        let Some(root) = self.git_status.as_ref().map(|s| s.root.clone()) else {
            return;
        };
        let res = if currently_staged {
            unstage_file(&root, path)
        } else {
            stage_file(&root, path)
        };
        if let Err(err) = res {
            self.git_error = Some(err.to_string());
        } else {
            self.selected_diff_file = Some((path.to_string(), !currently_staged));
            self.refresh_git();
        }
    }

    pub fn refresh_files(&mut self) {
        self.entries = read_directory_entries(&self.browse_dir);
    }

    pub fn open_path(&mut self, path: PathBuf) {
        if path.is_dir() {
            self.browse_dir = path;
            self.refresh_files();
            return;
        }
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_string();
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| format!("(Unable to read as UTF-8 text: {e})"));
        let clipped: String = raw.lines().take(300).collect::<Vec<_>>().join("\n");
        self.preview = Some(FilePreview {
            path,
            extension,
            content: clipped,
        });
    }

    pub fn navigate_up(&mut self) {
        if let Some(parent) = self.browse_dir.parent() {
            self.browse_dir = parent.to_path_buf();
            self.refresh_files();
        }
    }

    /// Translate a GPUI `KeyDownEvent` into raw VT100/PTY bytes for the embedded shell.
    pub fn send_terminal_key(&self, event: &KeyDownEvent, clipboard_text: Option<String>) -> bool {
        let Some(term) = &self.terminal else {
            return false;
        };
        let key = event.keystroke.key.as_str();
        let mods = &event.keystroke.modifiers;

        // Cmd+V pastes into the terminal
        if mods.platform && key == "v" {
            if let Some(text) = clipboard_text {
                let _ = term.write_bytes(text.as_bytes());
            }
            return true;
        }

        // Ctrl+<a..z> control sequences (Ctrl+C, Ctrl+D, Ctrl+L, Ctrl+Z, ...)
        if mods.control && !mods.platform && key.len() == 1 {
            let b = key.as_bytes()[0];
            if b.is_ascii_alphabetic() {
                let ctrl = (b.to_ascii_lowercase() - b'a') + 1;
                let _ = term.write_bytes(&[ctrl]);
                return true;
            }
        }

        let seq: Option<&[u8]> = match key {
            "enter" => Some(b"\r"),
            "backspace" => Some(b"\x7f"),
            "tab" => Some(b"\t"),
            "escape" => Some(b"\x1b"),
            "up" => Some(b"\x1b[A"),
            "down" => Some(b"\x1b[B"),
            "right" => Some(b"\x1b[C"),
            "left" => Some(b"\x1b[D"),
            "home" => Some(b"\x1b[H"),
            "end" => Some(b"\x1b[F"),
            "delete" => Some(b"\x1b[3~"),
            _ => None,
        };
        if let Some(bytes) = seq {
            let _ = term.write_bytes(bytes);
            return true;
        }

        if !mods.platform
            && !mods.control
            && let Some(ch) = &event.keystroke.key_char
        {
            let _ = term.write_bytes(ch.as_bytes());
            return true;
        }

        false
    }
}

fn read_directory_entries(dir: &Path) -> Vec<FileEntry> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if matches!(name.as_str(), ".git" | "node_modules" | "target" | ".DS_Store") {
            continue;
        }
        let path = entry.path();
        let is_dir = path.is_dir();
        out.push(FileEntry { name, path, is_dir });
    }
    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

/// Render the terminal tab body.
pub fn render_terminal_body<V: 'static>(
    state: &InspectorState,
    theme: &HermesTheme,
    focused: bool,
    on_focus: impl Fn(&mut V, &mut Context<V>) + 'static,
    on_restart: impl Fn(&mut V, &mut Context<V>) + 'static,
    cx: &mut Context<V>,
) -> AnyElement {
    let mut lines_col = div()
        .id("terminal-screen")
        .flex()
        .flex_col()
        .flex_1()
        .w_full()
        .overflow_y_scroll()
        .p_3()
        .bg(theme.code_bg)
        .font_family("Menlo")
        .text_xs()
        .line_height(px(16.0))
        .cursor_text()
        .on_click(cx.listener(move |this, _, _, cx| on_focus(this, cx)));

    if let Some(err) = &state.terminal_error {
        lines_col = lines_col.child(
            div()
                .text_color(theme.destructive)
                .child(format!("Terminal error: {err}")),
        );
    } else if state.terminal_snap.lines.is_empty() {
        lines_col = lines_col.child(
            div()
                .text_color(theme.fg_muted)
                .child("Starting native PTY shell..."),
        );
    } else {
        for (row_idx, line) in state.terminal_snap.lines.iter().enumerate() {
            let is_cursor_row = focused && row_idx == state.terminal_snap.cursor_row as usize;
            let text = if line.is_empty() {
                " ".to_string()
            } else {
                line.clone()
            };
            lines_col = lines_col.child(
                div()
                    .w_full()
                    .when(is_cursor_row, |d| d.bg(theme.accent_subtle))
                    .text_color(theme.fg)
                    .child(text),
            );
        }
    }

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .w_full()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .px_3()
                .py_1p5()
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.surface)
                .child(
                    div()
                        .text_xs()
                        .text_color(if focused { theme.accent } else { theme.fg_muted })
                        .child(if state.terminal_snap.exited {
                            "Shell exited"
                        } else if focused {
                            "PTY focused (typing goes to shell · Esc returns to chat)"
                        } else {
                            "Click terminal to type"
                        }),
                )
                .child(
                    div()
                        .id("terminal-restart-btn")
                        .px_2()
                        .py_0p5()
                        .rounded_sm()
                        .bg(theme.surface_hover)
                        .cursor_pointer()
                        .text_xs()
                        .text_color(theme.fg)
                        .child("Restart")
                        .on_click(cx.listener(move |this, _, _, cx| on_restart(this, cx))),
                ),
        )
        .child(lines_col)
        .into_any_element()
}

/// Render the Git Review tab body.
pub fn render_git_body<V: 'static>(
    state: &InspectorState,
    theme: &HermesTheme,
    on_refresh: impl Fn(&mut V, &mut Context<V>) + 'static,
    on_select: impl Fn(&mut V, String, bool, bool, &mut Context<V>) + Clone + 'static,
    on_toggle_stage: impl Fn(&mut V, String, bool, &mut Context<V>) + Clone + 'static,
    cx: &mut Context<V>,
) -> AnyElement {
    let header_label = match &state.git_status {
        Some(st) => {
            let wt = state.git_worktrees.len();
            format!(
                "⎇ {} (↑{} ↓{} · {} wt{})",
                st.branch,
                st.ahead,
                st.behind,
                wt,
                if wt == 1 { "" } else { "s" }
            )
        }
        None => "Not a git repository".into(),
    };

    let mut file_list = div()
        .id("git-file-list")
        .flex()
        .flex_col()
        .max_h(px(210.0))
        .overflow_y_scroll()
        .p_2()
        .gap_1()
        .border_b_1()
        .border_color(theme.border);

    if let Some(err) = &state.git_error {
        file_list = file_list.child(
            div()
                .text_xs()
                .text_color(theme.destructive)
                .child(err.clone()),
        );
    } else if let Some(st) = &state.git_status {
        if st.files.is_empty() {
            file_list = file_list.child(
                div()
                    .px_2()
                    .py_2()
                    .text_xs()
                    .text_color(theme.fg_muted)
                    .child("Working tree clean."),
            );
        } else {
            for (idx, file) in st.files.iter().enumerate() {
                file_list = file_list.child(render_changed_file_row(
                    idx,
                    file,
                    state.selected_diff_file.as_ref(),
                    theme,
                    on_select.clone(),
                    on_toggle_stage.clone(),
                    cx,
                ));
            }
        }
    }

    let mut diff_view = div()
        .id("git-diff-scroll")
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .p_2p5()
        .bg(theme.code_bg)
        .font_family("Menlo")
        .text_xs()
        .line_height(px(16.0));

    if state.diff_lines.is_empty() {
        diff_view = diff_view.child(
            div()
                .text_color(theme.fg_muted)
                .child("Select a changed file to inspect its unified diff."),
        );
    } else {
        for line in &state.diff_lines {
            diff_view = diff_view.child(render_diff_line(line, theme));
        }
    }

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .w_full()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .px_3()
                .py_1p5()
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.surface)
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.fg)
                        .child(header_label),
                )
                .child(
                    div()
                        .id("git-refresh-btn")
                        .px_2()
                        .py_0p5()
                        .rounded_sm()
                        .bg(theme.surface_hover)
                        .cursor_pointer()
                        .text_xs()
                        .text_color(theme.fg)
                        .child("Refresh")
                        .on_click(cx.listener(move |this, _, _, cx| on_refresh(this, cx))),
                ),
        )
        .child(file_list)
        .child(diff_view)
        .into_any_element()
}

fn render_changed_file_row<V: 'static>(
    idx: usize,
    file: &ChangedFile,
    selected: Option<&(String, bool)>,
    theme: &HermesTheme,
    on_select: impl Fn(&mut V, String, bool, bool, &mut Context<V>) + 'static,
    on_toggle_stage: impl Fn(&mut V, String, bool, &mut Context<V>) + 'static,
    cx: &mut Context<V>,
) -> AnyElement {
    let is_sel = selected.is_some_and(|(p, s)| p == &file.path && *s == file.staged);
    let badge = match file.kind {
        FileChangeKind::Added => ("A", theme.success),
        FileChangeKind::Deleted => ("D", theme.destructive),
        FileChangeKind::Modified => ("M", theme.warning),
        FileChangeKind::Renamed => ("R", theme.accent),
        FileChangeKind::Untracked => ("?", theme.fg_muted),
    };
    let path_sel = file.path.clone();
    let path_stage = file.path.clone();
    let staged = file.staged;
    let untracked = file.kind == FileChangeKind::Untracked;

    div()
        .id(SharedString::from(format!("git-file-{idx}")))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_2()
        .px_2()
        .py_1()
        .rounded_sm()
        .cursor_pointer()
        .when(is_sel, |d| d.bg(theme.accent_subtle))
        .when(!is_sel, |d| d.hover(|s| s.bg(theme.surface_hover)))
        .on_click(cx.listener(move |this, _, _, cx| {
            on_select(this, path_sel.clone(), staged, untracked, cx);
        }))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1p5()
                .min_w_0()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .font_family("Menlo")
                        .text_color(badge.1)
                        .child(badge.0),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.fg)
                        .truncate()
                        .child(file.path.clone()),
                ),
        )
        .child(
            div()
                .id(SharedString::from(format!("git-stage-{idx}")))
                .px_1p5()
                .py_0p5()
                .rounded_sm()
                .bg(theme.surface)
                .border_1()
                .border_color(theme.border)
                .text_xs()
                .text_color(if staged { theme.success } else { theme.fg_muted })
                .child(if staged { "Staged ✓" } else { "Stage" })
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    on_toggle_stage(this, path_stage.clone(), staged, cx);
                })),
        )
        .into_any_element()
}

fn render_diff_line(line: &DiffLine, theme: &HermesTheme) -> AnyElement {
    let (bg, fg) = match line.kind {
        DiffLineKind::Added => (Some(rgba(0x2ea04328).into()), theme.success),
        DiffLineKind::Removed => (Some(rgba(0xf8514928).into()), theme.destructive),
        DiffLineKind::Hunk => (Some(theme.accent_subtle), theme.accent),
        DiffLineKind::Header => (None, theme.fg_muted),
        DiffLineKind::Context => (None, theme.fg),
    };
    div()
        .w_full()
        .px_1()
        .when_some(bg, |d, c| d.bg(c))
        .text_color(fg)
        .child(if line.text.is_empty() {
            " ".to_string()
        } else {
            line.text.clone()
        })
        .into_any_element()
}

/// Render the Workspace File Explorer + Code Preview tab body.
pub fn render_files_body<V: 'static>(
    state: &InspectorState,
    theme: &HermesTheme,
    on_up: impl Fn(&mut V, &mut Context<V>) + 'static,
    on_open: impl Fn(&mut V, PathBuf, &mut Context<V>) + Clone + 'static,
    cx: &mut Context<V>,
) -> AnyElement {
    let mut tree = div()
        .id("files-tree-scroll")
        .flex()
        .flex_col()
        .max_h(px(220.0))
        .overflow_y_scroll()
        .p_2()
        .gap_0p5()
        .border_b_1()
        .border_color(theme.border);

    for (idx, entry) in state.entries.iter().enumerate() {
        let p = entry.path.clone();
        let open_cb = on_open.clone();
        let icon = if entry.is_dir { "▸" } else { "·" };
        tree = tree.child(
            div()
                .id(SharedString::from(format!("file-row-{idx}")))
                .flex()
                .flex_row()
                .items_center()
                .gap_1p5()
                .px_2()
                .py_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(|s| s.bg(theme.surface_hover))
                .on_click(cx.listener(move |this, _, _, cx| open_cb(this, p.clone(), cx)))
                .child(div().text_xs().text_color(theme.fg_muted).child(icon))
                .child(
                    div()
                        .text_xs()
                        .font_weight(if entry.is_dir {
                            FontWeight::MEDIUM
                        } else {
                            FontWeight::NORMAL
                        })
                        .text_color(if entry.is_dir {
                            theme.accent
                        } else {
                            theme.fg
                        })
                        .truncate()
                        .child(entry.name.clone()),
                ),
        );
    }

    let mut preview_pane = div()
        .id("file-preview-scroll")
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .bg(theme.code_bg);

    if let Some(prev) = &state.preview {
        let copy_path = prev.path.to_string_lossy().into_owned();
        let header_name = prev
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| copy_path.clone());
        let code_elem = highlighted_code(&prev.content, &prev.extension, theme.mode);

        preview_pane = preview_pane
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.surface)
                    .child(
                        div()
                            .text_xs()
                            .font_family("Menlo")
                            .text_color(theme.fg)
                            .child(header_name),
                    )
                    .child(
                        div()
                            .id("copy-file-path")
                            .px_1p5()
                            .py_0p5()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_xs()
                            .text_color(theme.fg_muted)
                            .hover(|s| s.bg(theme.surface_hover).text_color(theme.fg))
                            .child("Copy Path")
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
                            }),
                    ),
            )
            .child(
                div()
                    .p_3()
                    .font_family("Menlo")
                    .text_xs()
                    .line_height(px(17.0))
                    .text_color(theme.fg)
                    .child(code_elem),
            );
    } else {
        preview_pane = preview_pane.child(
            div()
                .p_3()
                .text_xs()
                .text_color(theme.fg_muted)
                .child("Click any file above to inspect it with syntax highlighting."),
        );
    }

    let dir_label = state
        .browse_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| state.browse_dir.to_string_lossy().into_owned());

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .w_full()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .px_3()
                .py_1p5()
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.surface)
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .font_family("Menlo")
                        .text_color(theme.fg)
                        .truncate()
                        .child(dir_label),
                )
                .child(
                    div()
                        .id("files-up-btn")
                        .px_2()
                        .py_0p5()
                        .rounded_sm()
                        .bg(theme.surface_hover)
                        .cursor_pointer()
                        .text_xs()
                        .text_color(theme.fg)
                        .child("↑ Up")
                        .on_click(cx.listener(move |this, _, _, cx| on_up(this, cx))),
                ),
        )
        .child(tree)
        .child(preview_pane)
        .into_any_element()
}
