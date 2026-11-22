//! Management Overlay (`Cmd+,`) for Settings / Config, Skills & Capabilities, Cron, and Usage.
//!
//! Follows `apps/desktop/DESIGN.md`: route overlays render as modal cards over the workspace
//! and return cleanly to the chat transcript on `Esc` or scrim click.

use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px, rgba};
use serde_json::Value;

use crate::state::{CompanionState, ConnectionStatus};
use crate::theme::HermesTheme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagementTab {
    Config,
    Capabilities,
    Cron,
    Usage,
}

#[derive(Debug, Clone, Default)]
pub struct ManagementData {
    pub config_json: Option<Value>,
    pub cron_json: Option<Value>,
    pub usage_json: Option<Value>,
}

impl ManagementData {
    pub fn demo() -> Self {
        Self {
            config_json: Some(serde_json::json!({
                "model": "claude-sonnet-4",
                "provider": "anthropic",
                "terminal.backend": "local",
                "profile": "default"
            })),
            cron_json: Some(serde_json::json!({
                "scheduler": "active",
                "jobs": [
                    { "id": "daily-digest", "schedule": "0 9 * * *", "status": "idle" }
                ]
            })),
            usage_json: Some(serde_json::json!({
                "session_tokens_in": 12450,
                "session_tokens_out": 3890,
                "cache_hit_ratio": "91.4%"
            })),
        }
    }
}

pub fn render_management_overlay<V: 'static>(
    tab: ManagementTab,
    state: &CompanionState,
    data: &ManagementData,
    theme: &HermesTheme,
    on_close: impl Fn(&mut V, &mut Context<V>) + 'static,
    on_tab: impl Fn(&mut V, ManagementTab, &mut Context<V>) + Clone + 'static,
    on_refresh: impl Fn(&mut V, &mut Context<V>) + 'static,
    cx: &mut Context<V>,
) -> AnyElement {
    let tabs = [
        (ManagementTab::Config, "Config & Gateway"),
        (ManagementTab::Capabilities, "Skills & Commands"),
        (ManagementTab::Cron, "Cron Jobs"),
        (ManagementTab::Usage, "Usage"),
    ];

    let mut tab_bar = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .px_3()
        .py_2()
        .border_b_1()
        .border_color(theme.border);

    for (idx, (t, label)) in tabs.into_iter().enumerate() {
        let is_active = t == tab;
        let cb = on_tab.clone();
        tab_bar = tab_bar.child(
            div()
                .id(SharedString::from(format!("mgmt-tab-{idx}")))
                .px_2p5()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .text_xs()
                .font_weight(if is_active {
                    FontWeight::SEMIBOLD
                } else {
                    FontWeight::MEDIUM
                })
                .when(is_active, |d| d.bg(theme.accent_subtle).text_color(theme.accent))
                .when(!is_active, |d| {
                    d.text_color(theme.fg_muted)
                        .hover(|s| s.bg(theme.surface_hover).text_color(theme.fg))
                })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| cb(this, t, cx))),
        );
    }

    let body: AnyElement = match tab {
        ManagementTab::Config => {
            let conn_line = match &state.status {
                ConnectionStatus::Connected {
                    profile,
                    instance_id,
                    protocol,
                } => format!("Profile: {profile} · Instance: {instance_id} · Protocol: {protocol}"),
                other => other.label(),
            };
            let pretty = data
                .config_json
                .as_ref()
                .and_then(|v| serde_json::to_string_pretty(v).ok())
                .unwrap_or_else(|| "No config snapshot loaded yet. Click Refresh when connected.".into());

            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(theme.surface)
                        .border_1()
                        .border_color(theme.border)
                        .text_xs()
                        .text_color(theme.fg)
                        .child(conn_line),
                )
                .child(
                    div()
                        .p_3()
                        .rounded_md()
                        .bg(theme.code_bg)
                        .border_1()
                        .border_color(theme.border)
                        .font_family("Menlo")
                        .text_xs()
                        .line_height(px(17.0))
                        .text_color(theme.fg)
                        .child(pretty),
                )
                .into_any_element()
        }
        ManagementTab::Capabilities => {
            let mut rows = div().flex().flex_col().gap_1();
            for cmd in &state.slash_catalog {
                rows = rows.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .px_3()
                        .py_1p5()
                        .rounded_md()
                        .bg(theme.surface)
                        .border_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_family("Menlo")
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(theme.accent)
                                        .child(cmd.name.clone()),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.fg_muted)
                                        .child(cmd.description.clone()),
                                ),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.fg_muted)
                                .child(cmd.category.clone()),
                        ),
                );
            }
            rows.into_any_element()
        }
        ManagementTab::Cron => {
            let pretty = data
                .cron_json
                .as_ref()
                .and_then(|v| serde_json::to_string_pretty(v).ok())
                .unwrap_or_else(|| "No cron status loaded. Click Refresh when connected to gateway.".into());
            div()
                .p_3()
                .rounded_md()
                .bg(theme.code_bg)
                .border_1()
                .border_color(theme.border)
                .font_family("Menlo")
                .text_xs()
                .line_height(px(17.0))
                .text_color(theme.fg)
                .child(pretty)
                .into_any_element()
        }
        ManagementTab::Usage => {
            let pretty = data
                .usage_json
                .as_ref()
                .and_then(|v| serde_json::to_string_pretty(v).ok())
                .unwrap_or_else(|| "No usage snapshot loaded. Click Refresh when connected.".into());
            div()
                .p_3()
                .rounded_md()
                .bg(theme.code_bg)
                .border_1()
                .border_color(theme.border)
                .font_family("Menlo")
                .text_xs()
                .line_height(px(17.0))
                .text_color(theme.fg)
                .child(pretty)
                .into_any_element()
        }
    };

    div()
        .id("management-scrim")
        .absolute()
        .inset_0()
        .flex()
        .flex_col()
        .items_center()
        .pt(px(64.0))
        .bg(rgba(0x010409aa))
        .on_click(cx.listener(move |this, _, _, cx| on_close(this, cx)))
        .child(
            div()
                .id("management-card")
                .flex()
                .flex_col()
                .w(px(660.0))
                .max_h(px(540.0))
                .rounded_lg()
                .bg(theme.popover)
                .border_1()
                .border_color(theme.border)
                .shadow_lg()
                .overflow_hidden()
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .px_4()
                        .py_3()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.fg)
                                .child("Hermes Control Center"),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .id("mgmt-refresh-btn")
                                        .px_2()
                                        .py_1()
                                        .rounded_sm()
                                        .bg(theme.surface)
                                        .border_1()
                                        .border_color(theme.border)
                                        .cursor_pointer()
                                        .text_xs()
                                        .text_color(theme.fg)
                                        .child("Refresh")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            on_refresh(this, cx);
                                        })),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.fg_muted)
                                        .child("Esc to close"),
                                ),
                        ),
                )
                .child(tab_bar)
                .child(
                    div()
                        .id("mgmt-body-scroll")
                        .flex_1()
                        .overflow_y_scroll()
                        .p_4()
                        .child(body),
                ),
        )
        .into_any_element()
}
