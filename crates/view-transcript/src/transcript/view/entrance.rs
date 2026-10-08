//! How a live step arrives on its phase's rail: `PhaseStep`, `useStepQueue`,
//! and the `.zen-phase-step[data-entering]` keyframes in index.css.
//!
//! A step that lands while its group is open and live makes room first: its
//! slot grows from nothing and pushes what is below it down, the spine runs
//! on past the step above, and the branch curves off it. Only then does the
//! row rise 10px into the room as it fades in. Steps in a burst queue, each
//! waiting for the one before it to finish. Steps already there when a group
//! first draws, or that land while it is folded or off screen, are history
//! and never animate.
//!
//! GPUI's `with_animation` keeps its clock in element state, which is lost
//! when a row leaves the list or its element path changes, and then the
//! animation plays again. Each step's start time lives here instead, keyed by
//! the step's block id, so a re-render or a scroll never replays it.
//!
//! Two parts of the CSS have no GPUI equivalent. The slot's `0fr` to `1fr`
//! grid track grows to the row's height as measured on the frame before, and
//! the soft-edged mask that revealed the row from the bottom up is left out.
//! The slot still clips the row while it rises.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, IntoElement, ParentElement as _, Pixels, Styled as _, canvas, div, px};
use monocode_ui::theme::CubicBezier;
use monocode_ui::{Theme, u};

use super::parts::{rail_branch, rail_spine};
use super::style::rail_color;

/// `STEP_ENTRANCE_MS`: the first steps of a burst take this long each.
pub const STEP_ENTRANCE: Duration = Duration::from_millis(480);
/// `STEP_ENTRANCE_MIN_MS`: the fastest a step in a long burst plays.
pub const STEP_ENTRANCE_MIN: Duration = Duration::from_millis(160);
/// `STEP_QUEUE_CALM_MS`: a queue this long still plays at full pace.
pub const STEP_QUEUE_CALM: Duration = Duration::from_millis(960);
/// `STEP_QUEUE_MS`: a queue this long plays at the fastest pace.
pub const STEP_QUEUE: Duration = Duration::from_millis(2000);
/// `var(--step-ms, 320ms)`: the spine transition of a step that had no
/// entrance of its own.
const SETTLED_STEP_PACE: Duration = Duration::from_millis(320);

/// `zen-step-open`: `cubic-bezier(0.22, 1, 0.36, 1)`.
const OPEN_EASE: CubicBezier = CubicBezier(0.22, 1.0, 0.36, 1.0);
/// `zen-step-in`: `cubic-bezier(0.16, 1, 0.3, 1)`.
const RISE_EASE: CubicBezier = CubicBezier(0.16, 1.0, 0.3, 1.0);
/// CSS `ease-in-out`, for `zen-rail-grow`.
const EASE_IN_OUT: CubicBezier = CubicBezier(0.42, 0.0, 0.58, 1.0);
/// CSS `ease-out`, for `zen-branch-draw`.
const EASE_OUT: CubicBezier = CubicBezier(0.0, 0.0, 0.58, 1.0);
/// CSS `ease-in`, for the spine of the step above.
const EASE_IN: CubicBezier = CubicBezier(0.42, 0.0, 1.0, 1.0);
/// `translateY(10px)` at the start of `zen-step-in`.
const RISE: f32 = 10.;
/// The spine of a last step stops at its branch.
const LAST_SPINE: f32 = 7.;

/// One step's entrance: when it starts and how long it takes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StepTurn {
    pub start: Instant,
    pub pace: Duration,
}

/// Where a step is in its entrance at some moment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StepStage {
    /// Queued behind another step. It stays out of the layout.
    Waiting,
    /// Arriving, `progress` of the way through its `pace`.
    Entering { progress: f32, pace: Duration },
    /// On the rail for good.
    Settled,
}

#[derive(Debug, Default)]
struct PhaseSteps {
    /// The render pass that last drew the phase.
    seen: u64,
    /// Steps that are history, or have been given their turn.
    settled: HashSet<String>,
    turns: HashMap<String, StepTurn>,
    /// When the last queued entrance ends.
    next: Option<Instant>,
}

/// Every phase's arriving steps, by phase key.
#[derive(Debug, Default)]
pub struct StepEntrances {
    /// Render passes of the transcript so far. A phase missing from a pass
    /// was off screen or folded away.
    pass: u64,
    phases: HashMap<String, PhaseSteps>,
}

impl StepEntrances {
    /// Call once per render of the transcript, before its rows draw.
    pub fn next_pass(&mut self) {
        self.pass += 1;
    }

    /// Forget every phase, for a different session.
    pub fn clear(&mut self) {
        self.phases.clear();
    }

    /// Record the steps a phase draws this pass. With `arriving`, the
    /// group is open and live, and a step it has not seen before gets an
    /// entrance. Otherwise new steps settle quietly. A phase drawn for the
    /// first time, or again after a pass without it, settles everything it
    /// holds: only a step you watch arrive gets the entrance.
    pub fn sync<'a>(
        &mut self,
        phase: &str,
        steps: impl IntoIterator<Item = &'a str>,
        arriving: bool,
        now: Instant,
    ) {
        let pass = self.pass;
        let fresh = !self.phases.contains_key(phase);
        let entry = self.phases.entry(phase.to_string()).or_default();
        let watched = !fresh && entry.seen + 1 >= pass;
        entry.seen = pass;
        for id in steps {
            if !entry.settled.insert(id.to_string()) || !watched || !arriving {
                continue;
            }
            let turn = queue_turn(entry.next, now);
            entry.next = Some(turn.start + turn.pace);
            entry.turns.insert(id.to_string(), turn);
        }
    }

    /// Where a step is in its entrance at `now`.
    pub fn stage(&self, phase: &str, step: &str, now: Instant) -> StepStage {
        let Some(turn) = self
            .phases
            .get(phase)
            .and_then(|entry| entry.turns.get(step))
        else {
            return StepStage::Settled;
        };
        if now < turn.start {
            return StepStage::Waiting;
        }
        let elapsed = now.duration_since(turn.start);
        if elapsed >= turn.pace {
            return StepStage::Settled;
        }
        StepStage::Entering {
            progress: elapsed.as_secs_f32() / turn.pace.as_secs_f32(),
            pace: turn.pace,
        }
    }

    /// The pace a step's spine transition plays at: its own entrance's, or
    /// the CSS fallback for a step that had none.
    pub fn pace(&self, phase: &str, step: &str) -> Duration {
        self.phases
            .get(phase)
            .and_then(|entry| entry.turns.get(step))
            .map_or(SETTLED_STEP_PACE, |turn| turn.pace)
    }

    /// When a step's entrance started, if it had one.
    pub fn started(&self, phase: &str, step: &str) -> Option<Instant> {
        self.phases
            .get(phase)
            .and_then(|entry| entry.turns.get(step))
            .map(|turn| turn.start)
    }
}

/// `useStepQueue`: a step waits for the entrance before it, and plays
/// faster the longer the queue it joins.
fn queue_turn(next: Option<Instant>, now: Instant) -> StepTurn {
    let start = next.filter(|next| *next > now).unwrap_or(now);
    let wait = start.duration_since(now).as_secs_f64();
    let backlog = (STEP_QUEUE.as_secs_f64() - wait)
        / (STEP_QUEUE.as_secs_f64() - STEP_QUEUE_CALM.as_secs_f64());
    let pace = if backlog >= 1. {
        STEP_ENTRANCE
    } else {
        STEP_ENTRANCE
            .mul_f64(backlog.max(0.))
            .max(STEP_ENTRANCE_MIN)
    };
    StepTurn { start, pace }
}

/// The eased progress of a beat that starts `delay` and lasts `length` of
/// a step's pace, at step progress `t`.
fn beat(t: f32, delay: f32, length: f32, curve: CubicBezier) -> f32 {
    curve.ease(((t - delay) / length).clamp(0., 1.))
}

/// Natural row heights by step key, written as rows prepaint and read on
/// the next frame. An entering slot clips its row, so its own height says
/// nothing about how tall the row is.
pub type StepHeights = Rc<RefCell<HashMap<String, Pixels>>>;

/// `.zen-phase-step` for a step arriving `progress` of the way through its
/// entrance. It is the last step drawn: a step queued behind it stays out.
pub fn entering_step(
    key: String,
    progress: f32,
    heights: &StepHeights,
    theme: &Theme,
    child: AnyElement,
) -> AnyElement {
    let open = beat(progress, 0., 0.4, OPEN_EASE);
    let spine = beat(progress, 0.15, 0.15, EASE_IN_OUT);
    let branch = beat(progress, 0.3, 0.2, EASE_OUT);
    let rise = beat(progress, 0.5, 0.5, RISE_EASE);
    let height = heights.borrow().get(&key).copied();
    let measured = heights.clone();
    div()
        .relative()
        .pl(u(20.))
        .overflow_hidden()
        .h(height.map_or(px(0.), |height| height * open))
        .on_children_prepainted(move |bounds, _, _| {
            // The row is the last child, after the spine and the branch.
            if let Some(row) = bounds.last() {
                measured.borrow_mut().insert(key.clone(), row.size.height);
            }
        })
        .child(
            div()
                .absolute()
                .left(u(6.))
                .top_0()
                .w(px(1.))
                .h(u(LAST_SPINE * spine))
                .bg(rail_color(theme)),
        )
        // `clip-path: inset(0 100% 100% 0)` to `inset(0)`: a box growing
        // from the curve's top left corner uncovers it in order.
        .child(
            div()
                .absolute()
                .left(u(6.))
                .top(u(6.))
                .w(u(8. * branch))
                .h(u(8. * branch))
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .size(u(8.))
                        .border_l(px(1.))
                        .border_b(px(1.))
                        .border_color(rail_color(theme))
                        .rounded_bl(u(8.)),
                ),
        )
        .child(
            div()
                .relative()
                .top(u(RISE * (1. - rise)))
                .opacity(rise)
                .child(child),
        )
        .into_any_element()
}

/// `.zen-phase-step` for a settled step. `spine` is how far its spine has
/// run on past its branch towards its bottom, from 0 to 1, while the step
/// under it arrives; `None` draws the spine whole (or stopped at the branch
/// for the `last` step). With `measure`, the row's height is recorded for
/// the step that arrives under it.
pub fn settled_step(
    last: bool,
    spine: Option<f32>,
    measure: Option<(String, StepHeights)>,
    theme: &Theme,
    child: AnyElement,
) -> AnyElement {
    let height = measure
        .as_ref()
        .and_then(|(key, heights)| heights.borrow().get(key).copied());
    let rail = match (spine, height) {
        (Some(run), Some(height)) if !last => {
            let reach = f32::from(height).max(LAST_SPINE);
            div()
                .absolute()
                .left(u(6.))
                .top_0()
                .w(px(1.))
                .h(px(LAST_SPINE + (reach - LAST_SPINE) * run))
                .bg(rail_color(theme))
                .into_any_element()
        }
        _ => rail_spine(6., last, theme).into_any_element(),
    };
    div()
        .relative()
        .pl(u(20.))
        .when_some(measure, |el, (key, heights)| {
            el.on_children_prepainted(move |bounds, _, _| {
                if let Some(row) = bounds.last() {
                    heights.borrow_mut().insert(key.clone(), row.size.height);
                }
            })
        })
        .child(rail)
        .child(rail_branch(6., theme))
        .child(child)
        .into_any_element()
}

/// How far the spine of the step above an arriving one has run, at `now`:
/// `transition: height calc(var(--step-ms) * 0.3) ease-in`, counted from
/// when the step under it started.
pub fn spine_run(above_pace: Duration, below_started: Instant, now: Instant) -> Option<f32> {
    let length = above_pace.mul_f32(0.3);
    let elapsed = now.saturating_duration_since(below_started);
    (elapsed < length).then(|| EASE_IN.ease(elapsed.as_secs_f32() / length.as_secs_f32()))
}

/// An invisible element that asks for the next frame while an entrance
/// plays, so the view draws its next beat.
pub fn next_frame() -> impl IntoElement {
    canvas(
        |_, window, _| window.request_animation_frame(),
        |_, _, _, _| {},
    )
    .absolute()
    .size_0()
}

/// `PROMPT_RISE_MS`: a sent prompt slides into its row this long.
pub const PROMPT_RISE: Duration = Duration::from_millis(560);
/// `PROMPT_FADE_MS`: the prompt fades in this long, on a gentler curve.
const PROMPT_FADE: Duration = Duration::from_millis(480);
/// `PROMPT_REVEAL_MS`: once the prompt lands, the rest of its turn rises
/// in this long (`prompt-turn-reveal`).
pub const PROMPT_REVEAL: Duration = Duration::from_millis(320);
/// `PROMPT_RISE_FROM`: where the prompt starts, as a fraction of the
/// viewport height from the top.
pub const PROMPT_RISE_FROM: f32 = 0.3;

/// `riseIntoAnchor` at one moment after the send, each value from 0 to 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PromptRise {
    /// How far the prompt has travelled to its row.
    pub travel: f32,
    /// The prompt's opacity.
    pub fade: f32,
    /// The rest of the turn: hidden while the prompt rises, then rising
    /// 10px as it fades in.
    pub reveal: f32,
}

/// Where a sent prompt's rise is `elapsed` after the send, or `None` once
/// the prompt has landed and the rest of its turn is in.
pub fn prompt_rise(elapsed: Duration) -> Option<PromptRise> {
    if elapsed >= PROMPT_RISE + PROMPT_REVEAL {
        return None;
    }
    let part = |elapsed: Duration, length: Duration| {
        (elapsed.as_secs_f32() / length.as_secs_f32()).clamp(0., 1.)
    };
    Some(PromptRise {
        travel: OPEN_EASE.ease(part(elapsed, PROMPT_RISE)),
        fade: EASE_OUT.ease(part(elapsed, PROMPT_FADE)),
        reveal: OPEN_EASE.ease(part(elapsed.saturating_sub(PROMPT_RISE), PROMPT_REVEAL)),
    })
}

/// A row of the turn a prompt was just sent in: the prompt slides up from
/// `from` below its row, and the rows after it wait, then rise into place.
pub fn rising_row(prompt: bool, rise: PromptRise, from: Pixels, child: AnyElement) -> AnyElement {
    let row = div().relative();
    let row = if prompt {
        row.top(from * (1. - rise.travel)).opacity(rise.fade)
    } else {
        row.top(u(RISE * (1. - rise.reveal))).opacity(rise.reveal)
    };
    row.child(child).child(next_frame()).into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    /// One render pass that draws `phase` with `steps`.
    fn pass(
        entrances: &mut StepEntrances,
        phase: &str,
        steps: &[&str],
        arriving: bool,
        now: Instant,
    ) {
        entrances.next_pass();
        entrances.sync(phase, steps.iter().copied(), arriving, now);
    }

    #[test]
    fn steps_on_screen_when_a_group_first_draws_are_history() {
        let mut entrances = StepEntrances::default();
        let now = Instant::now();
        pass(&mut entrances, "p", &["a", "b"], true, now);
        assert_eq!(entrances.stage("p", "a", now), StepStage::Settled);
        assert_eq!(entrances.stage("p", "b", now), StepStage::Settled);
    }

    #[test]
    fn a_step_that_lands_while_you_watch_enters_once() {
        let mut entrances = StepEntrances::default();
        let t0 = Instant::now();
        pass(&mut entrances, "p", &["a"], true, t0);
        pass(&mut entrances, "p", &["a", "b"], true, t0);
        assert_eq!(
            entrances.stage("p", "b", t0),
            StepStage::Entering {
                progress: 0.,
                pace: STEP_ENTRANCE
            }
        );
        // Later passes keep the same clock instead of starting again.
        let mid = t0 + ms(240);
        pass(&mut entrances, "p", &["a", "b"], true, mid);
        match entrances.stage("p", "b", mid) {
            StepStage::Entering { progress, .. } => assert!((progress - 0.5).abs() < 1e-3),
            stage => panic!("{stage:?}"),
        }
        let done = t0 + STEP_ENTRANCE;
        pass(&mut entrances, "p", &["a", "b"], true, done);
        assert_eq!(entrances.stage("p", "b", done), StepStage::Settled);
        assert_eq!(entrances.started("p", "b"), Some(t0));
    }

    #[test]
    fn a_burst_queues_and_a_long_queue_plays_faster() {
        let mut entrances = StepEntrances::default();
        let t0 = Instant::now();
        pass(&mut entrances, "p", &[], true, t0);
        let ids = ["a", "b", "c", "d", "e", "f"];
        pass(&mut entrances, "p", &ids, true, t0);
        assert!(matches!(
            entrances.stage("p", "a", t0),
            StepStage::Entering { .. }
        ));
        assert_eq!(entrances.stage("p", "b", t0), StepStage::Waiting);
        let starts: Vec<Instant> = ids
            .iter()
            .map(|id| entrances.started("p", id).unwrap())
            .collect();
        let paces: Vec<Duration> = ids.iter().map(|id| entrances.pace("p", id)).collect();
        // Each waits for the one before it to finish.
        for i in 1..ids.len() {
            assert_eq!(starts[i], starts[i - 1] + paces[i - 1]);
        }
        // Three steps in, the wait passes the calm mark and paces shrink.
        assert_eq!(paces[0], STEP_ENTRANCE);
        assert_eq!(paces[1], STEP_ENTRANCE);
        assert_eq!(paces[2], STEP_ENTRANCE);
        assert!(paces[3] < STEP_ENTRANCE);
        assert!(paces.iter().all(|pace| *pace >= STEP_ENTRANCE_MIN));
        assert!(paces[5] <= paces[4]);
    }

    #[test]
    fn steps_that_land_folded_or_off_screen_do_not_enter() {
        let mut entrances = StepEntrances::default();
        let now = Instant::now();
        pass(&mut entrances, "p", &["a"], true, now);
        // Folded: the header draws, the step does not arrive.
        pass(&mut entrances, "p", &["a", "b"], false, now);
        assert_eq!(entrances.stage("p", "b", now), StepStage::Settled);
        // Off screen for a pass, then back with a new step.
        entrances.next_pass();
        pass(&mut entrances, "p", &["a", "b", "c"], true, now);
        assert_eq!(entrances.stage("p", "c", now), StepStage::Settled);
        // Watched again from there on.
        pass(&mut entrances, "p", &["a", "b", "c", "d"], true, now);
        assert!(matches!(
            entrances.stage("p", "d", now),
            StepStage::Entering { .. }
        ));
    }

    #[test]
    fn a_session_switch_forgets_every_phase() {
        let mut entrances = StepEntrances::default();
        let now = Instant::now();
        pass(&mut entrances, "p", &["a"], true, now);
        entrances.clear();
        pass(&mut entrances, "p", &["a", "b"], true, now);
        assert_eq!(entrances.stage("p", "b", now), StepStage::Settled);
    }

    #[test]
    fn a_sent_prompt_lands_before_its_turn_rises_in() {
        let start = prompt_rise(ms(0)).unwrap();
        assert_eq!((start.travel, start.fade, start.reveal), (0., 0., 0.));
        // Mid rise the prompt moves and shows; the rest of the turn waits.
        let mid = prompt_rise(ms(280)).unwrap();
        assert!(mid.travel > 0.5 && mid.fade > 0. && mid.fade < 1.);
        assert_eq!(mid.reveal, 0.);
        // Landed: the turn reveals over the next 320ms, then it is done.
        let landed = prompt_rise(PROMPT_RISE).unwrap();
        assert_eq!((landed.travel, landed.fade, landed.reveal), (1., 1., 0.));
        assert!(prompt_rise(PROMPT_RISE + ms(160)).unwrap().reveal > 0.5);
        assert!(prompt_rise(PROMPT_RISE + PROMPT_REVEAL).is_none());
    }

    #[test]
    fn beats_follow_the_keyframe_delays() {
        // The slot opens over the first 40%.
        assert_eq!(beat(0., 0., 0.4, OPEN_EASE), 0.);
        assert_eq!(beat(0.4, 0., 0.4, OPEN_EASE), 1.);
        // The row does not move until halfway.
        assert_eq!(beat(0.5, 0.5, 0.5, RISE_EASE), 0.);
        assert!(beat(0.75, 0.5, 0.5, RISE_EASE) > 0.5);
        assert_eq!(beat(1., 0.5, 0.5, RISE_EASE), 1.);
        // The spine above runs on over 30% of its own pace.
        let start = Instant::now();
        assert_eq!(spine_run(ms(480), start, start), Some(0.));
        assert!(spine_run(ms(480), start, start + ms(145)).is_none());
    }
}
