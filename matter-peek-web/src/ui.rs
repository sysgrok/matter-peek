//! Building blocks the pages share: headers, pills, empty states, dialogs.

use yew::prelude::*;

use crate::icons::{icon, Icon};

/// A page's title, what it shows, and its actions.
pub fn page_header(title: &str, subtitle: Html, actions: Html) -> Html {
    html! {
        <div class="page-head">
            <div class="page-head-text">
                <h1 class="page-title">{ title.to_string() }</h1>
                <p class="page-subtitle">{ subtitle }</p>
            </div>
            <div class="page-head-actions">{ actions }</div>
        </div>
    }
}

/// A small status pill: `class` is `is-up`, `is-warning`, `is-danger` or none.
pub fn pill(text: &str, class: Option<&'static str>, title: Option<String>) -> Html {
    html! {
        <span class={classes!("pill", class)} title={title}>{ text.to_string() }</span>
    }
}

/// A two-way switch between `left` and `right` (icon, label, title), its
/// thumb on the one `right` says (`None`: on neither); `compact` with icons
/// only. `on` gets the one picked: `true` for `right`.
pub fn two_way(
    left: (Icon, &'static str, &'static str),
    right: (Icon, &'static str, &'static str),
    right_on: Option<bool>,
    compact: bool,
    on: Callback<bool>,
) -> Html {
    let option = |to_right: bool, (glyph, label, title): (Icon, &'static str, &'static str)| {
        let checked = right_on == Some(to_right);
        html! {
            <button class="two-way-option" role="radio" aria-checked={checked.to_string()}
                {title} onclick={on.reform(move |_| to_right)}>
                { icon(glyph, 16) }
                if !compact {
                    <span>{ label }</span>
                }
            </button>
        }
    };

    html! {
        <div class={classes!("two-way", compact.then_some("is-compact"), match right_on {
            Some(true) => Some("is-right"),
            Some(false) => Some("is-left"),
            None => None,
        })} role="radiogroup">
            <span class="two-way-thumb"></span>
            { option(false, left) }
            { option(true, right) }
        </div>
    }
}

/// Which side the device cards show: their health, or their controls
/// (`true`; `None` when some show one, some the other).
pub fn face_switch(controls: Option<bool>, compact: bool, on: Callback<bool>) -> Html {
    two_way(
        (Icon::Gauge, "Health", "Show the health"),
        (Icon::Sliders, "Controls", "Show the controls"),
        controls,
        compact,
        on,
    )
}

/// What a page shows while it has nothing to show.
pub fn empty_state(glyph: Icon, title: &str, text: &str, action: Html) -> Html {
    html! {
        <div class="empty-state">
            <div class="empty-state-icon">{ icon(glyph, 30) }</div>
            <p class="empty-state-title">{ title.to_string() }</p>
            <p class="empty-state-text">{ text.to_string() }</p>
            { action }
        </div>
    }
}

/// A button with an icon and a label.
pub fn icon_button(
    glyph: Icon,
    label: &str,
    class: Classes,
    disabled: bool,
    title: Option<&str>,
    onclick: Callback<MouseEvent>,
) -> Html {
    html! {
        <button class={classes!("button", class)} {disabled} {onclick}
            title={title.map(str::to_string)}>
            <span class="icon">{ icon(glyph, 18) }</span>
            <span>{ label.to_string() }</span>
        </button>
    }
}

/// A notification with an icon, dismissed by `on_close`.
pub fn notification(
    glyph: Icon,
    class: Classes,
    text: Html,
    on_close: Callback<MouseEvent>,
) -> Html {
    html! {
        <div class={classes!("notification", "note", class)}>
            <button class="delete" onclick={on_close}></button>
            <span class="note-icon">{ icon(glyph, 20) }</span>
            <div class="note-text">{ text }</div>
        </div>
    }
}

/// A dialog asking to confirm what `action` does.
pub fn confirm_dialog(
    title: &str,
    text: &str,
    action: &str,
    danger: bool,
    on_confirm: Callback<MouseEvent>,
    on_cancel: Callback<MouseEvent>,
) -> Html {
    html! {
        <div class="modal is-active">
            <div class="modal-background" onclick={on_cancel.clone()}></div>
            <div class="modal-card dialog">
                <header class="modal-card-head">
                    <span class={classes!("dialog-icon", danger.then_some("is-danger"))}>
                        { icon(if danger { Icon::Trash } else { Icon::Alert }, 20) }
                    </span>
                    <p class="modal-card-title">{ title.to_string() }</p>
                </header>
                <section class="modal-card-body">
                    <p>{ text.to_string() }</p>
                </section>
                <footer class="modal-card-foot">
                    <div class="buttons">
                        <button class={classes!("button", if danger { "is-danger" } else { "is-warning" })}
                            onclick={on_confirm}>
                            { action.to_string() }
                        </button>
                        <button class="button" onclick={on_cancel}>{ "Cancel" }</button>
                    </div>
                </footer>
            </div>
        </div>
    }
}

/// The head of a full-screen popup: what it is, about what, and its buttons.
pub fn modal_head(
    eyebrow: &str,
    title: &str,
    buttons: Html,
    on_close: Callback<MouseEvent>,
) -> Html {
    html! {
        <header class="modal-card-head">
            <div class="modal-card-title modal-title">
                <span class="modal-eyebrow">{ eyebrow.to_string() }</span>
                <span>{ title.to_string() }</span>
            </div>
            <div class="modal-head-actions">{ buttons }</div>
            <button class="delete is-large ml-3" aria-label="close" onclick={on_close}></button>
        </header>
    }
}

/// `bytes` in B, KiB or MiB.
pub fn bytes(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes}B"),
        1024..1_048_576 => format!("{:.1}KB", bytes as f64 / 1024.0),
        _ => format!("{:.1}MB", bytes as f64 / 1_048_576.0),
    }
}
