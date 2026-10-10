//! Controller input: an aim ray per hand (smoothed), select (trigger or A),
//! back (B), scroll (thumbstick; grip held = faster), seek (left D-pad or a
//! sideways stick flick). Bindings are suggested for the Steam
//! Frame controller, Index controllers and the generic simple profile.
//!
//! The stick's up/down and sideways jobs are kept apart: when it leaves the
//! centre its direction is decided once (within 30° of vertical, or not),
//! and a sideways push gives no vertical output until it is centred again.

use super::context::XrContext;
use openxr as xr;

pub struct Ray {
    pub origin: [f32; 3],
    pub direction: [f32; 3],
}

#[derive(Default)]
pub struct InputState {
    /// Aim rays per hand (left, right), when tracked.
    pub rays: [Option<Ray>; 2],
    /// Select pressed this frame, per hand.
    pub select: [bool; 2],
    /// Select held down, per hand (dragging).
    pub select_held: [bool; 2],
    pub back: bool,
    /// Thumbstick pressed in (reset view) this frame.
    pub reset: bool,
    /// Grip (lower trigger) held on either hand: a modifier, e.g. fast scroll.
    pub grip: bool,
    /// Seek pressed this frame: -1 back, +1 forward (D-pad left/right, or a
    /// sideways thumbstick flick where the D-pad isn't available).
    pub seek: i32,
    /// Thumbstick vertical deflection per hand (left, right): -1..1, up
    /// positive, 0 in the dead zone or while the stick is pushed sideways.
    pub stick: [f32; 2],
    /// The stronger of the two `stick` values.
    pub scroll: f32,
}

/// Deflection below which the stick counts as centred.
const STICK_DEADZONE: f32 = 0.2;
/// A sideways push re-arms (and may go vertical) once back under this.
const STICK_RECENTRE: f32 = 0.3;
/// A sideways push this far flicks (seeks).
const FLICK: f32 = 0.8;
/// tan(30°): within this ratio of the axis counts as along it.
const SECTOR: f32 = 0.577_350_3;

/// What a hand's stick is doing, decided when it leaves the centre.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Stick {
    Centred,
    /// Pushed up or down: scroll, zoom or move.
    Vertical,
    /// Pushed sideways: a flick seeks once, and nothing scrolls.
    Sideways {
        fired: bool,
    },
}

impl Stick {
    /// Feeds one reading; returns the vertical output and whether a flick
    /// fired (-1, 0 or +1).
    fn update(&mut self, x: f32, y: f32) -> (f32, i32) {
        let mag = x.hypot(y);
        let vertical = x.abs() <= SECTOR * y.abs();
        let sideways = y.abs() <= SECTOR * x.abs();
        match *self {
            Stick::Centred if mag >= STICK_DEADZONE => {
                *self = if vertical {
                    Stick::Vertical
                } else {
                    Stick::Sideways { fired: false }
                };
                self.update(x, y)
            }
            Stick::Centred => (0.0, 0),
            Stick::Vertical if mag < STICK_DEADZONE => {
                *self = Stick::Centred;
                (0.0, 0)
            }
            Stick::Vertical if vertical => {
                // Ramps from 0 at the dead zone's edge.
                let out = (mag - STICK_DEADZONE) / (1.0 - STICK_DEADZONE);
                (out.min(1.0).copysign(y), 0)
            }
            Stick::Vertical => (0.0, 0),
            Stick::Sideways { .. } if mag < STICK_RECENTRE => {
                *self = Stick::Centred;
                (0.0, 0)
            }
            Stick::Sideways { fired: false } if sideways && mag > FLICK => {
                *self = Stick::Sideways { fired: true };
                (0.0, if x > 0.0 { 1 } else { -1 })
            }
            Stick::Sideways { .. } => (0.0, 0),
        }
    }
}

pub struct Input {
    set: xr::ActionSet,
    aim: xr::Action<xr::Posef>,
    select: xr::Action<bool>,
    back: xr::Action<bool>,
    reset: xr::Action<bool>,
    grip: xr::Action<bool>,
    seek_back: xr::Action<bool>,
    seek_forward: xr::Action<bool>,
    scroll: xr::Action<xr::Vector2f>,
    hands: [xr::Path; 2],
    spaces: [xr::Space; 2],
    /// Per hand: what the stick is doing.
    sticks: [Stick; 2],
    /// Per hand: origin and direction filters.
    filters: [[OneEuro; 2]; 2],
}

/// One Euro filter (Casiez et al.): heavy smoothing while still, little lag
/// when moving fast. Filters a 3-vector.
#[derive(Clone, Copy)]
struct OneEuro {
    min_cutoff: f32,
    beta: f32,
    value: Option<[f32; 3]>,
    speed: [f32; 3],
    time: i64,
}

impl OneEuro {
    const fn new(min_cutoff: f32, beta: f32) -> Self {
        Self {
            min_cutoff,
            beta,
            value: None,
            speed: [0.0; 3],
            time: 0,
        }
    }

    fn alpha(cutoff: f32, dt: f32) -> f32 {
        let tau = 1.0 / (2.0 * std::f32::consts::PI * cutoff);
        1.0 / (1.0 + tau / dt)
    }

    fn filter(&mut self, x: [f32; 3], time: i64) -> [f32; 3] {
        let Some(prev) = self.value else {
            self.value = Some(x);
            self.time = time;
            return x;
        };
        let dt = ((time - self.time) as f32 / 1e9).clamp(1e-4, 0.1);
        self.time = time;
        let a_d = Self::alpha(1.0, dt);
        let mut out = [0.0; 3];
        for i in 0..3 {
            let raw_speed = (x[i] - prev[i]) / dt;
            self.speed[i] += a_d * (raw_speed - self.speed[i]);
            let cutoff = self.min_cutoff + self.beta * self.speed[i].abs();
            out[i] = prev[i] + Self::alpha(cutoff, dt) * (x[i] - prev[i]);
        }
        self.value = Some(out);
        out
    }

    fn reset(&mut self) {
        self.value = None;
    }
}

fn rotate(q: xr::Quaternionf, v: [f32; 3]) -> [f32; 3] {
    // v' = v + 2w(q×v) + 2 q×(q×v)
    let (qx, qy, qz, w) = (q.x, q.y, q.z, q.w);
    let t = [
        2.0 * (qy * v[2] - qz * v[1]),
        2.0 * (qz * v[0] - qx * v[2]),
        2.0 * (qx * v[1] - qy * v[0]),
    ];
    [
        v[0] + w * t[0] + (qy * t[2] - qz * t[1]),
        v[1] + w * t[1] + (qz * t[0] - qx * t[2]),
        v[2] + w * t[2] + (qx * t[1] - qy * t[0]),
    ]
}

impl Input {
    pub fn new(ctx: &XrContext) -> anyhow::Result<Self> {
        let xr_ = &ctx.xr;
        let hands = [
            xr_.string_to_path("/user/hand/left")?,
            xr_.string_to_path("/user/hand/right")?,
        ];
        let set = xr_.create_action_set("player", "Player", 0)?;
        let aim = set.create_action::<xr::Posef>("aim", "Pointer", &hands)?;
        let select = set.create_action::<bool>("select", "Select", &hands)?;
        let back = set.create_action::<bool>("back", "Back", &hands)?;
        let reset = set.create_action::<bool>("reset", "Reset view", &hands)?;
        let grip = set.create_action::<bool>("grip", "Modifier", &hands)?;
        let seek_back = set.create_action::<bool>("seek_back", "Back 5 seconds", &hands)?;
        let seek_forward =
            set.create_action::<bool>("seek_forward", "Forward 5 seconds", &hands)?;
        let scroll = set.create_action::<xr::Vector2f>("scroll", "Scroll", &hands)?;

        let binding = |action: &str, path: xr::Path| match action {
            "aim" => xr::Binding::new(&aim, path),
            "select" => xr::Binding::new(&select, path),
            "back" => xr::Binding::new(&back, path),
            "reset" => xr::Binding::new(&reset, path),
            "grip" => xr::Binding::new(&grip, path),
            "seek_back" => xr::Binding::new(&seek_back, path),
            "seek_forward" => xr::Binding::new(&seek_forward, path),
            _ => xr::Binding::new(&scroll, path),
        };
        // Suggests every binding the runtime accepts for `profile` (each is
        // checked on its own first, since one bad path rejects the whole set).
        // An input is for both hands unless prefixed "left:" or "right:".
        // Nothing is suggested unless all of `required` were accepted, so a
        // profile we only half know can't replace a working one.
        let suggest =
            |profile: &str, wanted: &[(&str, &str)], required: &[&str]| -> anyhow::Result<usize> {
                let profile_path = xr_.string_to_path(profile)?;
                let mut accepted = Vec::new();
                for hand in ["left", "right"] {
                    for (action, input) in wanted {
                        let input = match input.split_once(':') {
                            Some((only, rest)) if only == hand => rest,
                            Some(_) => continue,
                            None => input,
                        };
                        let path =
                            xr_.string_to_path(&format!("/user/hand/{hand}/input/{input}"))?;
                        if xr_
                            .suggest_interaction_profile_bindings(
                                profile_path,
                                &[binding(action, path)],
                            )
                            .is_ok()
                        {
                            accepted.push((*action, path));
                        }
                    }
                }
                if let Some(missing) = required
                    .iter()
                    .find(|r| !accepted.iter().any(|(a, _)| a == *r))
                {
                    anyhow::bail!("no binding for {missing}");
                }
                let list: Vec<_> = accepted.iter().map(|(a, p)| binding(a, *p)).collect();
                if !list.is_empty() {
                    xr_.suggest_interaction_profile_bindings(profile_path, &list)?;
                }
                Ok(list.len())
            };
        let required = ["aim", "select", "back"];
        // Steam Frame controllers: A/B on the right, a D-pad on the left.
        let frame = [
            ("aim", "aim/pose"),
            ("select", "trigger/click"),
            ("select", "trigger/value"),
            ("select", "right:a/click"),
            ("back", "right:b/click"),
            ("scroll", "thumbstick"),
            ("reset", "thumbstick/click"),
            ("grip", "squeeze/click"),
            ("grip", "squeeze/value"),
            ("grip", "grip/click"),
            ("seek_back", "left:dpad_left/click"),
            ("seek_forward", "left:dpad_right/click"),
        ];
        for profile in [
            "/interaction_profiles/valve/frame_controller",
            "/interaction_profiles/valve/frame_controller_valve",
        ] {
            match suggest(profile, &frame, &required) {
                Ok(n) => eprintln!("Input: {profile}: {n} bindings"),
                Err(e) => eprintln!("Input: {profile} not used: {e}"),
            }
        }
        // Index bindings, which SteamVR maps Frame controllers onto when the
        // native profile isn't available. The Frame's left D-pad arrives as
        // the left A (down) and B (left/right/up), so A and B count only on
        // the right: the D-pad must not stop the video.
        let index = [
            ("aim", "aim/pose"),
            ("select", "trigger/click"),
            ("select", "trigger/value"),
            ("select", "right:a/click"),
            ("back", "right:b/click"),
            ("scroll", "thumbstick"),
            ("reset", "thumbstick/click"),
            ("grip", "squeeze/click"),
            ("grip", "squeeze/value"),
        ];
        match suggest(
            "/interaction_profiles/valve/index_controller",
            &index,
            &required,
        ) {
            Ok(n) => eprintln!("Input: index_controller: {n} bindings"),
            Err(e) => eprintln!("Input: index_controller not used: {e}"),
        }
        suggest(
            "/interaction_profiles/khr/simple_controller",
            &[
                ("aim", "aim/pose"),
                ("select", "select/click"),
                ("back", "menu/click"),
            ],
            &[],
        )?;
        ctx.session.attach_action_sets(&[&set])?;
        let spaces = [
            aim.create_space(&ctx.session, hands[0], xr::Posef::IDENTITY)?,
            aim.create_space(&ctx.session, hands[1], xr::Posef::IDENTITY)?,
        ];
        Ok(Self {
            set,
            aim,
            select,
            back,
            reset,
            grip,
            seek_back,
            seek_forward,
            scroll,
            hands,
            spaces,
            sticks: [Stick::Centred; 2],
            // Direction: unit vector (rad/s-ish speeds); origin: metres.
            filters: [[OneEuro::new(3.0, 6.0), OneEuro::new(1.5, 0.6)]; 2],
        })
    }

    pub fn poll(
        &mut self,
        ctx: &XrContext,
        space: &xr::Space,
        time: xr::Time,
    ) -> anyhow::Result<InputState> {
        ctx.session.sync_actions(&[(&self.set).into()])?;
        let mut state = InputState::default();
        let pressed = |action: &xr::Action<bool>, hand: xr::Path| -> anyhow::Result<bool> {
            let s = action.state(&ctx.session, hand)?;
            Ok(s.is_active && s.current_state && s.changed_since_last_sync)
        };
        for (i, &hand) in self.hands.iter().enumerate() {
            state.select[i] = pressed(&self.select, hand)?;
            let held = self.select.state(&ctx.session, hand)?;
            state.select_held[i] = held.is_active && held.current_state;
            state.back |= pressed(&self.back, hand)?;
            state.reset |= pressed(&self.reset, hand)?;
            let grip = self.grip.state(&ctx.session, hand)?;
            state.grip |= grip.is_active && grip.current_state;
            if pressed(&self.seek_back, hand)? {
                state.seek = -1;
            }
            if pressed(&self.seek_forward, hand)? {
                state.seek = 1;
            }
            let stick = self.scroll.state(&ctx.session, hand)?;
            if stick.is_active {
                let (out, flick) =
                    self.sticks[i].update(stick.current_state.x, stick.current_state.y);
                state.stick[i] = out;
                if out.abs() > state.scroll.abs() {
                    state.scroll = out;
                }
                if flick != 0 {
                    state.seek = flick;
                }
            }
            if self.aim.is_active(&ctx.session, hand)? {
                let location = self.spaces[i].locate(space, time)?;
                let flags = location.location_flags;
                if flags.contains(xr::SpaceLocationFlags::POSITION_VALID)
                    && flags.contains(xr::SpaceLocationFlags::ORIENTATION_VALID)
                {
                    let p = location.pose.position;
                    let t = time.as_nanos();
                    let origin = self.filters[i][0].filter([p.x, p.y, p.z], t);
                    let d = self.filters[i][1]
                        .filter(rotate(location.pose.orientation, [0.0, 0.0, -1.0]), t);
                    let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt().max(1e-6);
                    state.rays[i] = Some(Ray {
                        origin,
                        direction: [d[0] / len, d[1] / len, d[2] / len],
                    });
                } else {
                    self.filters[i][0].reset();
                    self.filters[i][1].reset();
                }
            }
        }
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_euro_smooths_jitter_but_follows_motion() {
        let mut f = OneEuro::new(3.0, 6.0);
        let step = 11_111_111; // 90 Hz
        // Holding still with ±0.004 noise (~0.25° on a unit vector).
        let mut max_dev = 0.0f32;
        for i in 0..200 {
            let noise = if i % 2 == 0 { 0.004 } else { -0.004 };
            let out = f.filter([noise, 0.0, -1.0], i * step);
            if i > 20 {
                max_dev = max_dev.max(out[0].abs());
            }
        }
        assert!(max_dev < 0.001, "jitter left: {max_dev}");
        // A fast turn is followed within a few frames.
        let mut out = [0.0; 3];
        for i in 200..210 {
            out = f.filter([0.5, 0.0, -0.8], i * step);
        }
        assert!((out[0] - 0.5).abs() < 0.05, "lagging: {out:?}");
    }

    #[test]
    fn rotation_turns_forward_vector() {
        // 90° yaw to the left (about +Y) maps -Z forward to -X.
        let half = std::f32::consts::FRAC_PI_4;
        let q = xr::Quaternionf {
            x: 0.0,
            y: half.sin(),
            z: 0.0,
            w: half.cos(),
        };
        let v = rotate(q, [0.0, 0.0, -1.0]);
        assert!(
            (v[0] + 1.0).abs() < 1e-5 && v[1].abs() < 1e-5 && v[2].abs() < 1e-5,
            "{v:?}"
        );
    }
}
