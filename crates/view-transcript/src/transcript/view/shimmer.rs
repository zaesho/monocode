//! Port of src/shared/ui/Shimmer.tsx (`.shimmer-text` in index.css): text at
//! 40% ink with a full-strength band sweeping across it.
//!
//! CSS paints the band with a clipped background gradient, so the band's
//! edge crosses a glyph partway. GPUI glyphs take one color each, so coloring
//! per character made the band hop from glyph to glyph. [`ShimmerText`]
//! paints the text at 40% ink, then repaints its glyphs at full ink in thin
//! vertical strips, each clipped to its strip and faded by the band's
//! strength there. The band moves by pixels, not by characters.

use std::f32::consts::PI;
use std::time::Duration;

use gpui::{
    App, Bounds, ContentMask, Element, ElementId, GlobalElementId, Hsla, InspectorElementId,
    IntoElement, LayoutId, ParentElement as _, Pixels, SharedString, Styled as _, StyledText,
    Window, div, point, px, size,
};
use monocode_ui::Theme;

use crate::motion::smooth_loop;

/// The band's half-width per character, in pixels: the CSS `--spread`.
const SPREAD_PER_CHAR: f32 = 2.;

/// The narrowest band, so a short label still gets a soft sweep.
const MIN_SPREAD: f32 = 24.;

/// Device pixels per strip. Two device pixels hide the strip edges and keep
/// a wide band to about a hundred clipped glyph draws.
const STRIP_DEVICE_PX: f32 = 2.;

/// The band's strength `offset` pixels from its center: 1 at the center,
/// easing to 0 at `spread` on a raised cosine, with no corner at the peak or
/// the edges.
fn band(offset: f32, spread: f32) -> f32 {
    if spread <= 0. || offset.abs() >= spread {
        return 0.;
    }
    0.5 + 0.5 * (PI * offset / spread).cos()
}

/// The band's center at `progress`, in pixels from the text's left edge.
/// `background-position` 100% to 0% over a 250% wide background moves the
/// center from -25% to 125% of the text width.
fn band_center(progress: f32, width: f32) -> f32 {
    (-0.25 + 1.5 * progress) * width
}

/// The band's half-width for `text`, in pixels.
fn band_spread(text: &str) -> f32 {
    (text.chars().count() as f32 * SPREAD_PER_CHAR).max(MIN_SPREAD)
}

/// Strips covering the band: each strip's left edge relative to the text,
/// and the band's strength at its middle. `strip` is the strip width in
/// pixels, and the strips sit on a grid of `strip` from the left edge so the
/// clips line up with device pixels.
fn band_strips(center: f32, spread: f32, strip: f32) -> Vec<(f32, f32)> {
    if strip <= 0. {
        return Vec::new();
    }
    let first = ((center - spread) / strip).floor() as i32;
    let last = ((center + spread) / strip).ceil() as i32;
    (first..last)
        .filter_map(|ix| {
            let left = ix as f32 * strip;
            let strength = band(left + strip / 2. - center, spread);
            (strength > 0.004).then_some((left, strength))
        })
        .collect()
}

/// Text at the inherited color with a full `ink` band at `progress` of the
/// sweep. Lays out like [`StyledText`], so a parent's `truncate` applies.
struct ShimmerText {
    text: StyledText,
    ink: Hsla,
    spread: f32,
    progress: f32,
}

impl IntoElement for ShimmerText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ShimmerText {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.text.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.text
            .prepaint(id, inspector_id, bounds, state, window, cx);
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.text
            .paint(id, inspector_id, bounds, state, prepaint, window, cx);

        let layout = self.text.layout().clone();
        // A shimmer label truncates to one line.
        let Some(line) = layout.line_layouts().into_iter().next() else {
            return;
        };
        let line = &line.unwrapped_layout;
        let width = f32::from(line.width);
        if width <= 0. {
            return;
        }
        let origin = layout.bounds().origin;
        let line_height = layout.line_height();
        let baseline = origin.y + (line_height - line.ascent - line.descent) / 2. + line.ascent;
        // Glyphs whose origin is this far left of a strip can still reach it.
        let reach = f32::from(line.font_size) * 1.5;
        let strip = STRIP_DEVICE_PX / window.scale_factor();
        let center = band_center(self.progress, width);
        for (left, strength) in band_strips(center, self.spread, strip) {
            let color = Hsla {
                a: self.ink.a * strength,
                ..self.ink
            };
            let mask = ContentMask {
                bounds: Bounds::new(
                    point(origin.x + px(left), origin.y),
                    size(px(strip), line_height),
                ),
            };
            window.with_content_mask(Some(mask), |window| {
                for run in &line.runs {
                    for glyph in &run.glyphs {
                        let x = f32::from(glyph.position.x);
                        if glyph.is_emoji || x > left + strip || x + reach < left {
                            continue;
                        }
                        window
                            .paint_glyph(
                                point(origin.x + glyph.position.x, baseline),
                                run.font_id,
                                glyph.id,
                                line.font_size,
                                color,
                            )
                            .ok();
                    }
                }
            });
        }
    }
}

/// `<Shimmer duration={…}>`: `text` swept once every `duration`, drawn as a
/// [`smooth_loop`] so the sweep does not redraw the window every refresh.
pub fn shimmer(
    text: impl Into<SharedString>,
    duration: Duration,
    theme: &Theme,
) -> impl IntoElement {
    let text: SharedString = text.into();
    let ink = theme.colors.content;
    let spread = band_spread(&text);
    div()
        .min_w_0()
        .truncate()
        .text_color(theme.content(0.4))
        .child(smooth_loop(duration, move |progress| ShimmerText {
            text: StyledText::new(text),
            ink,
            spread,
            progress,
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_band_eases_from_its_center_to_nothing_at_its_spread() {
        assert!((band(0., 60.) - 1.).abs() < 1e-6);
        assert!((band(30., 60.) - 0.5).abs() < 1e-6);
        assert_eq!(band(60., 60.), 0.);
        assert_eq!(band(-90., 60.), 0.);
        // No corner at the peak: a pixel off center is still nearly full.
        assert!(band(1., 60.) > 0.999);
    }

    #[test]
    fn the_band_sweeps_from_before_the_text_to_past_it() {
        assert_eq!(band_center(0., 200.), -50.);
        assert_eq!(band_center(0.5, 200.), 100.);
        assert_eq!(band_center(1., 200.), 250.);
        assert_eq!(band_spread("Working for 3s"), 28.);
        assert_eq!(band_spread("Hi"), MIN_SPREAD);
    }

    #[test]
    fn strips_cover_the_band_in_pixel_steps_and_fade_smoothly() {
        let strips = band_strips(100., 60., 1.);
        let (first, _) = strips[0];
        let (last, _) = *strips.last().unwrap();
        assert!((40. ..45.).contains(&first));
        assert!((155. ..160.).contains(&last));
        // Adjacent strips differ by a small step, so no strip edge shows.
        for pair in strips.windows(2) {
            assert_eq!(pair[1].0 - pair[0].0, 1.);
            assert!((pair[1].1 - pair[0].1).abs() < 0.03);
        }
    }

    #[test]
    fn the_band_moves_by_fractions_of_a_character() {
        // Two frames a thirtieth of a sweep apart move the peak strip by
        // pixels on a 200px label, not by whole characters.
        let peak = |progress: f32| {
            band_strips(band_center(progress, 200.), 60., 1.)
                .into_iter()
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .unwrap()
                .0
        };
        let step = peak(0.5 + 1. / 30.) - peak(0.5);
        assert!((step - 10.).abs() <= 1.);
    }
}
