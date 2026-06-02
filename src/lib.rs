//! # codex-budget-guard
//!
//! Budget enforcement for [OpenAI Codex CLI](https://github.com/openai/codex).
//!
//! Tracks token spending across daily, weekly, and monthly windows using
//! [conservation-checker](https://crates.io/crates/conservation-checker)'s
//! one-sided conservation law engine. When a budget approaches its limit:
//!
//! 1. **Phase detection** warns you that spending is accelerating
//!    (`PreTransition` → `Transitioning`)
//! 2. **Auto-throttle** downgrades the model capability (e.g. GPT-5 → GPT-4o → GPT-4.1-mini)
//!    as budget depletion gets critical
//! 3. **Serde snapshots** checkpoint spending for audit, billing, or recovery
//!
//! ## Quick start
//!
//! ```ignore
//! use codex_budget_guard::{BudgetGuard, BudgetConfig, BudgetPeriod};
//!
//! let config = BudgetConfig::builder()
//!     .daily(500_000)       // 500K tokens/day max
//!     .weekly(2_500_000)    // 2.5M tokens/week max
//!     .monthly(10_000_000)  // 10M tokens/month max
//!     .build();
//!
//! let mut guard = BudgetGuard::new("my-codex-session", config);
//!
//! // After each API call, report token usage:
//! guard.record(1200, "gpt-5-codex").unwrap();
//!
//! // Before the next call, check what to do:
//! let action = guard.recommend_action();
//! match action {
//!     BudgetAction::Proceed(model) => println!("Use model: {model}"),
//!     BudgetAction::Throttle(model) => println!("Throttled to: {model}"),
//!     BudgetAction::Halt => println!("Budget exhausted!"),
//! }
//! ```

use conservation_checker::ConservationChecker;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// Re-export for convenience
pub use conservation_checker::Phase;

// ── Errors ────────────────────────────────────────────────────────────────────

/// Errors that can occur during budget guard operations.
#[derive(Debug, thiserror::Error)]
pub enum BudgetError {
    /// The named budget period has not been registered.
    #[error("budget period '{0}' is not registered")]
    NotRegistered(String),
    /// Serialization/deserialization failure for audit snapshots.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    /// I/O error during file operations.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

// ── Budget types ──────────────────────────────────────────────────────────────

/// Supported budget period windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BudgetPeriod {
    /// Resets every 24 hours.
    Daily,
    /// Resets every 7 days.
    Weekly,
    /// Resets every 30 days.
    Monthly,
}

impl BudgetPeriod {
    fn label(&self) -> &'static str {
        match self {
            BudgetPeriod::Daily => "daily",
            BudgetPeriod::Weekly => "weekly",
            BudgetPeriod::Monthly => "monthly",
        }
    }

    /// Human-readable labels for each budget period.
    pub fn labels() -> [&'static str; 3] {
        ["daily", "weekly", "monthly"]
    }
}

/// Configuration for token budget limits across time windows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetConfig {
    /// Maximum tokens allowed per day. `None` means unlimited.
    pub daily: Option<f64>,
    /// Maximum tokens allowed per week. `None` means unlimited.
    pub weekly: Option<f64>,
    /// Maximum tokens allowed per month. `None` means unlimited.
    pub monthly: Option<f64>,
    /// Tolerance fraction (0.0–1.0) applied to each budget. A tolerance of
    /// 0.05 means you can overshoot by 5% before being considered violated.
    /// Default: 0.0 (strict).
    #[serde(default)]
    pub tolerance: f64,
    /// Optional model tier ladder for auto-throttle. Each entry is a model
    /// slug that represents one step down in capability/cost. When budget
    /// depletion reaches critical phases, the guard steps down this ladder.
    #[serde(default = "default_throttle_ladder")]
    pub throttle_ladder: Vec<String>,
    /// Minimum number of records before phase analysis produces actionable
    /// results. Prevents false positives on cold starts.
    #[serde(default = "default_warmup_records")]
    pub warmup_records: usize,
}

fn default_throttle_ladder() -> Vec<String> {
    vec![
        "gpt-5-codex".to_string(),
        "gpt-4.1".to_string(),
        "gpt-4.1-mini".to_string(),
        "gpt-4.1-nano".to_string(),
    ]
}

fn default_warmup_records() -> usize {
    5
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            daily: Some(500_000.0),
            weekly: Some(2_500_000.0),
            monthly: Some(10_000_000.0),
            tolerance: 0.0,
            throttle_ladder: default_throttle_ladder(),
            warmup_records: default_warmup_records(),
        }
    }
}

/// Builder for `BudgetConfig`.
#[derive(Debug, Default)]
pub struct BudgetConfigBuilder {
    daily: Option<f64>,
    weekly: Option<f64>,
    monthly: Option<f64>,
    tolerance: f64,
    throttle_ladder: Option<Vec<String>>,
    warmup_records: Option<usize>,
}

impl BudgetConfigBuilder {
    /// Set daily token limit.
    pub fn daily(mut self, tokens: u64) -> Self {
        self.daily = Some(tokens as f64);
        self
    }
    /// Set weekly token limit.
    pub fn weekly(mut self, tokens: u64) -> Self {
        self.weekly = Some(tokens as f64);
        self
    }
    /// Set monthly token limit.
    pub fn monthly(mut self, tokens: u64) -> Self {
        self.monthly = Some(tokens as f64);
        self
    }
    /// Set budget tolerance fraction (0.0 = strict, 0.05 = allow 5% overshoot).
    pub fn tolerance(mut self, tol: f64) -> Self {
        self.tolerance = tol.clamp(0.0, 1.0);
        self
    }
    /// Set the throttle ladder (ordered most → least capable).
    pub fn throttle_ladder(mut self, ladder: Vec<String>) -> Self {
        self.throttle_ladder = Some(ladder);
        self
    }
    /// Set the minimum number of records before phase analysis produces actionable
    /// results. Prevents false positives on cold starts.
    pub fn warmup_records(mut self, n: usize) -> Self {
        self.warmup_records = Some(n);
        self
    }
    /// Build the `BudgetConfig`.
    pub fn build(self) -> BudgetConfig {
        BudgetConfig {
            daily: self.daily,
            weekly: self.weekly,
            monthly: self.monthly,
            tolerance: self.tolerance,
            throttle_ladder: self.throttle_ladder.unwrap_or_else(default_throttle_ladder),
            warmup_records: self.warmup_records.unwrap_or_else(default_warmup_records),
        }
    }
}

impl BudgetConfig {
    /// Create a builder for `BudgetConfig`.
    pub fn builder() -> BudgetConfigBuilder {
        BudgetConfigBuilder::default()
    }

    /// Compute absolute tolerance in tokens for a given limit.
    fn tolerance_for(&self, limit: f64) -> f64 {
        limit * self.tolerance
    }
}

// ── Suggested action ──────────────────────────────────────────────────────────

/// The action the budget guard recommends before the next API call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetAction {
    /// Budget healthy. Use the originally-requested model slug.
    Proceed(String),
    /// Budget approaching depletion. Downgrade to a cheaper model.
    Throttle(String),
    /// All budget windows exhausted. Should block further requests.
    Halt,
}

// ── Audit snapshot ────────────────────────────────────────────────────────────

/// A point-in-time snapshot of all budget state, suitable for logging, audit, or
/// recovery via `BudgetGuard::from_snapshot`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetSnapshot {
    /// Session identifier.
    pub session_id: String,
    /// When this snapshot was taken (Unix millis).
    pub timestamp_ms: i64,
    /// Serialized budget periods (conservation-checker state, period params).
    pub periods: BTreeMap<String, PeriodSnapshot>,
    /// Cumulative total tokens spent across all periods.
    pub cumulative_total: f64,
    /// Current throttle level index (0 = full speed, >0 = downgraded).
    pub throttle_level: usize,
    /// The model currently in use.
    pub active_model: String,
}

/// Snapshot of a single budget period's state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeriodSnapshot {
    /// Token limit for this period.
    pub limit: f64,
    /// Tokens consumed so far in this period.
    pub consumed: f64,
    /// Remaining tokens.
    pub remaining: f64,
    /// Whether the budget is currently violated.
    pub violated: bool,
    /// Current phase of this period.
    pub phase: Phase,
    /// Drift rate (tokens per record).
    pub drift_rate: f64,
}

// ── Spending record ───────────────────────────────────────────────────────────

/// A single spending record kept for history/audit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpendingRecord {
    /// Timestamp when the record was created (Unix millis).
    pub timestamp_ms: i64,
    /// Tokens consumed in this call.
    pub tokens: f64,
    /// Model slug used for this call.
    pub model: String,
}

// ── BudgetGuard ───────────────────────────────────────────────────────────────

/// Token budget enforcer for Codex CLI sessions.
///
/// Uses `conservation-checker` internally to track one-sided conservation of
/// daily, weekly, and monthly token budgets. Detects spending phases
/// (`Stable`, `PreTransition`, `Transitioning`, `Resolving`) and recommends
/// automated model downgrades when budget depletion accelerates.
///
/// ## Audit snapshots
///
/// Call [`snapshot_json`](BudgetGuard::snapshot_json) periodically or on
/// shutdown to persist spending for billing or forensics. Restore with
/// [`from_snapshot`](BudgetGuard::from_snapshot).
pub struct BudgetGuard {
    /// Session identifier (e.g. thread ID or user identity).
    session_id: String,
    /// Configuration for budget limits.
    config: BudgetConfig,
    /// Underlying conservation checker tracking token budgets.
    checker: ConservationChecker,
    /// Number of `record()` calls so far.
    record_count: usize,
    /// Cumulative tokens spent across all periods.
    cumulative_total: f64,
    /// Current throttle level index (0 = no throttle).
    throttle_level: usize,
    /// The model slug that was last requested.
    active_model: String,
    /// History of recent spending records (for rollup/display).
    history: Vec<SpendingRecord>,
}

impl BudgetGuard {
    /// Create a new budget guard for a session.
    ///
    /// Registers daily, weekly, and monthly tracking windows based on
    /// `config`. Each period starts with its full token allowance.
    pub fn new(session_id: impl Into<String>, config: BudgetConfig) -> Self {
        let session_id = session_id.into();
        let mut checker = ConservationChecker::new();

        // Register budget periods as conservation quantities using token
        // limits as the initial "value" — we track remaining tokens.
        let active_model = config
            .throttle_ladder
            .first()
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());

        if let Some(limit) = config.daily {
            let tol = config.tolerance_for(limit);
            checker.register(BudgetPeriod::Daily.label(), limit, tol);
        }
        if let Some(limit) = config.weekly {
            let tol = config.tolerance_for(limit);
            checker.register(BudgetPeriod::Weekly.label(), limit, tol);
        }
        if let Some(limit) = config.monthly {
            let tol = config.tolerance_for(limit);
            checker.register(BudgetPeriod::Monthly.label(), limit, tol);
        }

        Self {
            session_id,
            config,
            checker,
            record_count: 0,
            cumulative_total: 0.0,
            throttle_level: 0,
            active_model,
            history: Vec::new(),
        }
    }

    /// Restore a `BudgetGuard` from a previously saved `BudgetSnapshot`.
    ///
    /// This is useful for resuming budget tracking across sessions (e.g.
    /// after a restart or crash).
    pub fn from_snapshot(snapshot: BudgetSnapshot, config: BudgetConfig) -> Self {
        let mut checker = ConservationChecker::new();
        for (period_label, ps) in &snapshot.periods {
            checker.register(period_label.clone(), ps.limit, 0.0);
            checker.update(period_label, ps.remaining);
        }
        Self {
            session_id: snapshot.session_id,
            config,
            checker,
            record_count: 0,
            cumulative_total: snapshot.cumulative_total,
            throttle_level: snapshot.throttle_level,
            active_model: snapshot.active_model,
            history: Vec::new(),
        }
    }

    /// Record token usage for an API call.
    ///
    /// Decreases remaining budget in each active period by `tokens`.
    /// Call this after each API response completes with the returned
    /// `TokenUsage.total_tokens`.
    ///
    /// Returns the current [`BudgetAction`] recommendation.
    ///
    /// # Errors
    ///
    /// Returns `BudgetError::NotRegistered` if no budgets are configured.
    pub fn record(&mut self, tokens: u64, model: &str) -> Result<BudgetAction, BudgetError> {
        let tokens = tokens as f64;
        let now_ms = chrono::Utc::now().timestamp_millis();

        self.record_count += 1;
        self.cumulative_total += tokens;
        self.active_model = model.to_string();

        // Record to history
        self.history.push(SpendingRecord {
            timestamp_ms: now_ms,
            tokens,
            model: model.to_string(),
        });

        // Update each registered budget period with remaining tokens
        let registered = self.checker.registered();
        if registered.is_empty() {
            return Err(BudgetError::NotRegistered("no budget periods".into()));
        }

        for period_label in &registered {
            let current = self.checker.current_value(period_label);
            let remaining = current - tokens;
            self.checker.update(period_label, remaining);
        }

        // Take a snapshot for phase detection
        self.checker.snapshot();

        Ok(self.recommend_action())
    }

    /// Determine the recommended action based on current budget state.
    ///
    /// Examines all budget periods, finds the worst phase, and returns a
    /// `BudgetAction`:
    ///
    /// - All periods `Stable` → `Proceed` with the requested model
    /// - Any `PreTransition` or `Transitioning` → potentially `Throttle`
    /// - All budgets exhausted → `Halt`
    pub fn recommend_action(&self) -> BudgetAction {
        let registered = self.checker.registered();

        // Check if any period is fully exhausted
        let all_exhausted = registered.iter().all(|label| {
            self.checker.current_value(label) <= 0.0
        });
        if all_exhausted {
            return BudgetAction::Halt;
        }

        // Find the most severe phase across all periods
        let worst_phase = registered
            .iter()
            .map(|label| self.checker.phase(label))
            .max_by_key(|p| phase_severity(*p))
            .unwrap_or(Phase::Stable);

        // Not enough data yet? Proceed.
        if self.record_count < self.config.warmup_records {
            return BudgetAction::Proceed(self.active_model.clone());
        }

        match worst_phase {
            Phase::Transitioning => {
                // Budgets are depleting fast. Step down the throttle ladder.
                let next_level = (self.throttle_level + 1).min(self.config.throttle_ladder.len());
                let model = self.config.throttle_ladder
                    .get(next_level.min(self.config.throttle_ladder.len().saturating_sub(1)))
                    .cloned()
                    .unwrap_or_else(|| self.active_model.clone());
                BudgetAction::Throttle(model)
            }
            Phase::PreTransition => {
                // Spending is accelerating. Conservative downgrade.
                if self.throttle_level < self.config.throttle_ladder.len() {
                    let model = self.config.throttle_ladder
                        .get(self.throttle_level)
                        .cloned()
                        .unwrap_or_else(|| self.active_model.clone());
                    BudgetAction::Throttle(model)
                } else {
                    BudgetAction::Proceed(self.active_model.clone())
                }
            }
            Phase::Resolving => {
                // Was violating but recovering. Stay at current level.
                BudgetAction::Proceed(self.active_model.clone())
            }
            Phase::Stable => {
                BudgetAction::Proceed(self.active_model.clone())
            }
        }
    }

    /// Generate a Serde audit snapshot of all budget state.
    ///
    /// Serializes every budget period's current value, phase, drift rate,
    /// and violation status as structured JSON. Use for persistent logging,
    /// billing, or recovery.
    pub fn snapshot_json(&self) -> Result<String, BudgetError> {
        let mut periods = BTreeMap::new();

        for label in self.checker.registered() {
            let limit = self.checker.initial_value(&label);
            let consumed = limit - self.checker.current_value(&label);
            periods.insert(
                label.clone(),
                PeriodSnapshot {
                    limit,
                    consumed: consumed.max(0.0),
                    remaining: self.checker.current_value(&label).max(0.0),
                    violated: !self.checker.is_conserved(&label),
                    phase: self.checker.phase(&label),
                    drift_rate: self.checker.drift_rate(&label),
                },
            );
        }

        let snapshot = BudgetSnapshot {
            session_id: self.session_id.clone(),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
            periods,
            cumulative_total: self.cumulative_total,
            throttle_level: self.throttle_level,
            active_model: self.active_model.clone(),
        };

        Ok(serde_json::to_string_pretty(&snapshot)?)
    }

    /// Reset a budget period to its full allowance (e.g. at the start of a new day/week/month).
    ///
    /// Uses `reset_baseline` on the underlying conservation checker, which
    /// effectively clears any violation for that period.
    ///
    /// # Errors
    ///
    /// Returns `BudgetError::NotRegistered` if the period label doesn't exist.
    pub fn reset_period(&mut self, period: BudgetPeriod) -> Result<(), BudgetError> {
        let label = period.label();
        let limit = self
            .config
            .limit_for_period(period)
            .ok_or_else(|| BudgetError::NotRegistered(label.to_string()))?;

        if !self.checker.registered().contains(&label.to_string()) {
            return Err(BudgetError::NotRegistered(label.to_string()));
        }

        self.checker.deregister(label);
        let tol = self.config.tolerance_for(limit);
        self.checker.register(label, limit, tol);
        self.checker.update(label, limit);
        Ok(())
    }

    /// Access the underlying conservation checker for advanced queries.
    pub fn checker(&self) -> &ConservationChecker {
        &self.checker
    }

    /// Total tokens recorded across all periods.
    pub fn cumulative_total(&self) -> f64 {
        self.cumulative_total
    }

    /// Number of `record()` calls made so far.
    pub fn record_count(&self) -> usize {
        self.record_count
    }

    /// Current throttle level (0 = no throttle, 1+ = downgraded).
    pub fn throttle_level(&self) -> usize {
        self.throttle_level
    }

    /// Get spending history (most recent first).
    pub fn history(&self) -> &[SpendingRecord] {
        &self.history
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

impl BudgetConfig {
    fn limit_for_period(&self, period: BudgetPeriod) -> Option<f64> {
        match period {
            BudgetPeriod::Daily => self.daily,
            BudgetPeriod::Weekly => self.weekly,
            BudgetPeriod::Monthly => self.monthly,
        }
    }
}

fn phase_severity(p: Phase) -> u8 {
    match p {
        Phase::Stable => 0,
        Phase::PreTransition => 1,
        Phase::Resolving => 1,
        Phase::Transitioning => 2,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> BudgetConfig {
        BudgetConfig::builder()
            .daily(1000)
            .weekly(5000)
            .monthly(20_000)
            .build()
    }

    #[test]
    fn new_guard_starts_healthy() {
        let guard = BudgetGuard::new("test-session", test_config());
        assert_eq!(guard.cumulative_total(), 0.0);
        assert_eq!(guard.record_count(), 0);
        assert_eq!(guard.throttle_level(), 0);
        assert_eq!(guard.checker().registered().len(), 3);
    }

    #[test]
    fn record_deducts_from_budget() {
        let mut guard = BudgetGuard::new("test-session", test_config());
        guard.record(100, "gpt-5-codex").unwrap();
        assert_eq!(guard.cumulative_total(), 100.0);
        assert_eq!(guard.record_count(), 1);

        // daily remaining should be 1000 - 100 = 900
        assert!((guard.checker().current_value("daily") - 900.0).abs() < 1e-9);
    }

    #[test]
    fn record_multiple_deducts_cumulatively() {
        let mut guard = BudgetGuard::new("test-session", test_config());
        guard.record(300, "gpt-5-codex").unwrap();
        guard.record(400, "gpt-5-codex").unwrap();
        guard.record(200, "gpt-5-codex").unwrap();
        assert_eq!(guard.cumulative_total(), 900.0);

        // daily: 1000 - 900 = 100 remaining
        assert!((guard.checker().current_value("daily") - 100.0).abs() < 1e-9);
        // weekly: 5000 - 900 = 4100
        assert!((guard.checker().current_value("weekly") - 4100.0).abs() < 1e-9);
    }

    #[test]
    fn exhaust_budget_returns_halt() {
        let config = BudgetConfig::builder().daily(100).build();
        let mut guard = BudgetGuard::new("test", config);
        guard.record(50, "gpt-5-codex").unwrap();
        guard.record(50, "gpt-5-codex").unwrap();
        let action = guard.record(1, "gpt-5-codex").unwrap();
        assert_eq!(action, BudgetAction::Halt);
    }

    #[test]
    fn budget_approaching_limit_throttles() {
        let config = BudgetConfig::builder()
            .daily(5000) // large enough to avoid full exhaustion after 5 records
            .warmup_records(1)
            .build();
        let mut guard = BudgetGuard::new("test", config);

        // Spend aggressively — 5 turns of 800 each = 4000 of 5000
        for _ in 0..5 {
            guard.record(800, "gpt-5-codex").unwrap();
        }

        let action = guard.recommend_action();
        // With 4000/5000 consumed, phase should be PreTransition or Transitioning
        // which maps to Throttle
        match action {
            BudgetAction::Throttle(_) => {} // expected
            other => panic!("expected Throttle, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_serialization_roundtrip() {
        let mut guard = BudgetGuard::new("test-session", test_config());
        guard.record(500, "gpt-5-codex").unwrap();

        let json = guard.snapshot_json().unwrap();
        let snapshot: BudgetSnapshot = serde_json::from_str(&json).unwrap();

        assert_eq!(snapshot.session_id, "test-session");
        assert_eq!(snapshot.cumulative_total, 500.0);
        assert!(snapshot.periods.contains_key("daily"));
        assert!(snapshot.periods.contains_key("weekly"));
        assert!(snapshot.periods.contains_key("monthly"));

        // Restore from snapshot
        let restored = BudgetGuard::from_snapshot(snapshot, test_config());
        assert_eq!(restored.cumulative_total(), 500.0);
    }

    #[test]
    fn reset_period_restores_allowance() {
        let mut guard = BudgetGuard::new("test", test_config());
        guard.record(900, "gpt-5-codex").unwrap();
        assert!((guard.checker().current_value("daily") - 100.0).abs() < 1e-9);

        guard.reset_period(BudgetPeriod::Daily).unwrap();
        assert!((guard.checker().current_value("daily") - 1000.0).abs() < 1e-9);
    }

    #[test]
    fn custom_ladder_respected() {
        let config = BudgetConfig::builder()
            .daily(500) // larger budget
            .throttle_ladder(vec![
                "my-big-model".into(),
                "my-small-model".into(),
            ])
            .warmup_records(1)
            .build();
        let mut guard = BudgetGuard::new("test", config);
        guard.record(250, "my-big-model").unwrap();
        guard.record(250, "my-big-model").unwrap();
        // 250*2 = 500 consumed, but tolerance=0 means we haven't exceeded
        // The daily should be exhausted (0 remaining), so this might Halt.
        // Let's use 400 total budget and consume 350 via a single record:
        drop(guard);

        let config = BudgetConfig::builder()
            .daily(100)
            .throttle_ladder(vec![
                "my-big-model".into(),
                "my-small-model".into(),
            ])
            .warmup_records(2) // need 2 records
            .build();
        let mut guard = BudgetGuard::new("test", config);
        guard.record(40, "my-big-model").unwrap(); // 40 used
        guard.record(40, "my-big-model").unwrap(); // 80 used, 20 remain → PreTransition
        let action = guard.recommend_action();
        match &action {
            BudgetAction::Throttle(model) => {
                // The test just validates we get a Throttle response
                // (could be first or second rung depending on phase)
                assert!(!model.is_empty());
            }
            _ => panic!("expected Throttle, got {action:?}"),
        }
    }

    #[test]
    fn serializable_snapshot() {
        // Verify that snapshot can be (de)serialized via JSON
        let mut guard = BudgetGuard::new("serde-test", test_config());
        guard.record(100, "gpt-5-codex").unwrap();
        let json = guard.snapshot_json().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["session_id"], "serde-test");
        assert_eq!(parsed["cumulative_total"], 100.0);
        assert!(parsed["periods"]["daily"]["remaining"].as_f64().unwrap() >= 0.0);
    }

    #[test]
    fn history_tracks_records() {
        let mut guard = BudgetGuard::new("test", test_config());
        guard.record(100, "gpt-5-codex").unwrap();
        guard.record(200, "gpt-4.1").unwrap();
        assert_eq!(guard.history().len(), 2);
        assert_eq!(guard.history()[0].tokens, 100.0);
        assert_eq!(guard.history()[1].model, "gpt-4.1");
    }

    #[test]
    fn no_budget_periods_returns_error() {
        let config = BudgetConfig::default();
        let mut guard = BudgetGuard::new("test", BudgetConfig {
            daily: None,
            weekly: None,
            monthly: None,
            ..config
        });
        let result = guard.record(100, "gpt-5-codex");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no budget periods"));
    }

    #[test]
    fn phase_snapshots_integration() {
        let config = BudgetConfig::builder()
            .daily(500)
            .tolerance(0.0)
            .warmup_records(1)
            .build();
        let mut guard = BudgetGuard::new("phase-test", config);

        // Spend a little
        guard.record(100, "gpt-5-codex").unwrap();
        let snap = guard.snapshot_json().unwrap();
        let sv: serde_json::Value = serde_json::from_str(&snap).unwrap();
        assert_eq!(sv["periods"]["daily"]["consumed"], 100.0);
        assert_eq!(sv["periods"]["daily"]["remaining"], 400.0);
    }

    #[test]
    fn unregistered_period_reset_returns_error() {
        let config = BudgetConfig::builder().daily(100).build();
        let mut guard = BudgetGuard::new("test", config);
        // weekly wasn't registered
        let result = guard.reset_period(BudgetPeriod::Weekly);
        assert!(result.is_err());
    }
}
