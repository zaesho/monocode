//! Looping decorative motion: shimmers, pulses, turning rings, braille
//! spinners, and mascot beats.
//!
//! A repeating `with_animation` asks for a frame on every display refresh,
//! and each frame re-renders every view in the window. While an agent works,
//! that kept the whole window redrawing at 60 to 120 frames a second. These
//! loops go through [`monocode_ui::looping_step`] instead, which schedules one
//! redraw of the view at the next step:
//!
//! - [`stepped_loop`] for motion that is discrete anyway (a spinner's frames,
//!   a mascot's two poses), so the look is unchanged.
//! - [`smooth_loop`] for continuous motion (a shimmer band, an opacity pulse,
//!   a turning ring), drawn at [`SMOOTH_FPS`] frames a second.
//!
//! Every loop counts from one shared clock, so loops in the same view share
//! their redraws. With reduced motion they hold their first step, as GPUI's
//! repeating animations do, except [`spinner_loop`]: the React
//! `TerminalSpinner` ignored `prefers-reduced-motion`, and a frozen spinner
//! reads as a hang.

use std::time::Duration;

use gpui::{AnyElement, App, IntoElement, RenderOnce, Window};
use monocode_ui::{loading_step, looping_step};

/// Frames a second of a [`smooth_loop`]. Enough for a slow shimmer or pulse,
/// a quarter of a 120Hz display's refresh rate.
pub const SMOOTH_FPS: u32 = 30;

/// Steps a smooth loop of `period` takes, about [`SMOOTH_FPS`] a second.
/// Periods that are whole multiples of a frame keep every smooth loop on the
/// same step grid, so they share one redraw.
pub fn smooth_steps(period: Duration) -> u32 {
    let steps = period.as_secs_f64() * f64::from(SMOOTH_FPS);
    (steps.round() as u32).max(1)
}

/// A looping element: `build` draws the loop's current step.
#[derive(IntoElement)]
pub struct Looping {
    period: Duration,
    steps: u32,
    /// A loading indicator, which keeps moving with reduced motion.
    loading: bool,
    build: Box<dyn FnOnce(u32) -> AnyElement>,
}

/// A loop of `steps` discrete states over `period`. `build` gets the state,
/// in `0..steps`, and the view redraws only when the state changes.
pub fn stepped_loop<E: IntoElement>(
    period: Duration,
    steps: u32,
    build: impl FnOnce(u32) -> E + 'static,
) -> Looping {
    Looping {
        period,
        steps: steps.max(1),
        loading: false,
        build: Box::new(move |step| build(step).into_any_element()),
    }
}

/// A [`stepped_loop`] for a loading spinner. It keeps turning with reduced
/// motion.
pub fn spinner_loop<E: IntoElement>(
    period: Duration,
    steps: u32,
    build: impl FnOnce(u32) -> E + 'static,
) -> Looping {
    Looping {
        loading: true,
        ..stepped_loop(period, steps, build)
    }
}

/// A continuous loop over `period`. `build` gets the progress, in
/// `0.0..1.0` like a repeating animation's delta, at [`SMOOTH_FPS`].
pub fn smooth_loop<E: IntoElement>(
    period: Duration,
    build: impl FnOnce(f32) -> E + 'static,
) -> Looping {
    let steps = smooth_steps(period);
    Looping {
        period,
        steps,
        loading: false,
        build: Box::new(move |step| build(step as f32 / steps as f32).into_any_element()),
    }
}

impl RenderOnce for Looping {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let step = if self.loading {
            loading_step(self.period, self.steps, window, cx)
        } else {
            looping_step(self.period, self.steps, window, cx)
        };
        (self.build)(step)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui::{Context, ParentElement as _, Render, Styled as _, TestAppContext, div, px, size};

    use super::*;

    #[test]
    fn smooth_loops_share_one_step_length() {
        for ms in [1000, 1400, 1600, 1800, 2000, 3600] {
            let period = Duration::from_millis(ms);
            let step = period.as_nanos() / u128::from(smooth_steps(period));
            assert_eq!(step, 33_333_333, "{ms}ms");
        }
        assert_eq!(smooth_steps(Duration::from_millis(10)), 1);
    }

    struct Shimmering {
        renders: Rc<Cell<usize>>,
    }

    impl Render for Shimmering {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.renders.set(self.renders.get() + 1);
            div()
                .child(smooth_loop(Duration::from_millis(1600), |t| {
                    div().opacity(0.5 + t / 2.)
                }))
                .child(stepped_loop(Duration::from_millis(460), 2, |beat| {
                    div().opacity(if beat == 0 { 1. } else { 0.5 })
                }))
        }
    }

    #[gpui::test]
    fn loops_redraw_on_their_steps_not_on_every_refresh(cx: &mut TestAppContext) {
        let renders = Rc::new(Cell::new(0));
        let window = cx.open_window(size(px(100.), px(100.)), {
            let renders = renders.clone();
            move |_, _| Shimmering { renders }
        });
        cx.run_until_parked();
        let first = renders.get();
        assert!(first >= 1);
        // A display refresh alone does not re-render the view.
        let callbacks = window
            .update(cx, |_, window, cx| window.simulate_next_frame(cx))
            .unwrap();
        assert_eq!(callbacks, 0);
        cx.run_until_parked();
        assert_eq!(renders.get(), first);
        // The next step does, once.
        cx.executor().advance_clock(Duration::from_millis(40));
        cx.run_until_parked();
        assert_eq!(renders.get(), first + 1);
    }
}
