//! The throughput chart (TODO 7), in retained XAML shapes: Reactor's Canvas with a Line per segment and an Ellipse per error mark. One
//! series (a 2 px line, the speed over the last seconds of the test's own time; the seal's work and a step's sealed first pass in the
//! seal's colour, and a gap where there are no figures, TODO 109), hairline gridlines at the
//! axis's quarters, the y axis rounded up from the samples' largest (`Series::axis_top`), a "60 s ago .. now" x axis (120 samples at
//! 500 ms), a thin labelled line where each stage began (`C1 T5 Mem-MovingInv256`), and at each sample where the run's errors rose a mark:
//! a thin vertical line in the palette's error red, with a dot on the series.

use windows_reactor::*;

use crate::model::{SAMPLES, Sample, Series, axis_label};
use crate::palette::{Palette, brush};

pub const HEIGHT: f64 = 200.0;
/// Seconds ago, with the label shown under that point.
const X_TICKS: [(f64, &str); 5] = [(60.0, "60 s ago"), (45.0, "45 s"), (30.0, "30 s"), (15.0, "15 s"), (0.0, "now")];
const LEFT: f64 = 40.0;
const RIGHT: f64 = 6.0;
const TOP: f64 = 8.0;
const BOTTOM: f64 = 22.0;
const SECONDS_PER_SAMPLE: f64 = 0.5;
const SECONDS_SHOWN: f64 = (SAMPLES - 1) as f64 * SECONDS_PER_SAMPLE;
const DOT: f64 = 7.0;
/// A stage's label goes in the room up to the next stage's line, and only when this much fits
const LABEL_MIN: f64 = 40.0;

struct Plot {
    left: f64,
    top: f64,
    width: f64,
    height: f64,
    y_max: f64,
}

impl Plot {
    fn new(width: f64, height: f64, y_max: f64) -> Self {
        Self { left: LEFT, top: TOP, width: (width - LEFT - RIGHT).max(1.0), height: (height - TOP - BOTTOM).max(1.0), y_max }
    }

    fn y(&self, value: f64) -> f64 {
        self.top + self.height * (1.0 - value.clamp(0.0, self.y_max) / self.y_max)
    }

    /// x for the sample `seconds_ago` before the newest one.
    fn x_ago(&self, seconds_ago: f64) -> f64 {
        self.left + self.width * (1.0 - seconds_ago / SECONDS_SHOWN)
    }

    fn x_samples_ago(&self, samples_ago: usize) -> f64 {
        self.x_ago(samples_ago as f64 * SECONDS_PER_SAMPLE)
    }

    fn right(&self) -> f64 {
        self.left + self.width
    }

    fn bottom(&self) -> f64 {
        self.top + self.height
    }
}

// Keys for the Canvas children. Segments are keyed by position (an update moves coordinates); marks by sample number (a mark keeps its
// native element while it scrolls left).
const KEY_GRID: u64 = 1000;
const KEY_Y_LABEL: u64 = 2000;
const KEY_X_LABEL: u64 = 3000;
const KEY_MARK_LINE: u64 = 1 << 40;
const KEY_MARK_DOT: u64 = 1 << 41;
const KEY_STAGE_LINE: u64 = 1 << 42;
const KEY_STAGE_LABEL: u64 = 1 << 43;
const KEY_LONE: u64 = 1 << 44;
/// A reading with a gap on each side has no segment, so it is drawn as a dash this wide
const LONE: f64 = 6.0;

/// The chart, `width` wide: one Line per segment, kept by position key, so an update only changes coordinates. Nothing renders between
/// updates.
pub fn shapes_chart(series: &Series, width: f64, p: &Palette) -> View {
    let plot = Plot::new(width, HEIGHT, series.axis_top());
    let mut children: Vec<(u64, View)> = Vec::with_capacity(SAMPLES + 32);

    for k in 0..5u64 {
        let value = plot.y_max * k as f64 / 4.0;
        let y = plot.y(value);
        children.push((
            KEY_GRID + k,
            Line::new().x1(plot.left).y1(y).x2(plot.right()).y2(y).stroke(brush(p.chart_grid)).stroke_thickness(1.0).into(),
        ));
        // Canvas does not align, and TextBlock has no TextAlignment in reactor 0.100.0, so a fixed-width Grid right-aligns the label.
        children.push((
            KEY_Y_LABEL + k,
            Grid::new().width(LEFT - 6.0).canvas_left(0.0).canvas_top(y - 8.0).children((TextBlock::new()
                .text(axis_label(value))
                .font_size(11.0)
                .foreground(brush(p.text_secondary))
                .horizontal_alignment(HorizontalAlignment::Right),)),
        ));
    }

    let last = X_TICKS.len() - 1;
    for (k, &(ago, label)) in X_TICKS.iter().enumerate() {
        let x = plot.x_ago(ago);
        let (left, align) = match k {
            0 => (x, HorizontalAlignment::Left),
            k if k == last => (x - 60.0, HorizontalAlignment::Right),
            _ => (x - 30.0, HorizontalAlignment::Center),
        };
        children.push((
            KEY_X_LABEL + k as u64,
            Grid::new().width(60.0).canvas_left(left).canvas_top(plot.bottom() + 4.0).children((TextBlock::new()
                .text(label)
                .font_size(11.0)
                .foreground(brush(p.text_secondary))
                .horizontal_alignment(align),)),
        ));
    }

    // Where each stage began: a thin line, and its label at the top, in the room to the next line, under everything else
    let stages: Vec<(u64, f64, &str)> = series.stages().map(|(sample, ago, label)| (sample, plot.x_samples_ago(ago), label)).collect();
    for (k, &(sample, x, label)) in stages.iter().enumerate() {
        children.push((
            KEY_STAGE_LINE + sample,
            Line::new().x1(x).y1(plot.top).x2(x).y2(plot.bottom()).stroke(brush(p.text_secondary)).stroke_thickness(1.0).into(),
        ));
        let room = stages.get(k + 1).map_or(plot.right(), |&(_, next, _)| next) - x - 6.0;
        if room >= LABEL_MIN {
            children.push((
                KEY_STAGE_LABEL + sample,
                Grid::new().width(room).canvas_left(x + 3.0).canvas_top(plot.top).children((TextBlock::new()
                    .text(label)
                    .font_size(11.0)
                    .foreground(brush(p.text_secondary))
                    .text_trimming(TextTrimming::CharacterEllipsis),)),
            ));
        }
    }

    // Error marks, under the series line so the line stays readable across them.
    for (sample, ago, _) in series.marks() {
        let x = plot.x_samples_ago(ago);
        children.push((
            KEY_MARK_LINE + sample,
            Line::new().x1(x).y1(plot.top).x2(x).y2(plot.bottom()).stroke(brush(p.error)).stroke_thickness(1.5).into(),
        ));
    }

    // The samples, newest at the right edge: a segment between two that have figures, the colour of the later one's, and a gap where one
    // has none
    let n = series.samples.len();
    let samples: Vec<(f64, Sample)> = series.samples.iter().enumerate().map(|(i, &s)| (plot.x_samples_ago(n - 1 - i), s)).collect();
    let colour = |s: Sample| if matches!(s, Sample::Seal(_) | Sample::SealedPass(_)) { p.chart_seal } else { p.accent };
    for (j, pair) in samples.windows(2).enumerate() {
        let ((x1, from), (x2, to)) = (pair[0], pair[1]);
        let (Some(v1), Some(v2)) = (from.value(), to.value()) else { continue };
        children.push((
            j as u64,
            Line::new().x1(x1).y1(plot.y(v1)).x2(x2).y2(plot.y(v2)).stroke(brush(colour(to))).stroke_thickness(2.0).into(),
        ));
    }
    // A reading alone between gaps (a test that published once, TODO 109), as a dash: it has no segment to show it
    for (j, &(x, sample)) in samples.iter().enumerate() {
        let alone = |k: Option<usize>| k.and_then(|k| samples.get(k)).is_none_or(|(_, s)| s.value().is_none());
        if let Some(v) = sample.value()
            && alone(j.checked_sub(1))
            && alone(Some(j + 1))
        {
            let y = plot.y(v);
            children.push((
                KEY_LONE + j as u64,
                Line::new().x1(x - LONE / 2.0).y1(y).x2(x + LONE / 2.0).y2(y).stroke(brush(colour(sample))).stroke_thickness(2.0).into(),
            ));
        }
    }

    // The temperature series' segments would go here, keyed in their own range, coloured by a palette key of their own, scaled to a
    // right-hand axis.

    // A dot on the series at each mark, over the line; none in a gap
    for (sample, ago, value) in series.marks() {
        let Some(value) = value else { continue };
        let (x, y) = (plot.x_samples_ago(ago), plot.y(value));
        children.push((
            KEY_MARK_DOT + sample,
            Ellipse::new().fill(brush(p.error)).width(DOT).height(DOT).canvas_left(x - DOT / 2.0).canvas_top(y - DOT / 2.0).into(),
        ));
    }

    Canvas::new().width(width).height(HEIGHT).keyed_children(children)
}
