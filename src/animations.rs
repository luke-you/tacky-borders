use serde::Deserialize;
use std::sync::Arc;
use std::time;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;

use windows_numerics::{Matrix3x2, Vector2};

use crate::anim_timer::AnimationTimer;
use crate::colors::ColorBrush;
use crate::config::{serde_default_bool, serde_default_i32};
use crate::utils::cubic_bezier;
use crate::window_border::WindowState;

pub const MIN_ANIMATION_FPS: i32 = 1;
pub const MAX_ANIMATION_FPS: i32 = 1000;

pub fn frame_interval_for_fps(
    fps: i32,
) -> Result<time::Duration, crate::anim_timer::AnimationTimerExit> {
    if !(MIN_ANIMATION_FPS..=MAX_ANIMATION_FPS).contains(&fps) {
        return Err(crate::anim_timer::AnimationTimerExit::InvalidFrameRate(fps));
    }

    Ok(time::Duration::from_secs_f64(1.0 / f64::from(fps)))
}

fn deserialize_animation_fps<'de, D>(deserializer: D) -> Result<i32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // Defer malformed timing values to validation so disabled blocks can ignore them.
    let value = serde_yaml_ng::Value::deserialize(deserializer)?;
    Ok(serde_yaml_ng::from_value(value).unwrap_or(i32::MIN))
}

fn deserialize_animation_duration<'de, D>(deserializer: D) -> Result<Option<f32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_yaml_ng::Value>::deserialize(deserializer)?;
    Ok(value.map(|value| {
        let duration: f32 = serde_yaml_ng::from_value(value).unwrap_or(0.0);
        if duration.is_finite() { duration } else { 0.0 }
    }))
}

fn progress_delta(elapsed: time::Duration, duration_ms: f32) -> f64 {
    elapsed.as_secs_f64() * 1000.0 / f64::from(duration_ms)
}

#[derive(Debug, Default, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AnimationsConfig {
    #[serde(default)]
    active: Vec<AnimParamsConfig>,
    #[serde(default)]
    inactive: Vec<AnimParamsConfig>,
    #[serde(
        default = "serde_default_i32::<60>",
        deserialize_with = "deserialize_animation_fps"
    )]
    fps: i32,
    #[serde(default = "serde_default_bool::<true>")]
    enabled: bool,
}

impl AnimationsConfig {
    pub(crate) fn validate(&self, path: &str) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }

        anyhow::ensure!(
            (MIN_ANIMATION_FPS..=MAX_ANIMATION_FPS).contains(&self.fps),
            "{path}.fps must be an integer from {MIN_ANIMATION_FPS} through \
             {MAX_ANIMATION_FPS}; received {}",
            self.fps
        );

        for (list_name, animations) in [("active", &self.active), ("inactive", &self.inactive)] {
            for (index, animation) in animations.iter().enumerate() {
                if let Some(duration) = animation.duration {
                    anyhow::ensure!(
                        duration.is_finite() && duration > 0.0,
                        "{path}.{list_name}[{}].duration must be finite and greater than zero milliseconds; received {duration}",
                        index + 1
                    );
                }
            }
        }
        Ok(())
    }

    pub fn to_animations(&self) -> Animations {
        if self.enabled {
            Animations {
                active: self
                    .active
                    .iter()
                    .map(|params_config| params_config.to_anim_params())
                    .collect(),
                inactive: self
                    .inactive
                    .iter()
                    .map(|params_config| params_config.to_anim_params())
                    .collect(),
                fps: self.fps,
                ..Default::default()
            }
        } else {
            Animations::default()
        }
    }
}

#[derive(Debug, Default)]
pub struct Animations {
    pub active: Vec<AnimParams>,
    pub inactive: Vec<AnimParams>,
    pub timer: Option<AnimationTimer>,
    pub fps: i32,
    pub fade_progress: f32,
    pub spiral_progress: f32,
}

impl Animations {
    pub fn animate_spiral(
        &mut self,
        bounds: &D2D_RECT_F,
        active_color: &ColorBrush,
        inactive_color: &ColorBrush,
        anim_elapsed: &time::Duration,
        anim_params: &AnimParams,
    ) {
        let direction = if anim_params.anim_type == AnimType::ReverseSpiral {
            -1.0
        } else {
            1.0
        };

        let delta_x = progress_delta(*anim_elapsed, anim_params.duration) * direction;
        let progress = f64::from(self.spiral_progress) + delta_x;

        self.spiral_progress = if (0.0..=1.0).contains(&progress) {
            progress as f32
        } else {
            progress.rem_euclid(1.0) as f32
        };

        let y_coord = anim_params.easing_fn.as_ref()(self.spiral_progress);

        // Calculate the center point of the bounds
        let center_x = bounds.left + ((bounds.right - bounds.left) / 2.0);
        let center_y = bounds.top + ((bounds.bottom - bounds.top) / 2.0);

        let transform = Matrix3x2::rotation_around(
            360.0 * y_coord,
            Vector2 {
                X: center_x,
                Y: center_y,
            },
        );

        active_color.set_transform(&transform);
        inactive_color.set_transform(&transform);
    }

    pub fn animate_fade(
        &mut self,
        window_state: WindowState,
        active_color: &ColorBrush,
        inactive_color: &ColorBrush,
        anim_elapsed: &time::Duration,
        anim_params: &AnimParams,
    ) -> anyhow::Result<()> {
        let prev_active_opacity = active_color.get_opacity()?;
        let prev_inactive_opacity = inactive_color.get_opacity()?;

        // We reset 'fade_progress' if either color has 0 opacity (i.e. when the animation is not
        // in progress). This ensures we start from the correct position in the following cases:
        // 1. The border has just been created.
        // 2. The fade progress hasn't updated properly, which can occur if only one of the
        //    active/inactive animations lists contains the fade animation type.
        // NOTE: opacities can be negative
        if prev_active_opacity == 0.0 || prev_inactive_opacity == 0.0 {
            self.fade_progress = match window_state {
                WindowState::Active => 0.0,
                WindowState::Inactive => 1.0,
            };
        }

        // Determine which direction we should move fade_progress
        let direction = match window_state {
            WindowState::Active => 1.0,
            WindowState::Inactive => -1.0,
        };

        let delta_x = progress_delta(*anim_elapsed, anim_params.duration) * f64::from(direction);
        let progress = f64::from(self.fade_progress) + delta_x;

        // Check if the fade animation is finished
        if !(0.0..=1.0).contains(&progress) {
            let final_opacity = progress.clamp(0.0, 1.0) as f32;

            active_color.set_opacity(final_opacity)?;
            inactive_color.set_opacity(1.0 - final_opacity)?;

            self.fade_progress = final_opacity;
            return Ok(());
        }

        self.fade_progress = progress as f32;
        let y_coord = anim_params.easing_fn.as_ref()(self.fade_progress);

        // Don't question it; trust the process
        // Ok, it's mainly done this way to handle edge cases when the border has just been created
        let active_opacity_diff = f32::min(
            f32::abs(y_coord - prev_active_opacity),
            f32::abs(y_coord - (1.0 - prev_inactive_opacity)),
        ) * direction;

        // Clamp opacities from the negative direction. We use MAX_NEGATIVE instead of 0.0 so we
        // don't enter the if statement at the start of animate_fade() (jank ik)
        const MAX_NEGATIVE: f32 = -f32::MIN_POSITIVE;
        let new_active_opacity = f32::max(prev_active_opacity + active_opacity_diff, MAX_NEGATIVE);
        let new_inactive_opacity =
            f32::max(prev_inactive_opacity - active_opacity_diff, MAX_NEGATIVE);

        active_color.set_opacity(new_active_opacity)?;
        inactive_color.set_opacity(new_inactive_opacity)?;

        Ok(())
    }

    pub fn get_current(&self, window_state: WindowState) -> &Vec<AnimParams> {
        match window_state {
            WindowState::Active => &self.active,
            WindowState::Inactive => &self.inactive,
        }
    }

    pub fn set_timer_if_needed(
        &mut self,
        border_window: HWND,
        last_anim_time: &mut Option<time::Instant>,
    ) -> Result<(), crate::anim_timer::AnimationTimerExit> {
        if self.timer.is_none() && (!self.active.is_empty() || !self.inactive.is_empty()) {
            self.timer = Some(AnimationTimer::new(
                border_window,
                frame_interval_for_fps(self.fps)?,
            )?);

            *last_anim_time = Some(time::Instant::now());
        }

        Ok(())
    }

    pub fn destroy_timer(&mut self) -> Result<(), crate::anim_timer::AnimationTimerExit> {
        if let Some(mut timer) = self.timer.take() {
            match timer.stop_and_join() {
                crate::anim_timer::AnimationTimerExit::Stopped => {}
                failure => return Err(failure.clone()),
            }
        }
        Ok(())
    }

    pub fn update_fade_progress(&mut self, window_state: WindowState) {
        self.fade_progress = match window_state {
            WindowState::Active => 1.0,
            WindowState::Inactive => 0.0,
        };
    }
}

#[cfg(test)]
mod timing_tests {
    use super::*;

    #[test]
    fn frame_interval_covers_safe_fps_boundaries() {
        assert_eq!(
            frame_interval_for_fps(MIN_ANIMATION_FPS).unwrap(),
            time::Duration::from_secs(1)
        );
        assert_eq!(
            frame_interval_for_fps(MAX_ANIMATION_FPS).unwrap(),
            time::Duration::from_millis(1)
        );
        assert!(frame_interval_for_fps(0).is_err());
        assert!(frame_interval_for_fps(-1).is_err());
        assert!(frame_interval_for_fps(MAX_ANIMATION_FPS + 1).is_err());
    }

    #[test]
    fn smallest_positive_duration_has_finite_progress_delta() {
        let duration = f32::from_bits(1);
        let delta = progress_delta(time::Duration::from_millis(1), duration);
        assert!(delta.is_finite());
        assert!(delta > 0.0);

        let longest = progress_delta(time::Duration::from_millis(1), f32::MAX);
        assert!(longest.is_finite());
        assert!(longest > 0.0);
    }

    #[test]
    fn enabled_timing_validation_rejects_unsafe_values() {
        let mut config = AnimationsConfig {
            fps: MIN_ANIMATION_FPS,
            enabled: true,
            ..Default::default()
        };
        config.validate("global.animations").unwrap();

        config.fps = MAX_ANIMATION_FPS;
        config.validate("global.animations").unwrap();

        for fps in [i32::MIN, -1, 0, MAX_ANIMATION_FPS + 1, i32::MAX] {
            config.fps = fps;
            let error = config.validate("global.animations").unwrap_err();
            assert!(format!("{error:#}").contains("global.animations.fps"));
        }

        config.fps = MIN_ANIMATION_FPS;
        for duration in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            config.active = vec![AnimParamsConfig {
                anim_type: AnimType::Fade,
                duration: Some(duration),
                easing: None,
            }];
            let error = config.validate("global.animations").unwrap_err();
            assert!(format!("{error:#}").contains("global.animations.active[1].duration"));
        }
    }

    #[test]
    fn disabled_timing_validation_ignores_invalid_values() {
        let config = AnimationsConfig {
            active: vec![AnimParamsConfig {
                anim_type: AnimType::Fade,
                duration: Some(f32::NAN),
                easing: None,
            }],
            fps: 0,
            enabled: false,
            ..Default::default()
        };

        config.validate("global.animations").unwrap();
        assert!(config.to_animations().active.is_empty());
    }

    #[test]
    fn disabled_timing_fields_accept_unparseable_yaml_values() {
        let config: AnimationsConfig = serde_yaml_ng::from_str(
            "enabled: false\nfps: not-a-number\nactive:\n  - type: Fade\n    duration: not-a-duration\n",
        )
        .unwrap();

        config.validate("global.animations").unwrap();
        assert!(config.to_animations().active.is_empty());
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AnimParamsConfig {
    #[serde(rename = "type")]
    pub anim_type: AnimType,
    #[serde(default, deserialize_with = "deserialize_animation_duration")]
    pub duration: Option<f32>,
    pub easing: Option<AnimEasing>,
}

impl AnimParamsConfig {
    fn to_anim_params(&self) -> AnimParams {
        let duration = self.duration.unwrap_or(match self.anim_type {
            AnimType::Spiral | AnimType::ReverseSpiral => 1800.0,
            AnimType::Fade => 200.0,
        });

        let easing = self.easing.unwrap_or_default();
        let easing_function = cubic_bezier(&easing.to_points()).unwrap();

        AnimParams {
            anim_type: self.anim_type,
            duration,
            easing_fn: Arc::new(easing_function),
        }
    }
}

#[derive(Clone)]
pub struct AnimParams {
    pub anim_type: AnimType,
    pub duration: f32,
    pub easing_fn: Arc<dyn Fn(f32) -> f32 + Send + Sync>,
}

// We must manually implement Debug for AnimParams because Fn(f32) -> f32 doesn't implement it
impl std::fmt::Debug for AnimParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnimParams")
            .field("type", &self.anim_type)
            .field("duration", &self.duration)
            .field("easing_fn", &Arc::as_ptr(&self.easing_fn))
            .finish()
    }
}

pub trait AnimVec {
    fn contains_type(&self, anim_type: AnimType) -> bool;
}

impl AnimVec for Vec<AnimParams> {
    fn contains_type(&self, anim_type: AnimType) -> bool {
        self.iter()
            .any(|anim_params| anim_params.anim_type == anim_type)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum AnimType {
    Spiral,
    ReverseSpiral,
    Fade,
}

// Thanks to 0xJWLabs for the AnimEasing enum along with its methods
#[derive(Debug, Default, Clone, Copy, Deserialize, PartialEq)]
pub enum AnimEasing {
    // Linear
    #[default]
    Linear,

    // EaseIn variants
    EaseIn,
    EaseInSine,
    EaseInQuad,
    EaseInCubic,
    EaseInQuart,
    EaseInQuint,
    EaseInExpo,
    EaseInCirc,
    EaseInBack,

    // EaseOut variants
    EaseOut,
    EaseOutSine,
    EaseOutQuad,
    EaseOutCubic,
    EaseOutQuart,
    EaseOutQuint,
    EaseOutExpo,
    EaseOutCirc,
    EaseOutBack,

    // EaseInOut variants
    EaseInOut,
    EaseInOutSine,
    EaseInOutQuad,
    EaseInOutCubic,
    EaseInOutQuart,
    EaseInOutQuint,
    EaseInOutExpo,
    EaseInOutCirc,
    EaseInOutBack,

    #[serde(untagged)]
    CubicBezier([f32; 4]),
}

impl AnimEasing {
    /// Converts the easing to a corresponding array of points.
    /// Linear and named easing variants will return predefined control points,
    /// while CubicBezier returns its own array.
    pub fn to_points(self) -> [f32; 4] {
        match self {
            // Linear
            AnimEasing::Linear => [0.0, 0.0, 1.0, 1.0],

            // EaseIn variants
            AnimEasing::EaseIn => [0.42, 0.0, 1.0, 1.0],
            AnimEasing::EaseInSine => [0.12, 0.0, 0.39, 0.0],
            AnimEasing::EaseInQuad => [0.11, 0.0, 0.5, 0.0],
            AnimEasing::EaseInCubic => [0.32, 0.0, 0.67, 0.0],
            AnimEasing::EaseInQuart => [0.5, 0.0, 0.75, 0.0],
            AnimEasing::EaseInQuint => [0.64, 0.0, 0.78, 0.0],
            AnimEasing::EaseInExpo => [0.7, 0.0, 0.84, 0.0],
            AnimEasing::EaseInCirc => [0.55, 0.0, 1.0, 0.45],
            AnimEasing::EaseInBack => [0.36, 0.0, 0.66, -0.56],

            // EaseOut variants
            AnimEasing::EaseOut => [0.0, 0.0, 0.58, 1.0],
            AnimEasing::EaseOutSine => [0.61, 1.0, 0.88, 1.0],
            AnimEasing::EaseOutQuad => [0.5, 1.0, 0.89, 1.0],
            AnimEasing::EaseOutCubic => [0.33, 1.0, 0.68, 1.0],
            AnimEasing::EaseOutQuart => [0.25, 1.0, 0.5, 1.0],
            AnimEasing::EaseOutQuint => [0.22, 1.0, 0.36, 1.0],
            AnimEasing::EaseOutExpo => [0.16, 1.0, 0.3, 1.0],
            AnimEasing::EaseOutCirc => [0.0, 0.55, 0.45, 1.0],
            AnimEasing::EaseOutBack => [0.34, 1.56, 0.64, 1.0],

            // EaseInOut variants
            AnimEasing::EaseInOut => [0.42, 0.0, 0.58, 1.0],
            AnimEasing::EaseInOutSine => [0.37, 0.0, 0.63, 1.0],
            AnimEasing::EaseInOutQuad => [0.45, 0.0, 0.55, 1.0],
            AnimEasing::EaseInOutCubic => [0.65, 0.0, 0.35, 1.0],
            AnimEasing::EaseInOutQuart => [0.76, 0.0, 0.24, 1.0],
            AnimEasing::EaseInOutQuint => [0.83, 0.0, 0.17, 1.0],
            AnimEasing::EaseInOutExpo => [0.87, 0.0, 0.13, 1.0],
            AnimEasing::EaseInOutCirc => [0.85, 0.0, 0.15, 1.0],
            AnimEasing::EaseInOutBack => [0.68, -0.6, 0.32, 1.6],

            // CubicBezier variant returns its own points.
            AnimEasing::CubicBezier(bezier) => bezier,
        }
    }
}
