/// Example: Full integration of BudgetGuard with Codex CLI workflow simulation.
use codex_budget_guard::{BudgetAction, BudgetConfig, BudgetGuard};

/// Simulate OpenAI token costs per model (input + output per call).
fn cost_per_call(model: &str) -> u64 {
    match model {
        "gpt-5-codex" => 12_000,
        "gpt-4.1" => 8_000,
        "gpt-4.1-mini" => 4_000,
        "gpt-4.1-nano" => 1_500,
        _ => 10_000,
    }
}

/// Simulate Codex calling the LLM and getting back a token_usage response.
fn run_codex_turn(guard: &mut BudgetGuard, model: &str) {
    let tokens = cost_per_call(model);
    match guard.record(tokens, model).unwrap() {
        BudgetAction::Proceed(m) => {
            println!("  ✅ Proceed with {m} ({tokens} tokens used, total={})", guard.cumulative_total());
        }
        BudgetAction::Throttle(new_model) => {
            println!("  ⚠️  Throttle: {model} → {new_model} ({tokens} tokens would have been used)");
        }
        BudgetAction::Halt => {
            println!("  🛑 HALT — no budget remaining");
        }
    }
}

fn main() {
    println!("=== Codex Budget Guard: Full Integration Demo ===\n");

    // Realistic: $100/day budget → ~500K tokens at GPT-5 pricing
    let config = BudgetConfig::builder()
        .daily(500_000)        // 500K tokens/day
        .weekly(2_500_000)     // 2.5M tokens/week
        .monthly(10_000_000)   // 10M tokens/month
        .tolerance(0.05)       // allow 5% overshoot before flagging violation
        .throttle_ladder(vec![
            "gpt-5-codex".into(),
            "gpt-4.1".into(),
            "gpt-4.1-mini".into(),
            "gpt-4.1-nano".into(),
        ])
        .warmup_records(3)     // need 3 records before phase analysis kicks in
        .build();

    let mut guard = BudgetGuard::new("codex-session-42", config);

    // Simulate 10 coding sessions across a day
    for i in 1..=10 {
        let model = if guard.throttle_level() == 0 {
            "gpt-5-codex"
        } else if guard.throttle_level() <= 1 {
            "gpt-4.1"
        } else if guard.throttle_level() <= 2 {
            "gpt-4.1-mini"
        } else {
            "gpt-4.1-nano"
        };

        println!("\nTurn {i} (throttle_level={}, model={model}):", guard.throttle_level());
        run_codex_turn(&mut guard, model);

        // After turn 6, check phase analysis
        if i == 6 {
            let snap = guard.snapshot_json().unwrap();
            println!("\n  📸 Audit snapshot so far:\n{}", snap);
        }
    }

    // Final audit report
    println!("\n=== Final Audit ===");
    let snapshot = guard.snapshot_json().unwrap();
    // Just show key metrics
    let sv: serde_json::Value = serde_json::from_str(&snapshot).unwrap();
    println!("  Cumulative tokens: {}", sv["cumulative_total"]);
    println!("  Records: {}", guard.record_count());
    if let Some(daily) = sv["periods"]["daily"].as_object() {
        println!("  Daily: {:.0} consumed / {:.0} limit (phase={})",
            daily["consumed"].as_f64().unwrap_or(0.0),
            daily["limit"].as_f64().unwrap_or(0.0),
            daily["phase"]);
    }
    if let Some(weekly) = sv["periods"]["weekly"].as_object() {
        println!("  Weekly: {:.0} consumed / {:.0} limit (phase={})",
            weekly["consumed"].as_f64().unwrap_or(0.0),
            weekly["limit"].as_f64().unwrap_or(0.0),
            weekly["phase"]);
    }
}
