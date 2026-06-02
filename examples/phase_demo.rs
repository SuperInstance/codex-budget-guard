/// Example: Phase detection visualization.
///
/// Shows how conservation-checker's `PreTransition` / `Transitioning`
/// phases map to budget depletion warnings in real time.
use codex_budget_guard::{BudgetAction, BudgetConfig, BudgetGuard, Phase};

const SPENDING_PATTERN: &[u64] = &[
    2_000,  // turn 1: baseline
    2_500,  // turn 2: slight increase
    3_000,  // turn 3: steady
    4_000,  // turn 4: accelerating
    6_000,  // turn 5: PreTransition territory
    10_000, // turn 6: spending fast → Transitioning
    15_000, // turn 7: 🚨 critical
    5_000,  // turn 8: user slows down (Resolving)
    2_000,  // turn 9: back to baseline
];

fn main() {
    println!("=== Budget Guard Phase Detection Demo ===\n");

    let config = BudgetConfig::builder()
        .daily(100_000)
        .warmup_records(3)
        .build();

    let mut guard = BudgetGuard::new("phase-demo", config);

    for (i, &spend) in SPENDING_PATTERN.iter().enumerate() {
        let action = guard.record(spend, "gpt-5-codex").unwrap();
        let daily_remaining = guard.checker().current_value("daily");
        let daily_phase = guard.checker().phase("daily");
        let drift = guard.checker().drift_rate("daily");

        let phase_icon = match daily_phase {
            Phase::Stable => "✅",
            Phase::PreTransition => "⚠️",
            Phase::Transitioning => "🚨",
            Phase::Resolving => "🔄",
        };

        let action_str = match &action {
            BudgetAction::Proceed(m) => format!("Proceed({m})"),
            BudgetAction::Throttle(m) => format!("Throttle→{m}"),
            BudgetAction::Halt => "HALT".into(),
        };

        println!(
            "Turn {:2}: spent {:>5} tokens | remaining: {:>6.0} | phase: {} {:?} (drift: {:>+.0}/rec) | action: {}",
            i + 1,
            spend,
            daily_remaining,
            phase_icon,
            daily_phase,
            drift,
            action_str,
        );
    }

    println!("\n--- Key takeaways ---");
    println!("• Stable → Budget well within limits, full speed ahead");
    println!("• PreTransition → Spending accelerating, consider downgrade");
    println!("• Transitioning → Critical depletion rate, immediate action needed");
    println!("• Resolving → Was critical but recovering, hold throttle");
}
