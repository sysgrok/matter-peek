//! A device's memory, in a popup: its heap, and the stacks of the threads
//! (tasks) of its software, as its Software Diagnostics give them.

use yew::html::Scope;
use yew::prelude::*;

use matter_peek_core::api::{display, wire, SoftwareDiag, ThreadMetric};

use crate::icons::{icon, Icon};
use crate::{request_loop, ui, App, Msg};

/// Peak stack use, in percent of the stack, from which a task is short of
/// stack.
const TIGHT_PERCENT: u64 = 75;

/// … and from which it is about to overflow it.
const CRITICAL_PERCENT: u64 = 90;

/// The memory popup of one device.
pub struct MemoryView {
    pub node_id: u64,
    pub loading: bool,
    /// As last read; `None` before the first read.
    pub software: Option<SoftwareDiag>,
    pub error: Option<String>,
}

/// What happens in the memory popup.
pub enum MemoryMsg {
    Open(u64),
    Refresh,
    Close,
    Loaded(wire::Response),
    Failed(String),
}

impl App {
    pub(crate) fn update_memory(&mut self, ctx: &Context<Self>, msg: MemoryMsg) -> bool {
        let link = ctx.link();

        if let MemoryMsg::Open(node_id) = msg {
            self.memory = Some(MemoryView {
                node_id,
                loading: true,
                software: None,
                error: None,
            });
            peek(link, node_id);
            return true;
        }

        let Some(view) = self.memory.as_mut() else {
            return false;
        };

        match msg {
            MemoryMsg::Open(_) => unreachable!("opened above"),
            MemoryMsg::Refresh => {
                view.loading = true;
                view.error = None;
                peek(link, view.node_id);
            }
            MemoryMsg::Close => self.memory = None,
            MemoryMsg::Loaded(wire::Response::Peek { node_id, health }) => {
                if node_id == view.node_id {
                    view.loading = false;
                    view.software = Some(health.software.unwrap_or_default());
                }
            }
            MemoryMsg::Loaded(wire::Response::Error { message }) | MemoryMsg::Failed(message) => {
                view.loading = false;
                view.error = Some(message);
            }
            MemoryMsg::Loaded(_) => {}
        }

        true
    }

    /// Full-screen popup with a device's heap and tasks.
    pub(crate) fn view_memory_modal(&self, link: &Scope<Self>) -> Html {
        let Some(view) = &self.memory else {
            return html! {};
        };
        let title = self.device_title(view.node_id);

        html! {
            <div class="modal is-active">
                <div class="modal-background"></div>
                <div class="modal-card is-fullscreen">
                    { ui::modal_head("Memory", &title, html! {
                        <>
                        { self.device_state(view.node_id).1 }
                        <button
                            class={classes!("button", "is-small", view.loading.then_some("is-loading"))}
                            title="Read them again"
                            disabled={view.loading}
                            onclick={link.callback(|_| memory(MemoryMsg::Refresh))}>
                            <span class="icon is-small">{ icon(Icon::Refresh, 16) }</span>
                            <span>{ "Refresh" }</span>
                        </button>
                        </>
                    }, link.callback(|_| memory(MemoryMsg::Close))) }
                    <section class="modal-card-body">
                        <div class="app container">
                            if let Some(error) = &view.error {
                                <p class="tasks-error">{ icon(Icon::Alert, 16) }<span>{ error.clone() }</span></p>
                            }
                            { match &view.software {
                                None if view.loading => html! {
                                    <div class="empty-state">
                                        <span class="loader is-large"></span>
                                        <p class="empty-state-text">{ "Reading its memory…" }</p>
                                    </div>
                                },
                                None => html! {},
                                Some(software) if is_silent(software) => ui::empty_state(
                                    Icon::Chip,
                                    "No memory figures",
                                    "The device reports neither its heap nor its tasks (Software Diagnostics).",
                                    html! {},
                                ),
                                Some(software) => html! {
                                    <>
                                        { view_heap(software) }
                                        { view_threads(software.thread_metrics.as_deref()) }
                                    </>
                                },
                            } }
                        </div>
                    </section>
                </div>
            </div>
        }
    }
}

/// Whether the device reports nothing of its memory.
fn is_silent(software: &SoftwareDiag) -> bool {
    software.current_heap_free.is_none()
        && software.current_heap_used.is_none()
        && software.current_heap_high_watermark.is_none()
        && software.thread_metrics.as_ref().is_none_or(Vec::is_empty)
}

/// Its heap: used of all, and the peak.
fn view_heap(software: &SoftwareDiag) -> Html {
    let (free, used, peak) = (
        software.current_heap_free,
        software.current_heap_used,
        software.current_heap_high_watermark,
    );
    if free.is_none() && used.is_none() && peak.is_none() {
        return html! {};
    }

    let total = free.zip(used).map(|(free, used)| free + used);
    let percent = |value: Option<u64>| {
        value
            .zip(total)
            .filter(|(_, total)| *total > 0)
            .map(|(value, total)| value * 100 / total)
    };

    let figures = [("Used", used), ("Free", free), ("Peak", peak)];

    html! {
        <section class="tasks-section">
            <p class="tasks-section-title">{ "Heap" }</p>
            <div class="heap-card">
                if let Some(used_percent) = percent(used) {
                    <div class="stack-bar is-wide" title={format!("{used_percent}% used")}>
                        <span class={classes!("stack-bar-fill", level(used_percent))}
                            style={format!("width: {used_percent}%")}></span>
                        if let Some(peak_percent) = percent(peak) {
                            <span class="stack-bar-mark" title="Peak"
                                style={format!("left: {}%", peak_percent.min(100))}></span>
                        }
                    </div>
                }
                <div class="heap-figures">
                    { for figures.into_iter().filter_map(|(label, value)| value.map(|value| html! {
                        <div class="heap-figure">
                            <span class="health-label">{ label }</span>
                            <span class="health-value">{ display::bytes(value) }</span>
                        </div>
                    })) }
                    if let Some(total) = total {
                        <div class="heap-figure">
                            <span class="health-label">{ "Total" }</span>
                            <span class="health-value">{ display::bytes(total) }</span>
                        </div>
                    }
                </div>
            </div>
        </section>
    }
}

/// Its tasks, the one with the least stack to spare first.
fn view_threads(threads: Option<&[ThreadMetric]>) -> Html {
    let Some(threads) = threads.filter(|threads| !threads.is_empty()) else {
        return ui::empty_state(
            Icon::Chip,
            "No tasks reported",
            "The device does not list its tasks (Software Diagnostics' Thread Metrics).",
            html! {},
        );
    };

    let mut sorted: Vec<&ThreadMetric> = threads.iter().collect();
    sorted.sort_by_key(|thread| thread.spare_rank());
    let no_figures = threads
        .iter()
        .all(|thread| thread.stack_size.is_none() && thread.stack_free_minimum.is_none());

    html! {
        <section class="tasks-section">
            <p class="tasks-section-title">
                { "Tasks" }
                <span class="nav-count">{ threads.len() }</span>
                if !no_figures {
                    <span class="tasks-section-hint">{ "least stack to spare first" }</span>
                }
            </p>
            if no_figures {
                <p class="tasks-note">
                    { icon(Icon::Info, 16) }
                    <span>{ "The device names its tasks, but gives no stack figures for them." }</span>
                </p>
            }
            <div class="task-list">
                { for sorted.into_iter().map(view_thread) }
            </div>
        </section>
    }
}

fn view_thread(thread: &ThreadMetric) -> Html {
    let name = thread
        .name
        .clone()
        .unwrap_or_else(|| format!("Task {}", thread.id));
    let used = thread
        .stack_peak()
        .zip(thread.stack_size)
        .filter(|(_, size)| *size > 0)
        .map(|(peak, size)| (peak, size, peak as u64 * 100 / size as u64));

    let mut figures = Vec::new();
    match used {
        Some((peak, size, _)) => figures.push(format!(
            "peak {} of {}",
            display::bytes(peak as u64),
            display::bytes(size as u64)
        )),
        None => {
            if let Some(size) = thread.stack_size {
                figures.push(format!("stack {}", display::bytes(size as u64)));
            }
            if let Some(least) = thread.stack_free_minimum {
                figures.push(format!("least free {}", display::bytes(least as u64)));
            }
        }
    }
    if let Some(free) = thread.stack_free_current {
        figures.push(format!("free now {}", display::bytes(free as u64)));
    }

    html! {
        <div class="task-row">
            <div class="task-name">
                <span class="task-name-text">{ name }</span>
                <span class="task-id mono">{ format!("#{}", thread.id) }</span>
            </div>
            <div class="task-stack">
                if let Some((_, _, percent)) = used {
                    <div class="stack-bar" title={format!("At most {percent}% of its stack used")}>
                        <span class={classes!("stack-bar-fill", level(percent))}
                            style={format!("width: {}%", percent.min(100))}></span>
                    </div>
                    <span class={classes!("task-percent", level(percent))}>{ format!("{percent}%") }</span>
                }
                <span class="task-figures">
                    if figures.is_empty() {
                        { "no stack figures" }
                    } else {
                        { figures.join(" · ") }
                    }
                </span>
            </div>
        </div>
    }
}

/// The class of a use of `percent`.
fn level(percent: u64) -> &'static str {
    match percent {
        CRITICAL_PERCENT.. => "is-critical",
        TIGHT_PERCENT.. => "is-tight",
        _ => "is-fine",
    }
}

fn memory(msg: MemoryMsg) -> Msg {
    Msg::Memory(msg)
}

/// Peek at the device; the answer goes to the popup.
fn peek(link: &Scope<App>, node_id: u64) {
    request_loop(
        link.clone(),
        wire::Request::PeekDevice { node_id },
        |response| memory(MemoryMsg::Loaded(response)),
        |e| memory(MemoryMsg::Failed(e)),
    );
}
