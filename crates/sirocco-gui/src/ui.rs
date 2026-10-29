//! The view.
//!
//! A fixed configuration rail on the left; on the right a column that answers, in
//! order, "how far along", "how fast", "which stage is the constraint", and
//! "what exactly is happening". Every surface uses the same radius scale
//! (12 / 8 / 6) and the same spacing rhythm (4 / 8 / 12 / 16).

use iced::Alignment;
use iced::Color;
use iced::Element;
use iced::Font;
use iced::Length;
use iced::Theme;
use iced::widget::Space;
use iced::widget::button;
use iced::widget::canvas;
use iced::widget::checkbox;
use iced::widget::column;
use iced::widget::container;
use iced::widget::pick_list;
use iced::widget::row;
use iced::widget::scrollable;
use iced::widget::slider;
use iced::widget::text;
use iced::widget::text_input;

use sirocco_core::hash::HashKind;
use sirocco_core::plan::Mode;
use sirocco_core::plan::Order;
use sirocco_core::plan::Overwrite;
use sirocco_core::plan::VerifyMode;
use sirocco_engine::config::CompressMode;
use sirocco_engine::config::IndexMode;
use sirocco_engine::config::PreallocMode;
use sirocco_engine::config::SmallCopy;
use sirocco_sys::ReactorKind;

use crate::App;
use crate::FileRow;
use crate::HugeChoice;
use crate::Message;
use crate::NumaChoice;
use crate::Phase;
use crate::RowState;
use crate::charts;
use crate::theme as th;

const MONO: Font = Font::MONOSPACE;
const RAIL_W: f32 = 336.0;

const fn bold() -> Font {
    Font {
        weight: iced::font::Weight::Semibold,
        ..Font::DEFAULT
    }
}

/// Vertical space. A function, not a widget expression, so a stack of gaps stays
/// readable and cannot be mis-parenthesised.
fn gap(h: f32) -> Element<'static, Message> {
    Space::new().height(Length::Fixed(h)).into()
}

/// Horizontal space. The vertical twin above collapses to zero width inside a
/// row, which is how "`THROUGHPUTlast` 60 s" happened.
fn hgap(w: f32) -> Element<'static, Message> {
    Space::new().width(Length::Fixed(w)).into()
}

/// Centre a label inside a widget that stretches to fill its slot. iced's `Button`
/// lays its child out at the padding origin, so a full-width button left-aligns
/// its text unless the child fills the width itself.
fn centered<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content)
        .width(Length::Fill)
        .center_x(Length::Fill)
        .into()
}

/// Horizontal filler.
fn spacer() -> Element<'static, Message> {
    Space::new().width(Length::Fill).into()
}

/// Wrap scrollable content in a right-hand gutter.
///
/// iced draws the scrollbar *over* its content rather than beside it, so without
/// this every right-aligned column loses its last eight pixels — which is exactly
/// where a file size or a thread count lives.
fn gutter<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content)
        .padding(iced::Padding {
            top: 0.0,
            right: 10.0,
            bottom: 0.0,
            left: 0.0,
        })
        .into()
}

pub fn view(app: &App) -> Element<'_, Message> {
    container(row![rail(app), content(app)].height(Length::Fill))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(th::root)
        .into()
}

// ---------------------------------------------------------------- rail

// The rail is one column of labelled controls; splitting it would only move the
// line count somewhere else.
#[allow(clippy::too_many_lines)]
fn rail(app: &App) -> Element<'_, Message> {
    let mut col = column![
        text("SIROCCO").size(th::T_BRAND).font(Font {
            weight: iced::font::Weight::Bold,
            ..Font::DEFAULT
        }),
        gap(2.0),
        text("fast, low-level file copy").size(11).color(th::FAINT),
        gap(16.0),
        modes(app),
        gap(12.0),
        advanced_toggle(app),
    ];

    if let Some((msg, color)) = app.caveat() {
        col = col.push(gap(10.0)).push(notice(msg, color));
    }

    col = col
        .push(gap(16.0))
        .push(path_field(
            "SOURCE",
            &app.src,
            Message::Src,
            Message::BrowseSrc,
            "a folder or a file",
            !app.is_running(),
        ))
        .push(gap(12.0))
        .push(path_field(
            "DESTINATION",
            &app.dst,
            Message::Dst,
            Message::BrowseDst,
            "where it should land",
            !app.is_running(),
        ));

    // `into` only means something when the source is a folder. It is also
    // settled before the scan runs, so flipping it mid-job would change nothing
    // while looking like it did.
    if std::path::Path::new(app.src.trim()).is_dir() {
        let boxed: iced::widget::Checkbox<'_, Message> = checkbox(app.into)
            .label("Copy the folder itself into the destination")
            .size(15)
            .text_size(th::T_BODY)
            .style(th::checkbox_style);
        let boxed: Element<'_, Message> = if app.is_running() {
            boxed.into()
        } else {
            boxed.on_toggle(Message::Into).into()
        };
        col = col.push(gap(6.0)).push(boxed);
    }

    col = col
        .push(gap(14.0))
        .push(divider())
        .push(gap(14.0))
        .push(eyebrow("OPTIONS"))
        .push(gap(8.0))
        .push(toggles(app))
        .push(gap(14.0))
        .push(choice(
            "VERIFY",
            &VERIFY_OPTS,
            verify_label(app.verify),
            verify_from,
        ))
        .push(gap(12.0))
        .push(choice(
            "DIGEST",
            &HASH_OPTS,
            hash_label(app.hash),
            hash_from,
        ))
        .push(gap(12.0))
        .push(choice(
            "OVERWRITE",
            &OVERWRITE_OPTS,
            overwrite_label(app.overwrite),
            overwrite_from,
        ))
        .push(gap(12.0))
        .push(choice(
            "BLOCK INDEX",
            &INDEX_OPTS,
            index_label(app.index),
            index_from,
        ))
        .push(gap(12.0))
        .push(eyebrow("EXCLUDE"))
        .push(gap(6.0))
        .push(input_or_static(
            "*.tmp, node_modules, .git",
            &app.exclude,
            Message::Exclude,
            !app.is_running(),
        ))
        .push(gap(10.0))
        .push(eyebrow("INCLUDE"))
        .push(gap(6.0))
        .push(input_or_static(
            "leave empty for everything",
            &app.include,
            Message::Include,
            !app.is_running(),
        ))
        .push(gap(10.0))
        .push(choice(
            "SCAN ORDER",
            &ORDER_OPTS,
            order_label(app.order),
            order_from,
        ))
        .push(gap(14.0))
        .push(divider())
        .push(gap(14.0))
        .push(eyebrow("PARALLELISM"))
        .push(gap(8.0))
        .push(threads_row(app))
        .push(gap(16.0))
        .push(divider());

    if app.advanced {
        col = col
            .push(gap(12.0))
            .push(engine_group(app))
            .push(gap(14.0))
            .push(divider())
            .push(gap(14.0))
            .push(parallel_group(app))
            .push(gap(14.0))
            .push(divider())
            .push(gap(14.0))
            .push(delta_group(app))
            .push(gap(14.0))
            .push(divider())
            .push(gap(14.0))
            .push(scheduling_group(app))
            .push(gap(14.0))
            .push(divider())
            .push(gap(14.0))
            .push(fidelity_group(app))
            .push(gap(14.0))
            .push(divider())
            .push(gap(14.0))
            .push(filters_group(app))
            .push(gap(14.0))
            .push(divider())
            .push(gap(14.0))
            .push(privileges_group(app));
    }

    col = col.push(gap(16.0)).push(divider());

    // The actions live outside the scroll region: a Start button you have to
    // scroll to find is a broken Start button. A validation error belongs right
    // above it, not buried in the options.
    let mut footer = column![];
    if let Some(err) = &app.error {
        footer = footer.push(notice(err, th::ERR)).push(gap(8.0));
    }
    let mut footer = footer.push(actions(app)).push(gap(8.0)).push(
        text(format!("{} · {} cores", app.caps, app.cores))
            .size(10)
            .color(th::FAINT),
    );
    if let Some(note) = &app.priv_note {
        footer = footer
            .push(gap(3.0))
            .push(text(note).size(10).color(th::FAINT));
    }

    container(column![
        scrollable(gutter(col))
            .style(th::scroll)
            .height(Length::Fill),
        gap(14.0),
        footer
    ])
    .width(Length::Fixed(RAIL_W))
    .height(Length::Fill)
    .padding([22, 20])
    .style(th::rail)
    .into()
}

const ORDER_OPTS: [&str; 5] = [
    "directory order",
    "smallest first",
    "largest first",
    "physical layout",
    "by name",
];
const VERIFY_OPTS: [&str; 5] = ["off", "size", "size + time", "hash", "sample"];
const OVERWRITE_OPTS: [&str; 7] = [
    "if different",
    "if newer",
    "if older",
    "if size differs",
    "if content differs",
    "never",
    "always",
];

const fn verify_label(v: VerifyMode) -> &'static str {
    match v {
        | VerifyMode::None => "off",
        | VerifyMode::Size => "size",
        | VerifyMode::SizeMtime => "size + time",
        | VerifyMode::Hash => "hash",
        | VerifyMode::Sample => "sample",
    }
}

fn verify_from(s: &str) -> Message {
    Message::VerifyPicked(match s {
        | "size" => VerifyMode::Size,
        | "size + time" => VerifyMode::SizeMtime,
        | "hash" => VerifyMode::Hash,
        | "sample" => VerifyMode::Sample,
        | _ => VerifyMode::None,
    })
}

const fn overwrite_label(o: Overwrite) -> &'static str {
    match o {
        | Overwrite::IfDifferent => "if different",
        | Overwrite::IfNewer => "if newer",
        | Overwrite::IfOlder => "if older",
        | Overwrite::IfDifferentSize => "if size differs",
        | Overwrite::IfContentDiffers => "if content differs",
        | Overwrite::Never => "never",
        | Overwrite::Always => "always",
    }
}

fn order_from(s: &str) -> Message {
    Message::OrderPicked(match s {
        | "smallest first" => Order::SizeAsc,
        | "largest first" => Order::SizeDesc,
        | "physical layout" => Order::Physical,
        | "by name" => Order::Name,
        | _ => Order::Walk,
    })
}

const fn order_label(o: Order) -> &'static str {
    match o {
        | Order::Walk => "directory order",
        | Order::SizeAsc => "smallest first",
        | Order::SizeDesc => "largest first",
        | Order::Physical => "physical layout",
        | Order::Name => "by name",
    }
}

fn overwrite_from(s: &str) -> Message {
    Message::OverwritePicked(match s {
        | "if newer" => Overwrite::IfNewer,
        | "if older" => Overwrite::IfOlder,
        | "if size differs" => Overwrite::IfDifferentSize,
        | "if content differs" => Overwrite::IfContentDiffers,
        | "never" => Overwrite::Never,
        | "always" => Overwrite::Always,
        | _ => Overwrite::IfDifferent,
    })
}

fn modes(app: &App) -> Element<'_, Message> {
    row![
        mode_tab(app, Mode::Copy, "Copy"),
        mode_tab(app, Mode::Move, "Move"),
        mode_tab(app, Mode::Sync, "Mirror"),
    ]
    .spacing(6)
    .into()
}

fn mode_tab<'a>(
    app: &App,
    m: Mode,
    label: &'a str,
) -> Element<'a, Message> {
    let sel = app.mode == m;
    let mut b = button(centered(text(label).size(th::T_BODY).font(if sel {
        bold()
    } else {
        Font::DEFAULT
    })))
    .padding([7, 12])
    .width(Length::Fill)
    .style(th::tab(sel));
    if !app.is_running() {
        b = b.on_press(Message::ModePicked(m));
    }
    b.into()
}

fn eyebrow(t: &str) -> Element<'_, Message> {
    text(t)
        .size(th::T_EYEBROW)
        .color(th::FAINT)
        .font(bold())
        .into()
}

fn divider() -> Element<'static, Message> {
    container(Space::new().height(Length::Fixed(1.0)))
        .width(Length::Fill)
        .height(Length::Fixed(1.0))
        .style(th::rule)
        .into()
}

fn notice(
    msg: &str,
    color: Color,
) -> Element<'_, Message> {
    container(text(msg).size(11).color(color))
        .width(Length::Fill)
        .padding([8, 10])
        .style(th::notice(color))
        .into()
}

/// A text field that stops accepting input while a job is running, so the form
/// cannot drift away from the paths the engine is actually using.
fn input_or_static<'a>(
    hint: &'a str,
    value: &'a str,
    on: fn(String) -> Message,
    enabled: bool,
) -> Element<'a, Message> {
    let field = text_input(hint, value)
        .padding([9, 10])
        .size(th::T_BODY)
        .style(th::input);
    if enabled {
        field.on_input(on).into()
    } else {
        field.into()
    }
}

fn path_field<'a>(
    label: &'a str,
    value: &'a str,
    on: fn(String) -> Message,
    browse: Message,
    hint: &'a str,
    enabled: bool,
) -> Element<'a, Message> {
    let mut link = button(text("Browse").size(10).color(th::MUTED))
        .padding([3, 7])
        .style(th::ghost);
    if enabled {
        link = link.on_press(browse);
    }
    column![
        row![eyebrow(label), spacer(), link].align_y(Alignment::Center),
        gap(5.0),
        input_or_static(hint, value, on, enabled),
    ]
    .into()
}

fn choice<'a>(
    label: &'a str,
    options: &'a [&'a str],
    selected: &'a str,
    on: fn(&str) -> Message,
) -> Element<'a, Message> {
    column![
        eyebrow(label),
        gap(6.0),
        pick_list(options, Some(selected), on)
            .padding([7, 10])
            .text_size(th::T_BODY)
            .style(th::pick)
            .width(Length::Fill),
    ]
    .into()
}

/// `choice`, for a list computed at render time.
///
/// The list is owned rather than borrowed because it depends on another control
/// (the buffer size); `pick_list` takes it by value, so nothing dangles.
fn choice_owned<'a>(
    label: &'a str,
    options: Vec<&'a str>,
    selected: &'a str,
    on: fn(&str) -> Message,
) -> Element<'a, Message> {
    column![
        eyebrow(label),
        gap(6.0),
        pick_list(options, Some(selected), on)
            .padding([7, 10])
            .text_size(th::T_BODY)
            .style(th::pick)
            .width(Length::Fill),
    ]
    .into()
}

fn toggles(app: &App) -> Element<'_, Message> {
    let mut c = column![].spacing(6);
    for (label, value, msg) in [
        (
            "Keep hard links",
            app.links,
            Message::Links as fn(bool) -> Message,
        ),
        ("Punch holes for empty regions", app.sparse, Message::Sparse),
        ("Copy permissions and ACLs", app.acl, Message::Acl),
        ("Resume journal", app.journal, Message::Journal),
        ("Flush to disk before finishing", app.sync, Message::Sync),
        ("Plan only — change nothing", app.dry_run, Message::DryRun),
    ] {
        c = c.push(toggle_one(app, label, value, msg));
    }
    c.into()
}

/// One checkbox row. A toggle that changes nothing until the next run is a lie
/// while the current one is still going, so it loses its handler instead.
fn toggle_one<'a>(
    app: &'a App,
    label: &'a str,
    value: bool,
    on: fn(bool) -> Message,
) -> Element<'a, Message> {
    let boxed: iced::widget::Checkbox<'_, Message> = checkbox(value)
        .label(label)
        .size(15)
        .text_size(th::T_BODY)
        .style(th::checkbox_style);
    if app.is_running() {
        boxed.into()
    } else {
        boxed.on_toggle(on).into()
    }
}

/// A labelled integer slider, used where the value is a small count.
///
/// Always live, like the thread slider: a slider only writes the value the next
/// run will use, so freezing it mid-run would hide the setting without protecting
/// anything.
fn num_row(
    label: &str,
    range: std::ops::RangeInclusive<f32>,
    value: u32,
    shown: String,
    on: fn(u32) -> Message,
) -> Element<'_, Message> {
    column![
        row![
            text(label).size(th::T_BODY).color(th::MUTED),
            spacer(),
            text(shown).size(11).font(MONO).color(th::TEXT),
        ]
        .align_y(Alignment::Center),
        gap(4.0),
        slider(range, value as f32, move |v| on(v as u32))
            .step(1.0_f32)
            .style(th::slider),
    ]
    .into()
}

// ---------------------------------------------------------------- advanced

// Every label here is the same word the CLI's `--help` uses, so a user who knows
// one knows the other. The pick lists are ordered from "do the least" to "do the
// most", not alphabetically.

const HASH_OPTS: [&str; 2] = ["xxh64", "crc32c"];
const INDEX_OPTS: [&str; 3] = ["off", "trust", "verify"];
const SMALL_OPTS: [&str; 3] = ["auto", "kernel", "io"];
const HUGE_OPTS: [&str; 3] = ["auto", "on", "off"];
const DELTA_BLOCK_OPTS: [&str; 5] = ["auto", "1M", "4M", "16M", "64M"];
const PREALLOC_OPTS: [&str; 4] = ["auto", "valid", "sparse", "off"];
const COMPRESS_OPTS: [&str; 3] = ["off", "auto", "force"];
const BACKEND_OPTS: [&str; 2] = ["native", "blocking"];
const NUMA_OPTS: [&str; 3] = ["follow device", "off", "node"];
const BUFFER_OPTS: [&str; 6] = ["256K", "1M", "2M", "4M", "8M", "16M"];
const IO_PRIO_OPTS: [&str; 6] = ["default", "verylow", "low", "normal", "high", "critical"];
const PROC_PRIO_OPTS: [&str; 7] = [
    "default", "idle", "below", "normal", "above", "high", "realtime",
];
const THREAD_PRIO_OPTS: [&str; 8] = [
    "default",
    "idle",
    "lowest",
    "below",
    "normal",
    "above",
    "highest",
    "timecritical",
];

const fn hash_label(h: HashKind) -> &'static str {
    match h {
        | HashKind::Xxh64 => "xxh64",
        | HashKind::Crc32c => "crc32c",
    }
}

fn hash_from(s: &str) -> Message {
    Message::HashPicked(match s {
        | "crc32c" => HashKind::Crc32c,
        | _ => HashKind::Xxh64,
    })
}

const fn index_label(m: IndexMode) -> &'static str {
    match m {
        | IndexMode::Off => "off",
        | IndexMode::Trust => "trust",
        | IndexMode::Verify => "verify",
    }
}

fn index_from(s: &str) -> Message {
    Message::IndexPicked(match s {
        | "trust" => IndexMode::Trust,
        | "verify" => IndexMode::Verify,
        | _ => IndexMode::Off,
    })
}

const fn huge_label(h: HugeChoice) -> &'static str {
    match h {
        | HugeChoice::Auto => "auto",
        | HugeChoice::On => "on",
        | HugeChoice::Off => "off",
    }
}

fn huge_from(s: &str) -> Message {
    Message::HugePicked(match s {
        | "on" => HugeChoice::On,
        | "off" => HugeChoice::Off,
        | _ => HugeChoice::Auto,
    })
}

const fn small_label(m: SmallCopy) -> &'static str {
    match m {
        | SmallCopy::Auto => "auto",
        | SmallCopy::Kernel => "kernel",
        | SmallCopy::Io => "io",
    }
}

fn small_from(s: &str) -> Message {
    Message::SmallCopyPicked(match s {
        | "kernel" => SmallCopy::Kernel,
        | "io" => SmallCopy::Io,
        | _ => SmallCopy::Auto,
    })
}

const fn prealloc_label(m: PreallocMode) -> &'static str {
    match m {
        | PreallocMode::Auto => "auto",
        | PreallocMode::Valid => "valid",
        | PreallocMode::Sparse => "sparse",
        | PreallocMode::Off => "off",
    }
}

fn prealloc_from(s: &str) -> Message {
    Message::PreallocPicked(match s {
        | "valid" => PreallocMode::Valid,
        | "sparse" => PreallocMode::Sparse,
        | "off" => PreallocMode::Off,
        | _ => PreallocMode::Auto,
    })
}

const fn compress_label(m: CompressMode) -> &'static str {
    match m {
        | CompressMode::Off => "off",
        | CompressMode::Auto => "auto",
        | CompressMode::Force => "force",
    }
}

fn compress_from(s: &str) -> Message {
    Message::CompressPicked(match s {
        | "auto" => CompressMode::Auto,
        | "force" => CompressMode::Force,
        | _ => CompressMode::Off,
    })
}

const fn backend_label(k: ReactorKind) -> &'static str {
    match k {
        | ReactorKind::Native => "native",
        | ReactorKind::Blocking => "blocking",
    }
}

fn backend_from(s: &str) -> Message {
    Message::BackendPicked(match s {
        | "blocking" => ReactorKind::Blocking,
        | _ => ReactorKind::Native,
    })
}

const fn numa_label(n: NumaChoice) -> &'static str {
    match n {
        | NumaChoice::Auto => "follow device",
        | NumaChoice::Off => "off",
        | NumaChoice::Node => "node",
    }
}

fn numa_from(s: &str) -> Message {
    Message::NumaPicked(match s {
        | "off" => NumaChoice::Off,
        | "node" => NumaChoice::Node,
        | _ => NumaChoice::Auto,
    })
}

fn small_from_size(s: &str) -> Message {
    Message::SmallThresholdPicked(s.to_string())
}

fn delta_block_from(s: &str) -> Message {
    // "auto" is stored as empty, so `engine_config` leaves the block at its
    // default and `normalize_with` derives it from the I/O unit.
    Message::DeltaBlockPicked(if s == "auto" {
        String::new()
    } else {
        s.to_string()
    })
}

fn delta_min_from(s: &str) -> Message {
    Message::DeltaMinPicked(s.to_string())
}

/// The I/O units a given buffer size can actually use.
///
/// `normalize_with` clamps `io_unit` to `buffer_size`, so offering a larger one
/// would put a number on screen that the engine silently replaces.
fn io_unit_opts(buffer: &str) -> Vec<&'static str> {
    let limit = sirocco_core::human::parse_size(buffer).unwrap_or(1 << 20);
    BUFFER_OPTS
        .iter()
        .copied()
        .filter(|o| sirocco_core::human::parse_size(o).is_some_and(|v| v <= limit))
        .collect()
}

fn buffer_from(s: &str) -> Message {
    Message::BufferPicked(s.to_string())
}

fn io_unit_from(s: &str) -> Message {
    Message::IoUnitPicked(s.to_string())
}

fn io_prio_from(s: &str) -> Message {
    Message::IoPrioPicked(s.to_string())
}

fn proc_prio_from(s: &str) -> Message {
    Message::ProcPrioPicked(s.to_string())
}

fn thread_prio_from(s: &str) -> Message {
    Message::ThreadPrioPicked(s.to_string())
}

fn advanced_toggle(app: &App) -> Element<'_, Message> {
    let label = if app.advanced {
        "Hide advanced settings"
    } else {
        "Advanced settings"
    };
    button(centered(text(label).size(th::T_BODY)))
        .width(Length::Fill)
        .padding([8, 0])
        .style(th::ghost)
        .on_press(Message::ToggleAdvanced)
        .into()
}

/// The I/O shape: what the engine asks the device for, and how.
fn engine_group(app: &App) -> Element<'_, Message> {
    column![
        eyebrow("ENGINE"),
        gap(8.0),
        toggle_one(
            app,
            "Direct I/O — bypass the page cache",
            app.direct,
            Message::Direct
        ),
        gap(12.0),
        choice("LARGE PAGES", &HUGE_OPTS, huge_label(app.huge), huge_from),
        gap(6.0),
        toggle_one(
            app,
            "Physical order per file (spinning disks)",
            app.lcn,
            Message::Lcn
        ),
        gap(6.0),
        toggle_one(
            app,
            "Block delta and append-only updates",
            app.delta,
            Message::Delta
        ),
        gap(12.0),
        choice(
            "BACKEND",
            &BACKEND_OPTS,
            backend_label(app.backend),
            backend_from
        ),
        gap(12.0),
        choice(
            "SMALL FILES",
            &SMALL_OPTS,
            small_label(app.small_copy),
            small_from
        ),
        gap(12.0),
        choice(
            "PREALLOCATE",
            &PREALLOC_OPTS,
            prealloc_label(app.prealloc),
            prealloc_from
        ),
        gap(12.0),
        choice(
            "COMPRESSION",
            &COMPRESS_OPTS,
            compress_label(app.compress),
            compress_from
        ),
        gap(12.0),
        choice("BUFFER", &BUFFER_OPTS, &app.buffer, buffer_from),
        gap(12.0),
        choice_owned(
            "I/O UNIT",
            io_unit_opts(&app.buffer),
            &app.io_unit,
            io_unit_from
        ),
    ]
    .into()
}

/// Concurrency: how many requests are in flight, and how many threads make them.
///
/// Every count here has a "0 = let the engine decide" state, because the engine's
/// own derivation is better than a number typed without knowing the machine.
fn parallel_group(app: &App) -> Element<'_, Message> {
    column![
        eyebrow("CONCURRENCY"),
        gap(8.0),
        num_row(
            "Overlap per thread",
            1.0..=64.0,
            app.overlap,
            app.overlap.to_string(),
            Message::Overlap
        ),
        gap(8.0),
        num_row(
            "Queue depth",
            0.0..=512.0,
            app.queue,
            if app.queue == 0 {
                "auto".into()
            } else {
                app.queue.to_string()
            },
            Message::Queue
        ),
        gap(8.0),
        num_row(
            "Scheduler threads",
            0.0..=16.0,
            app.feeders,
            if app.feeders == 0 {
                "adaptive".into()
            } else {
                app.feeders.to_string()
            },
            Message::Feeders
        ),
        gap(8.0),
        num_row(
            "Explicit readers",
            0.0..=64.0,
            app.readers,
            if app.readers == 0 {
                "from threads".into()
            } else {
                app.readers.to_string()
            },
            Message::Readers
        ),
        gap(8.0),
        num_row(
            "Explicit writers",
            0.0..=64.0,
            app.writers,
            if app.writers == 0 {
                "from threads".into()
            } else {
                app.writers.to_string()
            },
            Message::Writers
        ),
        gap(12.0),
        eyebrow("SMALL-FILE TUNING"),
        gap(8.0),
        choice("SMALL THRESHOLD", &BUFFER_OPTS, &app.small, small_from_size),
        gap(8.0),
        num_row(
            "Small batch",
            1.0..=4096.0,
            app.small_batch,
            app.small_batch.to_string(),
            Message::SmallBatch
        ),
    ]
    .into()
}

/// The incremental-copy model: what it compares, how finely, and the measured
/// rates it reasons with.
fn delta_group(app: &App) -> Element<'_, Message> {
    column![
        eyebrow("DELTA"),
        gap(8.0),
        // "auto" follows the I/O unit, which is what the engine derives when the
        // block is left alone. Without it, picking 1M while the card shows 8M was
        // silently ignored.
        choice(
            "DELTA BLOCK",
            &DELTA_BLOCK_OPTS,
            app.delta_block_label(),
            delta_block_from
        ),
        gap(4.0),
        text(format!("using {}", app.effective_delta_block()))
            .size(10)
            .color(th::FAINT),
        gap(12.0),
        choice(
            "DELTA MIN SIZE",
            &BUFFER_OPTS,
            &app.delta_min,
            delta_min_from
        ),
        gap(8.0),
        num_row(
            "Probes per side",
            1.0..=256.0,
            app.probes,
            app.probes.to_string(),
            Message::Probes
        ),
        gap(8.0),
        calibrate_row(app),
    ]
    .into()
}

/// The calibration button, its result, and why it exists.
fn calibrate_row(app: &App) -> Element<'_, Message> {
    let label = if app.calibrating {
        "Measuring…"
    } else {
        "Measure throughput"
    };
    let button = button(centered(text(label).size(th::T_BODY)))
        .width(Length::Fill)
        .padding([7, 0])
        .style(th::ghost);
    let button = if app.calibrating || app.is_running() || app.dst.trim().is_empty() {
        button
    } else {
        button.on_press(Message::Calibrate)
    };
    let note = if app.calibration.is_empty() {
        "Feeds the delta cost model with measured rates. Without it the model assumes equal-speed media and rules delta out."
            .to_string()
    } else {
        app.calibration.clone()
    };
    column![button, gap(5.0), text(note).size(10).color(th::FAINT),].into()
}

/// Where the work runs: which CPUs, which NUMA node, at what priority.
fn scheduling_group(app: &App) -> Element<'_, Message> {
    let mut col = column![
        eyebrow("SCHEDULING"),
        gap(8.0),
        // A CPU list only takes effect through the affinity planner, so the
        // engine pins whenever one is set. Showing `app.pin` alone would make the
        // checkbox disagree with the config it produces.
        toggle_one(
            app,
            if app.cpu_list.trim().is_empty() {
                "Pin I/O threads to disjoint cores"
            } else {
                "Pin I/O threads to disjoint cores (implied by the CPU list)"
            },
            app.pin || !app.cpu_list.trim().is_empty(),
            Message::Pin
        ),
        gap(10.0),
        eyebrow("CPU LIST"),
        gap(6.0),
        input_or_static(
            "0-3,8-11 — empty for every CPU",
            &app.cpu_list,
            Message::CpuList,
            !app.is_running()
        ),
        gap(12.0),
        choice("NUMA", &NUMA_OPTS, numa_label(app.numa), numa_from),
    ];
    if app.numa == NumaChoice::Node {
        col = col.push(gap(10.0)).push(num_row(
            "NUMA node",
            0.0..=15.0,
            app.numa_node,
            app.numa_node.to_string(),
            Message::NumaNode,
        ));
    }
    col.push(gap(12.0))
        .push(choice(
            "I/O PRIORITY",
            &IO_PRIO_OPTS,
            &app.io_prio,
            io_prio_from,
        ))
        .push(gap(12.0))
        .push(choice(
            "PROCESS PRIORITY",
            &PROC_PRIO_OPTS,
            &app.proc_prio,
            proc_prio_from,
        ))
        .push(gap(12.0))
        .push(choice(
            "THREAD PRIORITY",
            &THREAD_PRIO_OPTS,
            &app.thread_prio,
            thread_prio_from,
        ))
        .into()
}

/// What survives the copy besides the bytes.
fn fidelity_group(app: &App) -> Element<'_, Message> {
    column![
        eyebrow("FIDELITY"),
        gap(8.0),
        toggle_one(
            app,
            "Copy permission bits and attributes",
            app.perms,
            Message::Perms
        ),
        gap(6.0),
        toggle_one(app, "Copy extended attributes", app.xattrs, Message::Xattrs),
        gap(6.0),
        toggle_one(
            app,
            "Keep modification times",
            app.keep_times,
            Message::KeepTimes
        ),
        gap(12.0),
        eyebrow("INDEX DIRECTORY"),
        gap(6.0),
        // A directory nothing writes to is a promise the run does not keep.
        input_or_static(
            if app.index == IndexMode::Off {
                "unused while BLOCK INDEX is off"
            } else {
                "empty for the per-user cache"
            },
            &app.index_dir,
            Message::IndexDir,
            !app.is_running() && app.index != IndexMode::Off
        ),
    ]
    .into()
}

/// What gets scanned, and what a symlink means.
fn filters_group(app: &App) -> Element<'_, Message> {
    column![
        eyebrow("FILTERS"),
        gap(8.0),
        toggle_one(
            app,
            "Follow symbolic links",
            app.follow_links,
            Message::FollowLinks
        ),
        gap(6.0),
        toggle_one(
            app,
            "Stay on the source filesystem",
            app.one_fs,
            Message::OneFs
        ),
        // The engine honours `delete_extra` only in Sync, where it is always on.
        // A checkbox that cannot be turned off is worse than a sentence.
        gap(6.0),
        text(if app.mode == Mode::Sync {
            "Mirror deletes destination files the source does not have."
        } else {
            "Extra files are only pruned in Mirror mode."
        })
        .size(10)
        .color(th::FAINT),
        gap(12.0),
        eyebrow("EXCLUDE FILE"),
        gap(6.0),
        input_or_static(
            "one glob per line, # for comments",
            &app.exclude_from,
            Message::ExcludeFrom,
            !app.is_running()
        ),
        gap(10.0),
        eyebrow("INCLUDE FILE"),
        gap(6.0),
        input_or_static(
            "one glob per line, # for comments",
            &app.include_from,
            Message::IncludeFrom,
            !app.is_running()
        ),
        gap(10.0),
        num_row(
            "Depth limit",
            0.0..=64.0,
            app.max_depth,
            if app.max_depth == 0 {
                "unlimited".into()
            } else {
                app.max_depth.to_string()
            },
            Message::MaxDepth
        ),
        gap(12.0),
        eyebrow("FILE LIST"),
        gap(6.0),
        input_or_static(
            "a file of paths, one per line",
            &app.files_from,
            Message::FilesFrom,
            !app.is_running()
        ),
    ]
    .into()
}

/// The privileged paths, and the switch that asks for them.
fn privileges_group(app: &App) -> Element<'_, Message> {
    column![
        eyebrow("PRIVILEGES"),
        gap(8.0),
        toggle_one(
            app,
            "Request privileges before starting",
            app.privileged,
            Message::Privileged
        ),
        gap(6.0),
        text(
            "Volume management, page locking and backup semantics. Without them the \
             engine falls back to the unprivileged paths, which are slower, not wrong."
        )
        .size(10)
        .color(th::FAINT),
    ]
    .into()
}

fn threads_row(app: &App) -> Element<'_, Message> {
    // While a job is running the row describes that job, not the form: the slider
    // is live on purpose, and two thread counts on one screen with nothing saying
    // which one is in force is worse than either.
    let shown = if app.is_running() {
        format!("running · {}+{}", app.last_readers, app.last_writers)
    } else {
        // The same pair the config gets, not the even split: with an explicit
        // count on either side the split is not what runs.
        let (r, w) = app.thread_pair();
        if r == 0 && w == 0 {
            let (sr, sw) = app.resolved_threads();
            format!("auto · {sr}+{sw}")
        } else {
            format!("{r}+{w}")
        }
    };
    column![
        row![
            text("I/O threads").size(th::T_BODY).color(th::MUTED),
            spacer(),
            text(shown).size(11).font(MONO).color(th::TEXT),
        ]
        .align_y(Alignment::Center),
        gap(4.0),
        slider(0.0..=64.0_f32, app.threads as f32, Message::Threads)
            .step(1.0_f32)
            .style(th::slider),
    ]
    .into()
}

fn actions(app: &App) -> Element<'_, Message> {
    let running = app.is_running();
    let (label, style) = if running {
        (
            "Stop",
            th::danger as fn(&Theme, button::Status) -> button::Style,
        )
    } else {
        (
            match app.mode {
                | Mode::Move => "Start move",
                | Mode::Sync => "Start mirror",
                | _ if app.dry_run => "Preview",
                | _ => "Start copy",
            },
            th::primary as fn(&Theme, button::Status) -> button::Style,
        )
    };
    let primary = button(centered(text(label).size(13).font(bold())))
        .width(Length::Fill)
        .padding([11, 0])
        .style(style);
    let primary = if running {
        primary.on_press(Message::Cancel)
    } else {
        primary.on_press(Message::Start)
    };

    // Clearing the feed mid-run would desynchronise the view from the engine, so
    // the button is simply unavailable while a job owns the list.
    let clear = button(centered(text("Clear").size(th::T_BODY)))
        .width(Length::Fixed(78.0))
        .padding([11, 0])
        .style(th::ghost);
    let clear = if running {
        clear
    } else {
        clear.on_press(Message::Clear)
    };

    row![primary, clear].spacing(8).into()
}

// ---------------------------------------------------------------- content

fn content(app: &App) -> Element<'_, Message> {
    let mut col = column![
        header(app),
        gap(14.0),
        stats_row(app),
        gap(12.0),
        progress_card(app),
        gap(12.0),
        rate_card(app),
        gap(12.0),
        feed_card(app),
    ];

    if let Some(sum) = &app.summary {
        if !sum.errors.is_empty() {
            col = col.push(gap(10.0)).push(error_list(sum));
        }
    } else if let Some(err) = &app.error {
        // A run that dies before producing a summary used to leave the whole
        // right-hand side at zero with the only explanation under the Start
        // button on the far left.
        col = col.push(gap(10.0)).push(notice(err, th::ERR));
    }

    container(col)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding([22, 24])
        .into()
}

fn header(app: &App) -> Element<'_, Message> {
    let (label, color) = match app.phase {
        | Phase::Idle => ("IDLE", th::FAINT),
        | Phase::Running => ("RUNNING", th::ACCENT),
        | Phase::Done => ("DONE", th::OK),
        | Phase::Failed => ("FAILED", th::ERR),
        | Phase::Cancelled => ("STOPPED", th::WARN),
    };
    let pill = container(text(label).size(th::T_EYEBROW).font(bold()).color(color))
        .padding([4, 10])
        .style(th::chip(color));

    row![
        text(&app.status).size(th::T_TITLE).color(th::TEXT),
        spacer(),
        pill,
    ]
    .align_y(Alignment::Center)
    .into()
}

fn error_list(sum: &sirocco_engine::session::Summary) -> Element<'_, Message> {
    let mut c = column![
        // This counts error *lines*: a failed mkdir lands here without being a
        // failed file, and a retried file can produce two lines. The FILES card
        // owns the file count.
        text(format!("{} error(s)", sum.errors.len()))
            .size(11)
            .color(th::ERR)
            .font(bold())
    ]
    .spacing(2);
    for e in sum.errors.iter().take(4) {
        c = c.push(text(truncate(e, 110)).size(10).font(MONO).color(th::MUTED));
    }
    if sum.errors.len() > 4 {
        c = c.push(
            text(format!("… and {} more", sum.errors.len() - 4))
                .size(10)
                .color(th::FAINT),
        );
    }
    container(c)
        .width(Length::Fill)
        .padding([8, 10])
        .style(th::notice(th::ERR))
        .into()
}

// ---------------------------------------------------------------- stat cards

fn stat_card(
    title: &str,
    value: String,
    unit: String,
    color: Color,
) -> Element<'static, Message> {
    container(
        column![
            text(title.to_string())
                .size(th::T_EYEBROW)
                .color(th::FAINT)
                .font(bold()),
            gap(6.0),
            row![
                text(value).size(th::T_VALUE).font(MONO).color(color),
                text(unit).size(11).color(th::FAINT),
            ]
            .align_y(Alignment::End)
            .spacing(4),
        ]
        .spacing(0),
    )
    .width(Length::Fill)
    .padding(th::PAD_CARD)
    .style(th::card)
    .into()
}

fn stats_row(app: &App) -> Element<'_, Message> {
    let live = app.snap().or_else(|| app.summary.as_ref().map(|x| x.stats));
    let idle = live.is_none();

    // While running, the card and the chart must agree: both read the
    // instantaneous rate. Once the run is over there is no "instantaneous" left,
    // and the last 90 ms sample would freeze the biggest number on screen at
    // whatever the final partial tick happened to catch.
    let speed = if idle {
        app.summary.as_ref().map_or(0.0, |s| {
            if s.stats.elapsed_secs > 0.0 {
                s.stats.bytes_accounted() as f64 / s.stats.elapsed_secs
            } else {
                0.0
            }
        })
    } else {
        f64::from(app.history.last().copied().unwrap_or(0.0))
    };

    let (done, total, eta, elapsed) = if app.is_preview() {
        let sum = app.summary.as_ref().unwrap();
        (
            0,
            sum.files_planned,
            "--".into(),
            crate::human_time(sum.stats.elapsed_secs),
        )
    } else {
        live.map_or_else(
            || (0, 0, "--".into(), "--".into()),
            |s| {
                (
                    // Failed files are settled as well: leaving them out made the
                    // card stop at "97 / 100" on a run that had finished.
                    s.files_done + s.files_skipped + s.files_failed,
                    s.files_total,
                    // A remaining time on a finished job is a lie; the frozen
                    // summary keeps whatever it was at the last tick.
                    if app.is_running() {
                        s.eta_secs().map_or_else(|| "--".into(), crate::human_time)
                    } else {
                        "--".into()
                    },
                    crate::human_time(s.elapsed_secs),
                )
            },
        )
    };

    // An empty screen should not shout: the accent only appears once there is a
    // number worth looking at.
    let (speed_c, num_c) = if idle {
        (th::FAINT, th::FAINT)
    } else {
        (th::ACCENT, th::TEXT)
    };

    row![
        stat_card(
            "SPEED",
            crate::human_bytes(speed as u64),
            "/s".into(),
            speed_c
        ),
        stat_card("FILES", format!("{done}"), format!("/ {total}"), num_c),
        stat_card("ELAPSED", elapsed, String::new(), num_c),
        stat_card("REMAINING", eta, String::new(), th::MUTED),
    ]
    .spacing(12)
    .into()
}

// ---------------------------------------------------------------- progress

fn progress_card(app: &App) -> Element<'_, Message> {
    let s = app.snap().or_else(|| app.summary.as_ref().map(|x| x.stats));

    // Counters stay at zero until the walk finishes, and `progress()` reports 1.0
    // for an empty plan. Painting "100.0 %" over a scan that has not produced a
    // single number yet would be the worst kind of lie.
    let scanning = app.is_running() && s.is_none_or(|s| s.bytes_total == 0 && s.files_total == 0);

    let (pct, done, total, failed) = match &s {
        | Some(_) if app.is_preview() => {
            let sum = app.summary.as_ref().unwrap();
            (0.0, 0, sum.bytes_planned, 0)
        },
        | Some(_) if scanning => (0.0, 0, 0, 0),
        // A mirror that finds everything already current writes no bytes, so the
        // byte ratio would read 0 % on a job that is completely finished. When
        // every planned file has been accounted for, that is 100 %.
        | Some(s)
            if !app.is_running()
                && s.files_total > 0
                && s.files_done + s.files_skipped >= s.files_total =>
        {
            (100.0, s.bytes_total, s.bytes_total, s.files_failed)
        },
        | Some(s) => {
            (
                // The files-only branch of `progress()` is not clamped, so a tree
                // of zero-byte files could print 112.5 %.
                (s.progress() * 100.0).clamp(0.0, 100.0),
                s.bytes_accounted(),
                s.bytes_total,
                s.files_failed,
            )
        },
        | None => (0.0, 0, 0, 0),
    };

    container(
        column![
            row![
                eyebrow("PROGRESS"),
                spacer(),
                if scanning {
                    text("scanning…").size(11).color(th::MUTED)
                } else if app.is_preview() {
                    text("preview").size(11).color(th::WARN)
                } else {
                    text(format!("{pct:.1}%"))
                        .size(11)
                        .font(MONO)
                        .color(th::TEXT)
                },
            ]
            .align_y(Alignment::Center),
            gap(10.0),
            canvas(charts::Progress {
                done: done as f32,
                total: total as f32,
                height: 10.0,
            })
            .width(Length::Fill)
            .height(Length::Fixed(10.0)),
            gap(8.0),
            row![
                text(format!(
                    "{} of {}",
                    crate::human_bytes(done),
                    crate::human_bytes(total)
                ))
                .size(11)
                .color(th::MUTED),
                spacer(),
                if failed > 0 {
                    text(format!("{failed} failed")).size(11).color(th::ERR)
                } else {
                    text("").size(11)
                },
            ]
            .align_y(Alignment::Center),
        ]
        .spacing(0),
    )
    .width(Length::Fill)
    .padding(th::PAD_CARD)
    .style(th::card)
    .into()
}

// ---------------------------------------------------------------- rate + pipeline

fn rate_card(app: &App) -> Element<'_, Message> {
    let peak = app.history.iter().copied().fold(0.0f32, f32::max);
    let (readers, writers, rbusy, wbusy, queued, cap) = app.gauges();

    let r_util = ratio(rbusy, readers);
    let w_util = ratio(wbusy, writers);
    let q_util = ratio(queued, cap);

    // Which stage is the constraint? That is the one question a throughput number
    // cannot answer, and the reason this diagram exists at all.
    let pipeline_quiet = rbusy + wbusy + queued == 0;
    let moving = app
        .snap()
        .is_some_and(|s| s.bytes_written + s.bytes_holes > 0);

    let (verdict, verdict_color) = if !app.is_running() {
        ("idle".to_string(), th::FAINT)
    } else if pipeline_quiet && moving {
        // Small files go straight to CopyFile2 / copy_file_range, which never
        // touches the chunk pipeline. Saying so beats showing three empty meters.
        ("small files · kernel path".to_string(), th::OK)
    } else if w_util >= 0.9 && q_util > 0.35 {
        ("destination-bound".to_string(), th::WARN)
    } else if r_util >= 0.9 && q_util < 0.25 {
        ("source-bound".to_string(), th::WARN)
    } else if r_util < 0.5 && w_util < 0.5 && q_util < 0.5 {
        ("metadata-bound".to_string(), th::MUTED)
    } else {
        ("flowing".to_string(), th::OK)
    };

    let read_hot = r_util >= 0.9 && q_util < 0.25;
    let write_hot = w_util >= 0.9 && q_util > 0.35;

    container(
        column![
            row![
                eyebrow("THROUGHPUT"),
                hgap(6.0),
                text("last 60 s").size(10).color(th::FAINT),
                spacer(),
                text(format!("peak {}", crate::human_rate(f64::from(peak))))
                    .size(10)
                    .font(MONO)
                    .color(th::FAINT),
            ]
            .align_y(Alignment::Center),
            gap(8.0),
            canvas(charts::Sparkline {
                samples: &app.history,
                accent: th::ACCENT,
            })
            .width(Length::Fill)
            .height(Length::Fixed(72.0)),
            gap(12.0),
            divider(),
            gap(10.0),
            row![
                eyebrow("PIPELINE"),
                spacer(),
                text(verdict).size(10).color(verdict_color),
            ]
            .align_y(Alignment::Center),
            gap(8.0),
            meter_row(
                "read",
                r_util,
                format!("{rbusy} / {readers}"),
                th::ACCENT,
                read_hot
            ),
            gap(5.0),
            meter_row(
                "queue",
                q_util,
                format!("{queued} / {cap}"),
                th::ACCENT_SOFT,
                false
            ),
            gap(5.0),
            meter_row(
                "write",
                w_util,
                format!("{wbusy} / {writers}"),
                th::OK,
                write_hot
            ),
        ]
        .spacing(0),
    )
    .width(Length::Fill)
    .padding(th::PAD_CARD)
    .style(th::card)
    .into()
}

fn ratio(
    a: usize,
    b: usize,
) -> f32 {
    if b == 0 {
        0.0
    } else {
        (a as f32 / b as f32).clamp(0.0, 1.0)
    }
}

fn meter_row(
    label: &str,
    util: f32,
    value: String,
    color: Color,
    hot: bool,
) -> Element<'static, Message> {
    row![
        text(label.to_string())
            .size(10)
            .color(th::MUTED)
            .width(Length::Fixed(40.0)),
        canvas(charts::Meter { value: util, color })
            .width(Length::Fill)
            .height(Length::Fixed(6.0)),
        text(value)
            .size(10)
            .font(MONO)
            .color(if hot { th::WARN } else { th::FAINT })
            .width(Length::Fixed(72.0))
            .align_x(Alignment::End),
    ]
    .align_y(Alignment::Center)
    .spacing(10)
    .into()
}

// ---------------------------------------------------------------- feed

fn feed_card(app: &App) -> Element<'_, Message> {
    let mut list = column![];

    // "Nothing yet -- press Start" is onboarding text. After a run that copied
    // nothing (an empty source, or everything filtered out) it reads as if the
    // job never started.
    if app.active.is_empty() && app.feed.is_empty() && app.phase == Phase::Idle {
        list = list.push(
            container(
                column![
                    text("Nothing yet").size(th::T_BODY).color(th::MUTED),
                    gap(3.0),
                    text("Drop a folder on the window, or type a path, then press Start.")
                        .size(11)
                        .color(th::FAINT),
                ]
                .spacing(0),
            )
            .padding([20, 4])
            .width(Length::Fill)
            .center_x(Length::Fill),
        );
    }

    if !app.active.is_empty() {
        list = list.push(group_label(&format!("IN FLIGHT · {}", app.active.len())));
        for f in &app.active {
            list = list.push(file_row(f));
        }
    }
    if !app.feed.is_empty() {
        list = list.push(group_label("RECENT"));
        for f in &app.feed {
            list = list.push(file_row(f));
        }
    }

    container(
        column![
            row![
                eyebrow("FILES"),
                spacer(),
                text(format!("{}", app.shown_count()))
                    .size(10)
                    .font(MONO)
                    .color(th::FAINT),
            ]
            .align_y(Alignment::Center),
            gap(6.0),
            scrollable(gutter(list))
                .style(th::scroll)
                .height(Length::Fill)
                .anchor_top(),
        ]
        .spacing(0),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .padding(th::PAD_CARD)
    .style(th::card)
    .into()
}

fn group_label(t: &str) -> Element<'static, Message> {
    container(text(t.to_string()).size(9).color(th::FAINT).font(bold()))
        .width(Length::Fill)
        .padding(iced::Padding {
            top: 8.0,
            right: 0.0,
            bottom: 4.0,
            left: 0.0,
        })
        .into()
}

fn file_row(f: &FileRow) -> Element<'_, Message> {
    let (badge_text, color) = match &f.state {
        | RowState::Active => ("RUN", th::ACCENT),
        | RowState::Done => ("OK", th::OK),
        | RowState::Skipped => ("SKIP", th::FAINT),
        | RowState::Failed(_) => ("FAIL", th::ERR),
    };
    let size = if f.size > 0 {
        crate::human_bytes(f.size)
    } else {
        String::new()
    };

    let head = row![
        badge(badge_text, color),
        text(truncate(&f.rel, 78))
            .size(11)
            .color(if matches!(f.state, RowState::Skipped) {
                th::MUTED
            } else {
                th::TEXT
            })
            .width(Length::Fill),
        text(size)
            .size(10)
            .font(MONO)
            .color(th::FAINT)
            .width(Length::Fixed(70.0))
            .align_x(Alignment::End),
    ]
    .align_y(Alignment::Center)
    .spacing(10);

    match &f.state {
        | RowState::Failed(e) => {
            column![
                container(head).padding([5, 0]),
                container(text(truncate(e, 120)).size(10).color(th::ERR)).padding(iced::Padding {
                    top: 0.0,
                    right: 0.0,
                    bottom: 6.0,
                    left: 52.0,
                }),
            ]
            .spacing(0)
            .into()
        },
        | _ => container(head).padding([5, 0]).into(),
    }
}

fn badge(
    label: &str,
    color: Color,
) -> Element<'static, Message> {
    container(text(label.to_string()).size(9).font(bold()).color(color))
        .width(Length::Fixed(42.0))
        .padding([2, 0])
        .center_x(Length::Fixed(42.0))
        .style(th::chip(color))
        .into()
}

/// Middle-ellipsis: the root and the file name both stay readable, which is what
/// you actually need to identify a path in a feed.
// The slices are bounded by the `n <= max` check above them.
#[allow(clippy::indexing_slicing)]
fn truncate(
    s: &str,
    max: usize,
) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let head = keep / 3;
    let tail = keep - head;
    let chars: Vec<char> = s.chars().collect();
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(chars[n - tail..].iter());
    out
}
