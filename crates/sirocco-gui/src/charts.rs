//! Canvas drawings: throughput history, the progress pill, and the three
//! pipeline meters.
//!
//! Everything here is plain 2D canvas — no chart crate. Each widget answers one
//! question and nothing else: "is the rate climbing", "how far along", "which
//! stage is saturated".

use iced::Color;
use iced::Point;
use iced::Rectangle;
use iced::Renderer;
use iced::Size;
use iced::Theme;
use iced::mouse;
use iced::widget::canvas::Frame;
use iced::widget::canvas::Geometry;
use iced::widget::canvas::Path;
use iced::widget::canvas::Stroke;
use iced::widget::canvas::{
    self,
};

use crate::theme;

fn px(v: f32) -> iced::Pixels {
    iced::Pixels::from(v)
}

// ---------------------------------------------------------------- sparkline

/// Rolling throughput, filled underneath, with a dot on "now".
pub struct Sparkline<'a> {
    /// Bytes per second, oldest first.
    pub samples: &'a [f32],
    pub accent: Color,
}

impl canvas::Program<crate::Message> for Sparkline<'_> {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let w = bounds.width;
        let h = bounds.height;
        if w < 4.0 || h < 4.0 {
            return vec![frame.into_geometry()];
        }

        // Gridlines first, so the data sits on top of them.
        for i in 1..=3 {
            let y = h * i as f32 / 4.0;
            frame.fill_rectangle(
                Point::new(0.0, y),
                Size::new(w, 1.0),
                Color {
                    a: 0.10,
                    ..theme::TEXT
                },
            );
        }

        if self.samples.len() < 2 {
            // A flat baseline still says "nothing has happened yet", which is
            // more honest than an empty rectangle.
            frame.fill_rectangle(
                Point::new(0.0, h - 1.5),
                Size::new(w, 1.5),
                Color {
                    a: 0.25,
                    ..theme::FAINT
                },
            );
            return vec![frame.into_geometry()];
        }

        let peak = self.samples.iter().copied().fold(1.0f32, f32::max).max(1.0);
        let scale = (h - 8.0) / peak;
        let step = w / (self.samples.len() - 1) as f32;
        let at = |i: usize, v: f32| Point::new(i as f32 * step, v.mul_add(-scale, h - 1.5));

        let area = Path::new(|p| {
            p.move_to(Point::new(0.0, h));
            for (i, v) in self.samples.iter().enumerate() {
                p.line_to(at(i, *v));
            }
            p.line_to(Point::new(w, h));
            p.close();
        });
        frame.fill(
            &area,
            Color {
                a: 0.11,
                ..self.accent
            },
        );

        // The length was checked above, so the first sample exists.
        #[allow(clippy::indexing_slicing)]
        let line = Path::new(|p| {
            p.move_to(at(0, self.samples[0]));
            for (i, v) in self.samples.iter().enumerate().skip(1) {
                p.line_to(at(i, *v));
            }
        });
        frame.stroke(
            &line,
            Stroke::default().with_color(self.accent).with_width(1.6),
        );

        let last = at(self.samples.len() - 1, *self.samples.last().unwrap());
        frame.fill(&Path::circle(last, 3.0), self.accent);
        frame.stroke(
            &Path::circle(last, 3.0),
            Stroke::default().with_color(theme::BG).with_width(1.5),
        );
        vec![frame.into_geometry()]
    }
}

// ---------------------------------------------------------------- progress

/// The main progress bar: a rounded track and a pill for the completed bytes.
///
/// There is deliberately no "failed" segment. The engine reports failures per
/// file, not per byte, so any red sliver would be an invented number — and drawn
/// over the tail of the accent pill it would also hide real progress. The count
/// belongs in the caption, where it can be exact.
pub struct Progress {
    pub done: f32,
    pub total: f32,
    pub height: f32,
}

impl canvas::Program<crate::Message> for Progress {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let w = bounds.width;
        let h = self.height.min(bounds.height).max(4.0);
        let r = h / 2.0;

        frame.fill(
            &Path::rounded_rectangle(Point::new(0.0, 0.0), Size::new(w, h), r.into()),
            theme::PANEL_HI,
        );
        if self.total <= 0.0 || w <= 1.0 {
            return vec![frame.into_geometry()];
        }

        let done = (self.done / self.total).clamp(0.0, 1.0) * w;

        // One pill, not a rectangle inside a rounded track: the fill has to share
        // the track's curvature or the corners look broken.
        if done > 0.5 {
            frame.fill(
                &Path::rounded_rectangle(Point::new(0.0, 0.0), Size::new(done.max(h), h), r.into()),
                theme::ACCENT,
            );
        }
        vec![frame.into_geometry()]
    }
}

// ---------------------------------------------------------------- meter

/// A thin utilisation bar for one pipeline stage.
pub struct Meter {
    /// 0.0..=1.0
    pub value: f32,
    pub color: Color,
}

impl canvas::Program<crate::Message> for Meter {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let w = bounds.width;
        let h = bounds.height.max(3.0);
        let r = h / 2.0;

        frame.fill(
            &Path::rounded_rectangle(Point::new(0.0, 0.0), Size::new(w, h), r.into()),
            theme::PANEL_HI,
        );
        let v = self.value.clamp(0.0, 1.0);
        if v > 0.001 {
            // A sliver of fill must still look like a bar, not a dot.
            let fw = (v * w).max(h);
            frame.fill(
                &Path::rounded_rectangle(Point::new(0.0, 0.0), Size::new(fw, h), r.into()),
                self.color,
            );
        }
        let _ = px(0.0);
        vec![frame.into_geometry()]
    }
}
