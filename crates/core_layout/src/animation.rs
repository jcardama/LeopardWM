// ============================================================================
// Animation Support
// ============================================================================

/// Easing function types for animations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Easing {
    /// Linear interpolation (constant speed).
    Linear,
    /// Smooth deceleration (starts fast, ends slow).
    #[default]
    EaseOut,
    /// Smooth acceleration (starts slow, ends fast).
    EaseIn,
    /// Smooth acceleration and deceleration.
    EaseInOut,
}

impl Easing {
    /// Apply the easing function to a progress value (0.0 to 1.0).
    /// Returns the eased progress value (0.0 to 1.0).
    pub fn apply(&self, t: f64) -> f64 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Easing::Linear => t,
            Easing::EaseOut => 1.0 - (1.0 - t).powi(3), // Cubic ease out
            Easing::EaseIn => t.powi(3),                // Cubic ease in
            Easing::EaseInOut => {
                // Cubic ease in-out
                if t < 0.5 {
                    4.0 * t.powi(3)
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
                }
            }
        }
    }
}

/// Duration of scroll animations in milliseconds.
pub const DEFAULT_ANIMATION_DURATION_MS: u64 = 200;

/// Animation state for smooth scrolling.
#[derive(Debug, Clone)]
pub struct ScrollAnimation {
    /// Starting scroll offset.
    pub start_offset: f64,
    /// Target scroll offset.
    pub target_offset: f64,
    /// Animation duration in milliseconds.
    pub duration_ms: u64,
    /// Elapsed time in milliseconds.
    pub elapsed_ms: u64,
    /// Easing function to use.
    pub easing: Easing,
}

impl ScrollAnimation {
    /// Create a new scroll animation.
    pub fn new(start: f64, target: f64, duration_ms: u64, easing: Easing) -> Self {
        Self {
            start_offset: start,
            target_offset: target,
            duration_ms,
            elapsed_ms: 0,
            easing,
        }
    }

    /// Create a new animation with default duration and easing.
    pub fn with_defaults(start: f64, target: f64) -> Self {
        Self::new(
            start,
            target,
            DEFAULT_ANIMATION_DURATION_MS,
            Easing::default(),
        )
    }

    /// Check if the animation is complete.
    pub fn is_complete(&self) -> bool {
        self.elapsed_ms >= self.duration_ms
    }

    /// Get the current progress (0.0 to 1.0).
    pub fn progress(&self) -> f64 {
        if self.duration_ms == 0 {
            return 1.0;
        }
        (self.elapsed_ms as f64 / self.duration_ms as f64).clamp(0.0, 1.0)
    }

    /// Get the current scroll offset based on animation progress.
    pub fn current_offset(&self) -> f64 {
        let eased_progress = self.easing.apply(self.progress());
        self.start_offset + (self.target_offset - self.start_offset) * eased_progress
    }

    /// Advance the animation by the given delta time in milliseconds.
    /// Returns true if the animation is still running, false if complete.
    pub fn tick(&mut self, delta_ms: u64) -> bool {
        self.elapsed_ms = self.elapsed_ms.saturating_add(delta_ms);
        !self.is_complete()
    }

    /// Get the final target offset.
    pub fn target(&self) -> f64 {
        self.target_offset
    }
}
