//! Intervention observer — learns human override patterns at phase gates.
//!
//! Observes whether the user intervenes at phase transitions and adjusts
//! autonomy recommendations over time.
//!
//! Each phase transition is one data point:
//! - Transition with no human intervention → "auto" data point
//! - Transition with human intervention → "override" data point
//! - Tool calls *within* a phase are NOT counted (they're execution,
//!   not gating decisions)
//!
//! Cold start: defaults to cautious. Converges within 10-20 sessions.
//! See #242 for the full design.

use crate::task_phase::TaskPhase;
use std::collections::HashMap;
use std::path::PathBuf;

/// Per-phase override frequency tracker.
///
/// Starts coarse (6 cells, one per phase). Each cell tracks:
/// - `auto_count`: transitions where the agent proceeded without human input
/// - `override_count`: transitions where the human intervened (approved,
///   rejected, or edited the plan)
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InterventionObserver {
    /// Phase → (auto_count, override_count)
    phase_stats: HashMap<String, (u32, u32)>,
}

impl InterventionObserver {
    /// Create a new observer with zero data.
    pub fn new() -> Self {
        Self {
            phase_stats: HashMap::new(),
        }
    }

    /// Record an "auto" data point: the agent transitioned without human input.
    pub fn record_auto(&mut self, phase: TaskPhase) {
        let entry = self.phase_stats.entry(phase.to_string()).or_insert((0, 0));
        entry.0 += 1;
    }

    /// Record an "override" data point: the human intervened at this phase gate.
    pub fn record_override(&mut self, phase: TaskPhase) {
        let entry = self.phase_stats.entry(phase.to_string()).or_insert((0, 0));
        entry.1 += 1;
    }

    /// Get the autonomy recommendation for a phase.
    ///
    /// Returns a value between 0.0 (always confirm) and 1.0 (always auto).
    /// With insufficient data, returns 0.0 (cautious default).
    pub fn autonomy_score(&self, phase: TaskPhase) -> f32 {
        let key = phase.to_string();
        let (auto_count, override_count) = self.phase_stats.get(&key).copied().unwrap_or((0, 0));
        let total = auto_count + override_count;

        if total < Self::MIN_DATA_POINTS {
            return 0.0; // insufficient data → cautious
        }

        auto_count as f32 / total as f32
    }

    /// Whether the observer has enough data to make recommendations for a phase.
    pub fn has_sufficient_data(&self, phase: TaskPhase) -> bool {
        let key = phase.to_string();
        let (auto, over) = self.phase_stats.get(&key).copied().unwrap_or((0, 0));
        (auto + over) >= Self::MIN_DATA_POINTS
    }

    /// Whether the observer recommends auto-approval for this phase.
    ///
    /// True if the user has historically not intervened at this phase
    /// gate (autonomy score > threshold with sufficient data).
    pub fn recommends_auto(&self, phase: TaskPhase) -> bool {
        self.has_sufficient_data(phase) && self.autonomy_score(phase) > Self::AUTO_THRESHOLD
    }

    /// Summary of all phase stats (for `koda priors show`).
    pub fn summary(&self) -> Vec<PhasePrior> {
        let phases = [
            TaskPhase::Understanding,
            TaskPhase::Planning,
            TaskPhase::Reviewing,
            TaskPhase::Executing,
            TaskPhase::Verifying,
            TaskPhase::Reporting,
        ];

        phases
            .iter()
            .map(|&phase| {
                let key = phase.to_string();
                let (auto, over) = self.phase_stats.get(&key).copied().unwrap_or((0, 0));
                PhasePrior {
                    phase,
                    auto_count: auto,
                    override_count: over,
                    autonomy_score: self.autonomy_score(phase),
                    recommends_auto: self.recommends_auto(phase),
                }
            })
            .collect()
    }

    /// Reset all learned data.
    pub fn reset(&mut self) {
        self.phase_stats.clear();
    }

    /// Minimum data points before making recommendations.
    const MIN_DATA_POINTS: u32 = 5;

    /// Autonomy score threshold for auto-approval.
    /// 0.8 = user didn't intervene in 80%+ of transitions.
    const AUTO_THRESHOLD: f32 = 0.8;

    // ── Persistence ────────────────────────────────────────

    /// Load from disk, or create new if not found.
    pub fn load() -> Self {
        let path = Self::storage_path();
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| Self::new()),
            Err(_) => Self::new(),
        }
    }

    /// Save to disk. Logs a warning if persistence fails.
    pub fn save(&self) {
        let path = Self::storage_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    tracing::warn!(
                        "Failed to save intervention priors to {}: {e}",
                        path.display()
                    );
                }
            }
            Err(e) => {
                tracing::warn!("Failed to serialize intervention priors: {e}");
            }
        }
    }

    /// Load from disk with auto-save on drop.
    pub fn load_auto_save() -> ObserverGuard {
        ObserverGuard {
            observer: Self::load(),
        }
    }

    fn storage_path() -> PathBuf {
        let config_dir = std::env::var("XDG_CONFIG_HOME")
            .or_else(|_| std::env::var("HOME").map(|h| format!("{h}/.config")))
            .or_else(|_| std::env::var("USERPROFILE").map(|h| format!("{h}/.config")))
            .unwrap_or_else(|_| ".".to_string());
        PathBuf::from(config_dir)
            .join("koda")
            .join("intervention_priors.json")
    }
}

impl Default for InterventionObserver {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII wrapper that saves the observer on drop.
///
/// Use `InterventionObserver::load_auto_save()` to get one.
/// The observer is saved when this guard goes out of scope,
/// guaranteeing persistence even on early returns.
pub struct ObserverGuard {
    pub observer: InterventionObserver,
}

impl std::ops::Deref for ObserverGuard {
    type Target = InterventionObserver;
    fn deref(&self) -> &Self::Target {
        &self.observer
    }
}

impl std::ops::DerefMut for ObserverGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.observer
    }
}

impl Drop for ObserverGuard {
    fn drop(&mut self) {
        self.observer.save();
    }
}

/// Summary of learned priors for one phase (for display).
#[derive(Debug, Clone)]
pub struct PhasePrior {
    pub phase: TaskPhase,
    pub auto_count: u32,
    pub override_count: u32,
    pub autonomy_score: f32,
    pub recommends_auto: bool,
}

impl std::fmt::Display for PhasePrior {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let total = self.auto_count + self.override_count;
        let status = if total < InterventionObserver::MIN_DATA_POINTS {
            "insufficient data".to_string()
        } else if self.recommends_auto {
            format!("auto ({:.0}%)", self.autonomy_score * 100.0)
        } else {
            format!("cautious ({:.0}%)", self.autonomy_score * 100.0)
        };
        write!(
            f,
            "{:>13}  {:>3} auto / {:>3} override  {}",
            self.phase.to_string(),
            self.auto_count,
            self.override_count,
            status,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_is_empty() {
        let obs = InterventionObserver::new();
        assert_eq!(obs.autonomy_score(TaskPhase::Reviewing), 0.0);
        assert!(!obs.has_sufficient_data(TaskPhase::Reviewing));
        assert!(!obs.recommends_auto(TaskPhase::Reviewing));
    }

    #[test]
    fn test_record_auto_builds_data() {
        let mut obs = InterventionObserver::new();
        for _ in 0..10 {
            obs.record_auto(TaskPhase::Executing);
        }
        assert!(obs.has_sufficient_data(TaskPhase::Executing));
        assert_eq!(obs.autonomy_score(TaskPhase::Executing), 1.0);
        assert!(obs.recommends_auto(TaskPhase::Executing));
    }

    #[test]
    fn test_mixed_signals() {
        let mut obs = InterventionObserver::new();
        for _ in 0..8 {
            obs.record_auto(TaskPhase::Reviewing);
        }
        for _ in 0..2 {
            obs.record_override(TaskPhase::Reviewing);
        }
        assert!(obs.has_sufficient_data(TaskPhase::Reviewing));
        assert_eq!(obs.autonomy_score(TaskPhase::Reviewing), 0.8);
        // 0.8 == threshold, not > threshold
        assert!(!obs.recommends_auto(TaskPhase::Reviewing));
    }

    #[test]
    fn test_mostly_auto_recommends_auto() {
        let mut obs = InterventionObserver::new();
        for _ in 0..9 {
            obs.record_auto(TaskPhase::Reviewing);
        }
        obs.record_override(TaskPhase::Reviewing);
        // 9/10 = 0.9 > 0.8 threshold
        assert!(obs.recommends_auto(TaskPhase::Reviewing));
    }

    #[test]
    fn test_insufficient_data_is_cautious() {
        let mut obs = InterventionObserver::new();
        for _ in 0..4 {
            obs.record_auto(TaskPhase::Reviewing);
        }
        // 4 < MIN_DATA_POINTS (5)
        assert!(!obs.has_sufficient_data(TaskPhase::Reviewing));
        assert!(!obs.recommends_auto(TaskPhase::Reviewing));
    }

    #[test]
    fn test_phases_are_independent() {
        let mut obs = InterventionObserver::new();
        for _ in 0..10 {
            obs.record_auto(TaskPhase::Executing);
        }
        // Executing has data, Reviewing doesn't
        assert!(obs.recommends_auto(TaskPhase::Executing));
        assert!(!obs.recommends_auto(TaskPhase::Reviewing));
    }

    #[test]
    fn test_reset_clears_data() {
        let mut obs = InterventionObserver::new();
        for _ in 0..10 {
            obs.record_auto(TaskPhase::Executing);
        }
        assert!(obs.recommends_auto(TaskPhase::Executing));
        obs.reset();
        assert!(!obs.recommends_auto(TaskPhase::Executing));
    }

    #[test]
    fn test_summary_covers_all_phases() {
        let obs = InterventionObserver::new();
        let summary = obs.summary();
        assert_eq!(summary.len(), 6);
    }

    #[test]
    fn test_serialization_roundtrip() {
        let mut obs = InterventionObserver::new();
        obs.record_auto(TaskPhase::Executing);
        obs.record_override(TaskPhase::Reviewing);

        let json = serde_json::to_string(&obs).unwrap();
        let loaded: InterventionObserver = serde_json::from_str(&json).unwrap();

        assert_eq!(loaded.phase_stats.get("Executing").copied(), Some((1, 0)));
        assert_eq!(loaded.phase_stats.get("Reviewing").copied(), Some((0, 1)));
    }

    #[test]
    fn test_display_format() {
        let mut obs = InterventionObserver::new();
        for _ in 0..9 {
            obs.record_auto(TaskPhase::Executing);
        }
        obs.record_override(TaskPhase::Executing);
        let summary = obs.summary();
        let exec = summary
            .iter()
            .find(|p| p.phase == TaskPhase::Executing)
            .unwrap();
        let display = format!("{exec}");
        assert!(display.contains("9 auto"));
        assert!(display.contains("1 override"));
        assert!(display.contains("auto (90%)"));
    }
}
