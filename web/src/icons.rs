//! The icons of the UI: inline SVG strokes (Lucide-style), drawn in the
//! current text color.

use yew::prelude::*;

use matter_peek_core::api;

/// The icons of the UI, drawn in the current text color.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Wifi,
    Thread,
    Ethernet,
    ArrowUp,
    ArrowDown,
    ArrowsUpDown,
    Connect,
    Trash,
    Plus,
    Lock,
    Eye,
    EyeOff,
    Refresh,
    Router,
    Channel,
    Gauge,
    Interfaces,
    Neighbors,
    Menu,
    Close,
    Logo,
    Devices,
    Commission,
    Updates,
    Search,
    Sync,
    Identify,
    Logs,
    Remember,
    Tag,
    Edit,
    Download,
    Upload,
    Bluetooth,
    QrCode,
    Key,
    Check,
    CheckCircle,
    XCircle,
    Alert,
    Info,
    Clock,
    Reboot,
    Timer,
    Chip,
    Globe,
    Radar,
    External,
    UpdateArrow,
    ChevronRight,
    Camera,
}

pub fn icon(glyph: Icon, size: u32) -> Html {
    let shape = match glyph {
        Icon::Wifi => html! {
            <>
                <path d="M2 8.5a15 15 0 0 1 20 0" />
                <path d="M5.5 12a10 10 0 0 1 13 0" />
                <path d="M9 15.5a5 5 0 0 1 6 0" />
                <circle cx="12" cy="19" r="1" fill="currentColor" />
            </>
        },
        Icon::Thread => html! {
            <>
                <circle cx="5" cy="6" r="2" />
                <circle cx="19" cy="6" r="2" />
                <circle cx="12" cy="19" r="2" />
                <path d="M7 6h10M6 8l5 9M18 8l-5 9" />
            </>
        },
        Icon::Ethernet => html! {
            <>
                <path d="M3 5h18v11h-5v3H8v-3H3z" />
                <path d="M7 8v3M10 8v3M14 8v3M17 8v3" />
            </>
        },
        Icon::ArrowUp => html! { <path d="M12 19V5M5 12l7-7 7 7" /> },
        Icon::ArrowDown => html! { <path d="M12 5v14M19 12l-7 7-7-7" /> },
        Icon::ArrowsUpDown => html! { <path d="M7 4v16M3 8l4-4 4 4M17 20V4M13 16l4 4 4-4" /> },
        Icon::Connect => html! {
            <>
                <path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71" />
                <path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71" />
            </>
        },
        Icon::Trash => html! {
            <>
                <path d="M3 6h18M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
                <path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6M10 11v6M14 11v6" />
            </>
        },
        Icon::Plus => html! { <path d="M12 5v14M5 12h14" /> },
        Icon::Lock => html! {
            <>
                <rect x="3" y="11" width="18" height="11" rx="2" />
                <path d="M7 11V7a5 5 0 0 1 10 0v4" />
            </>
        },
        Icon::Eye => html! {
            <>
                <path d="M2 12s3-7 10-7 10 7 10 7-3 7-10 7-10-7-10-7z" />
                <circle cx="12" cy="12" r="3" />
            </>
        },
        Icon::EyeOff => html! {
            <>
                <path d="M9.9 4.24A9.1 9.1 0 0 1 12 4c7 0 10 8 10 8a18.5 18.5 0 0 1-2.16 3.19" />
                <path d="M14.12 14.12a3 3 0 1 1-4.24-4.24" />
                <path d="M17.94 17.94A10.07 10.07 0 0 1 12 20c-7 0-10-8-10-8a18.45 18.45 0 0 1 5.06-5.94" />
                <path d="M2 2l20 20" />
            </>
        },
        Icon::Refresh => html! {
            <>
                <path d="M21 12a9 9 0 0 1-15.36 6.36L3 16" />
                <path d="M3 12a9 9 0 0 1 15.36-6.36L21 8" />
                <path d="M21 3v5h-5M3 21v-5h5" />
            </>
        },
        Icon::Router => html! {
            <>
                <rect x="2" y="14" width="20" height="8" rx="2" />
                <path d="M6 18h.01M10 18h.01M15 10v4" />
                <path d="M17.84 7.17a4 4 0 0 0-5.66 0M20.66 4.34a8 8 0 0 0-11.31 0" />
            </>
        },
        Icon::Channel => html! { <path d="M4 9h16M4 15h16M10 3L8 21M16 3l-2 18" /> },
        Icon::Gauge => html! {
            <>
                <path d="M12 14l4-4" />
                <path d="M3.34 19a10 10 0 1 1 17.32 0" />
            </>
        },
        Icon::Interfaces => html! {
            <>
                <rect x="16" y="16" width="6" height="6" rx="1" />
                <rect x="2" y="16" width="6" height="6" rx="1" />
                <rect x="9" y="2" width="6" height="6" rx="1" />
                <path d="M5 16v-3a1 1 0 0 1 1-1h12a1 1 0 0 1 1 1v3M12 12V8" />
            </>
        },
        Icon::Neighbors => html! {
            <>
                <circle cx="18" cy="5" r="3" />
                <circle cx="6" cy="12" r="3" />
                <circle cx="18" cy="19" r="3" />
                <path d="M8.59 13.51l6.83 3.98M15.41 6.51l-6.82 3.98" />
            </>
        },
        Icon::Menu => html! { <path d="M4 6h16M4 12h16M4 18h16" /> },
        Icon::Close => html! { <path d="M18 6L6 18M6 6l12 12" /> },
        Icon::Logo => html! {
            <>
                <path d="M3 7V5a2 2 0 0 1 2-2h2M17 3h2a2 2 0 0 1 2 2v2M21 17v2a2 2 0 0 1-2 2h-2M7 21H5a2 2 0 0 1-2-2v-2" />
                <circle cx="12" cy="12" r="1.5" fill="currentColor" />
                <path d="M18.94 12.33a1 1 0 0 0 0-.66 7.5 7.5 0 0 0-13.88 0 1 1 0 0 0 0 .66 7.5 7.5 0 0 0 13.88 0" />
            </>
        },
        Icon::Devices => html! {
            <>
                <rect x="3" y="3" width="7" height="7" rx="1.5" />
                <rect x="14" y="3" width="7" height="7" rx="1.5" />
                <rect x="14" y="14" width="7" height="7" rx="1.5" />
                <rect x="3" y="14" width="7" height="7" rx="1.5" />
            </>
        },
        Icon::Commission => html! {
            <>
                <circle cx="12" cy="12" r="10" />
                <path d="M8 12h8M12 8v8" />
            </>
        },
        Icon::Updates => html! {
            <>
                <path d="M21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16z" />
                <path d="M3.27 6.96L12 12.01l8.73-5.05M12 22.08V12M7.5 4.21l9 5.19" />
            </>
        },
        Icon::Search => html! {
            <>
                <circle cx="11" cy="11" r="7" />
                <path d="M21 21l-4.35-4.35" />
            </>
        },
        Icon::Sync => html! {
            <>
                <path d="M17 2l4 4-4 4" />
                <path d="M3 11v-1a4 4 0 0 1 4-4h14" />
                <path d="M7 22l-4-4 4-4" />
                <path d="M21 13v1a4 4 0 0 1-4 4H3" />
            </>
        },
        Icon::Identify => html! {
            <>
                <path d="M15 14c.2-1 .7-1.7 1.5-2.5 1-.9 1.5-2.2 1.5-3.5A6 6 0 0 0 6 8c0 1 .2 2.2 1.5 3.5.7.7 1.3 1.5 1.5 2.5" />
                <path d="M9 18h6M10 22h4" />
            </>
        },
        Icon::Logs => html! {
            <>
                <path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7z" />
                <path d="M14 2v4a2 2 0 0 0 2 2h4M10 9H8M16 13H8M16 17H8" />
            </>
        },
        Icon::Remember => html! { <path d="M19 21l-7-4-7 4V5a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2z" /> },
        Icon::Tag => html! {
            <>
                <path d="M12.59 2.59A2 2 0 0 0 11.17 2H4a2 2 0 0 0-2 2v7.17a2 2 0 0 0 .59 1.42l8.7 8.7a2.43 2.43 0 0 0 3.42 0l6.58-6.58a2.43 2.43 0 0 0 0-3.42z" />
                <circle cx="7.5" cy="7.5" r="1" fill="currentColor" />
            </>
        },
        Icon::Edit => html! {
            <>
                <path d="M21.17 6.81a1 1 0 0 0-3.99-3.99L3.84 16.17a2 2 0 0 0-.5.83l-1.32 4.35a.5.5 0 0 0 .62.62l4.35-1.32a2 2 0 0 0 .83-.5z" />
                <path d="M15 5l4 4" />
            </>
        },
        Icon::Download => html! {
            <>
                <path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" />
                <path d="M7 10l5 5 5-5M12 15V3" />
            </>
        },
        Icon::Upload => html! {
            <>
                <path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" />
                <path d="M17 8l-5-5-5 5M12 3v12" />
            </>
        },
        Icon::Bluetooth => html! { <path d="M7 7l10 10-5 5V2l5 5L7 17" /> },
        Icon::QrCode => html! {
            <>
                <rect x="3" y="3" width="5" height="5" rx="1" />
                <rect x="16" y="3" width="5" height="5" rx="1" />
                <rect x="3" y="16" width="5" height="5" rx="1" />
                <path d="M21 16h-3a2 2 0 0 0-2 2v3M21 21v.01M12 7v3a2 2 0 0 1-2 2H7M3 12h.01M12 3h.01M12 16v.01M16 12h1M21 12v.01M12 21v-1" />
            </>
        },
        Icon::Key => html! {
            <>
                <circle cx="7.5" cy="15.5" r="5.5" />
                <path d="M21 2l-9.6 9.6M15.5 7.5l3 3L22 7l-3-3" />
            </>
        },
        Icon::Check => html! { <path d="M20 6L9 17l-5-5" /> },
        Icon::CheckCircle => html! {
            <>
                <circle cx="12" cy="12" r="10" />
                <path d="M9 12l2 2 4-4" />
            </>
        },
        Icon::XCircle => html! {
            <>
                <circle cx="12" cy="12" r="10" />
                <path d="M15 9l-6 6M9 9l6 6" />
            </>
        },
        Icon::Alert => html! {
            <>
                <path d="M21.73 18l-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3" />
                <path d="M12 9v4M12 17h.01" />
            </>
        },
        Icon::Info => html! {
            <>
                <circle cx="12" cy="12" r="10" />
                <path d="M12 16v-4M12 8h.01" />
            </>
        },
        Icon::Clock => html! {
            <>
                <circle cx="12" cy="12" r="10" />
                <path d="M12 6v6l4 2" />
            </>
        },
        Icon::Reboot => html! {
            <>
                <path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8" />
                <path d="M3 3v5h5" />
            </>
        },
        Icon::Timer => html! {
            <>
                <path d="M10 2h4M12 14l3-3" />
                <circle cx="12" cy="14" r="8" />
            </>
        },
        Icon::Chip => html! {
            <>
                <rect x="4" y="4" width="16" height="16" rx="2" />
                <rect x="9" y="9" width="6" height="6" rx="1" />
                <path d="M15 2v2M15 20v2M2 15h2M2 9h2M20 15h2M20 9h2M9 2v2M9 20v2" />
            </>
        },
        Icon::Globe => html! {
            <>
                <circle cx="12" cy="12" r="10" />
                <path d="M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20M2 12h20" />
            </>
        },
        Icon::Radar => html! {
            <>
                <path d="M19.07 4.93A10 10 0 0 0 6.99 3.34M4 6h.01M2.29 9.62A10 10 0 1 0 21.31 8.35" />
                <path d="M16.24 7.76A6 6 0 1 0 8.23 16.67M12 18h.01M17.99 11.66A6 6 0 0 1 15.77 16.67" />
                <circle cx="12" cy="12" r="2" />
                <path d="M13.41 10.59l5.66-5.66" />
            </>
        },
        Icon::External => html! {
            <>
                <path d="M15 3h6v6M10 14L21 3" />
                <path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" />
            </>
        },
        Icon::Camera => html! {
            <>
                <path d="M14.5 4h-5L7 7H4a2 2 0 0 0-2 2v9a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2V9a2 2 0 0 0-2-2h-3z" />
                <circle cx="12" cy="13" r="3" />
            </>
        },
        Icon::ChevronRight => html! { <path d="M9 18l6-6-6-6" /> },
        Icon::UpdateArrow => html! {
            <>
                <circle cx="12" cy="12" r="10" />
                <path d="M16 12l-4-4-4 4M12 16V8" />
            </>
        },
    };

    html! {
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"
            width={size.to_string()} height={size.to_string()} fill="none" stroke="currentColor"
            stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            { shape }
        </svg>
    }
}

/// The icon of a kind of network.
pub fn kind_icon(kind: Option<api::NetworkKind>) -> Option<Icon> {
    match kind? {
        api::NetworkKind::WiFi => Some(Icon::Wifi),
        api::NetworkKind::Thread => Some(Icon::Thread),
        api::NetworkKind::Ethernet => Some(Icon::Ethernet),
        _ => None,
    }
}

/// The color class of a kind of network.
pub fn kind_class(kind: Option<api::NetworkKind>) -> &'static str {
    match kind {
        Some(api::NetworkKind::WiFi) => "is-wifi",
        Some(api::NetworkKind::Thread) => "is-thread",
        Some(api::NetworkKind::Ethernet) => "is-ethernet",
        _ => "is-other",
    }
}
