//! Looping animations that change in discrete steps.
//!
//! `with_animation(.., Animation::new(..).repeat())` asks for a new frame on
//! every display refresh, and each frame re-renders every view in the window.
//! A braille spinner changes every 80ms, so on a 120Hz display it re-rendered
//! the window about ten times per visible change. [`looping_step`] returns
//! the current step and schedules one redraw of the view being rendered at
//! the next step boundary instead.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, AsyncApp, Bounds, Element, ElementId, EntityId, Global, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Pixels, Window,
};

/// The shared clock and the redraws already scheduled, per view.
struct StepClock {
    /// Every stepped animation counts from here, so animations with the same
    /// period stay in phase and share one redraw.
    epoch: Instant,
    /// The earliest scheduled redraw for each view.
    pending: HashMap<EntityId, Instant>,
}

impl Default for StepClock {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
            pending: HashMap::new(),
        }
    }
}

impl Global for StepClock {}

/// The current step, in `0..steps`, of a decorative loop that shows `steps`
/// states over `period`, and a scheduled redraw of the current view when the
/// step changes. Call it while rendering.
///
/// With reduced motion it returns step 0 and schedules nothing, the way
/// GPUI's repeating animations hold their start state. Loading indicators
/// use [`loading_step`] instead.
pub fn looping_step(period: Duration, steps: u32, window: &mut Window, cx: &mut App) -> u32 {
    if cx.reduce_motion() {
        return 0;
    }
    loading_step(period, steps, window, cx)
}

/// [`looping_step`] for loading indicators: spinners and loading pulses keep
/// moving with reduced motion. The React spinners (`TerminalSpinner` and
/// `animate-spin` loaders) ignored `prefers-reduced-motion`, and a frozen
/// spinner reads as a hang.
pub fn loading_step(period: Duration, steps: u32, window: &mut Window, cx: &mut App) -> u32 {
    if steps == 0 || period.is_zero() {
        return 0;
    }
    let view = window.current_view();
    let now = Instant::now();
    let clock = cx.default_global::<StepClock>();
    let step_ns = (period.as_nanos() / u128::from(steps)).max(1);
    let index = now.saturating_duration_since(clock.epoch).as_nanos() / step_ns;
    let step = (index % u128::from(steps)) as u32;
    let next_ns = u64::try_from((index + 1) * step_ns).unwrap_or(u64::MAX);
    let deadline = clock.epoch + Duration::from_nanos(next_ns);
    let already_sooner = clock
        .pending
        .get(&view)
        .is_some_and(|pending| *pending <= deadline);
    if !already_sooner {
        clock.pending.insert(view, deadline);
        cx.spawn(async move |cx: &mut AsyncApp| {
            // The extra millisecond keeps a timer that fires a hair early
            // from rendering the old step and scheduling the same deadline.
            let wait =
                deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(1);
            cx.background_executor().timer(wait).await;
            cx.update(|cx| {
                let clock = cx.default_global::<StepClock>();
                // A sooner redraw replaced this one, and its own timer
                // handles the view.
                if clock.pending.get(&view) != Some(&deadline) {
                    return;
                }
                clock.pending.remove(&view);
                cx.notify(view);
            });
        })
        .detach();
    }
    step
}

/// Redraws a second for continuous motion through [`SteppedAnimationExt`]: a
/// turning loader or an opacity pulse. A quarter of a 120Hz refresh rate.
pub const SMOOTH_FPS: u32 = 30;

/// Steps for a continuous loop of `period` at [`SMOOTH_FPS`].
pub fn smooth_steps(period: Duration) -> u32 {
    ((period.as_secs_f64() * f64::from(SMOOTH_FPS)).round() as u32).max(1)
}

/// A looping animation that redraws `steps` times per `period` instead of on
/// every display refresh. The animator gets the loop progress in `0..1`.
///
/// Use it for loops whose motion reads the same at a capped rate, such as an
/// opacity pulse: on a 120Hz display a two second pulse at 30 steps a second
/// re-renders the window a quarter as often.
pub trait SteppedAnimationExt: IntoElement + Sized + 'static {
    /// A decorative loop. It holds its first step with reduced motion.
    fn with_stepped_animation(
        self,
        period: Duration,
        steps: u32,
        animator: impl Fn(Self, f32) -> Self + 'static,
    ) -> SteppedAnimation<Self> {
        SteppedAnimation {
            element: Some(self),
            period,
            steps,
            loading: false,
            animator: Box::new(animator),
        }
    }

    /// A loading indicator. It keeps moving with reduced motion, see
    /// [`loading_step`].
    fn with_loading_animation(
        self,
        period: Duration,
        steps: u32,
        animator: impl Fn(Self, f32) -> Self + 'static,
    ) -> SteppedAnimation<Self> {
        SteppedAnimation {
            loading: true,
            ..self.with_stepped_animation(period, steps, animator)
        }
    }
}

impl<E: IntoElement + 'static> SteppedAnimationExt for E {}

/// The element [`SteppedAnimationExt::with_stepped_animation`] returns.
pub struct SteppedAnimation<E> {
    element: Option<E>,
    period: Duration,
    steps: u32,
    /// Keeps moving with reduced motion.
    loading: bool,
    animator: Box<dyn Fn(E, f32) -> E + 'static>,
}

impl<E: IntoElement + 'static> IntoElement for SteppedAnimation<E> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<E: IntoElement + 'static> Element for SteppedAnimation<E> {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let step = if self.loading {
            loading_step(self.period, self.steps, window, cx)
        } else {
            looping_step(self.period, self.steps, window, cx)
        };
        let progress = step as f32 / self.steps.max(1) as f32;
        let element = self.element.take().expect("requested layout once");
        let mut element = (self.animator)(element, progress).into_any_element();
        (element.request_layout(window, cx), element)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        element: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        element.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        element: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        element.paint(window, cx);
    }
}

/// How many redraws are scheduled, for tests.
#[cfg(test)]
pub(crate) fn pending_redraws(cx: &App) -> usize {
    cx.try_global::<StepClock>()
        .map_or(0, |clock| clock.pending.len())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui::{Context, IntoElement, Render, TestAppContext, div, px, size};

    use super::*;

    struct Stepper {
        renders: Rc<Cell<usize>>,
    }

    impl Render for Stepper {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.renders.set(self.renders.get() + 1);
            // A spinner and a slower mark in the same view share one redraw.
            looping_step(Duration::from_millis(800), 10, window, cx);
            looping_step(Duration::from_millis(3200), 4, window, cx);
            div()
        }
    }

    #[gpui::test]
    fn redraws_once_per_step_instead_of_every_frame(cx: &mut TestAppContext) {
        let renders = Rc::new(Cell::new(0));
        let window = cx.open_window(size(px(100.), px(100.)), {
            let renders = renders.clone();
            move |_, _| Stepper { renders }
        });
        cx.run_until_parked();
        let first = renders.get();
        assert!(first >= 1);
        assert_eq!(cx.update(|cx| pending_redraws(cx)), 1);

        // A display refresh alone does not re-render the view.
        let callbacks = window
            .update(cx, |_, window, cx| window.simulate_next_frame(cx))
            .unwrap();
        assert_eq!(callbacks, 0);
        cx.run_until_parked();
        assert_eq!(renders.get(), first);

        // The step timer re-renders it once and schedules the next step.
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        assert_eq!(renders.get(), first + 1);
        assert_eq!(cx.update(|cx| pending_redraws(cx)), 1);
    }

    #[gpui::test]
    fn reduced_motion_holds_the_first_step(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let renders = Rc::new(Cell::new(0));
        cx.open_window(size(px(100.), px(100.)), {
            let renders = renders.clone();
            move |_, _| Stepper { renders }
        });
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| pending_redraws(cx)), 0);
    }

    struct Loading;

    impl Render for Loading {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            loading_step(Duration::from_millis(800), 10, window, cx);
            div()
        }
    }

    #[gpui::test]
    fn loading_indicators_keep_moving_with_reduced_motion(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        cx.open_window(size(px(100.), px(100.)), |_, _| Loading);
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| pending_redraws(cx)), 1);
    }
}
