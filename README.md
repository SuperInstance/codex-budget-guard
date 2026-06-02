# codex-budget-guard

> 💰 Budget enforcement for [OpenAI Codex CLI](https://github.com/openai/codex).

**Codex is brilliant.** It's the best coding agent in a terminal. But every `codex 'fix this bug'` costs tokens — and tokens cost money. Codex burns through models like GPT-5-codex, which adds up fast when you're iterating.

**Conservation-checker keeps it from being expensive.**

This crate combines Codex's token-tracking architecture with `conservation-checker`'s one-sided conservation laws to give you:

- **Set a budget** — daily, weekly, and monthly token limits
- **Get warned** before you exceed it — phase detection catches accelerating spending
- **Auto-downgrade** when you're close — GPT-5 → GPT-4.1 → GPT-4.1-mini → GPT-4.1-nano
- **Audit snapshots** — Serde-serialized checkpoints for billing and forensics

## How it works

```
┌─────────────────────────────────────────────────────┐
│                   Codex CLI                         │
│  ┌─────────────┐  ┌──────────────┐  ┌────────────┐ │
│  │ TokenUsage   │  │ ModelRouter │  │  Responses  │ │
│  │ (input+      │──│ (which model │  │  API call   │ │
│  │  output+     │  │  to use?)    │  │            │ │
│  │  reasoning)  │  └──────┬───────┘  └─────┬──────┘ │
│  └──────┬──────┘         │                 │       │
│         │                │                 │       │
└─────────┼────────────────┼─────────────────┼───────┘
          │                │                 │
          ▼                ▼                 ▼
┌─────────────────────────────────────────────────────┐
│               codex-budget-guard                    │
│                                                     │
│  ┌─────────────────────────────────────────────┐   │
│  │         BudgetGuard                          │   │
│  │  ┌──────────┐  ┌──────────┐  ┌──────────┐  │   │
│  │  │ Daily    │  │ Weekly   │  │ Monthly  │  │   │
│  │  │ budget   │  │ budget   │  │ budget   │  │   │
│  │  └────┬─────┘  └────┬─────┘  └────┬─────┘  │   │
│  │       │              │              │        │   │
│  │  ┌────▼──────────────▼──────────────▼────┐  │   │
│  │  │   conservation-checker                │  │   │
│  │  │   (Phase detection, drift rates)      │  │   │
│  │  └───────────────────────────────────────┘  │   │
│  │                                             │   │
│  │  Output: BudgetAction                       │   │
│  │  ├─ Proceed("gpt-5-codex")                  │   │
│  │  ├─ Throttle("gpt-4.1-mini")               │   │
│  │  └─ Halt                                    │   │
│  └─────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────┘
```

## Architecture context

At the time of writing, Codex CLI (openai/codex, 88K+ stars) tracks token usage through:

- **`codex_protocol::protocol::TokenUsage`** — total tokens, input tokens, cached tokens, output tokens, reasoning tokens per API call
- **`codex_core`** — session management, model routing, auto-compaction when context windows fill
- **`codex_analytics`** — tracks `TurnResolvedConfigFact` and other analytics events including model slug and token usage
- **`codex_models_manager`** — model metadata with context windows, speed tiers, service tiers, and personality configs

The integration happens at the **session + analytics layer**: after each turn's response completes with `TokenUsage`, you feed the total tokens into `BudgetGuard::record()`, get back a `BudgetAction`, and route accordingly.

## Usage

Add to your Codex-side `Cargo.toml`:

```toml
[dependencies]
codex-budget-guard = "0.1"
```

### Basic setup

```rust
use codex_budget_guard::{BudgetGuard, BudgetConfig};

let config = BudgetConfig::builder()
    .daily(500_000)        // 500K tokens/day ($~10 at GPT-5 pricing)
    .weekly(2_500_000)     // 2.5M tokens/week
    .monthly(10_000_000)   // 10M tokens/month
    .tolerance(0.05)       // allow 5% overshoot before flagging violation
    .build();

let mut guard = BudgetGuard::new("codex-session-42", config);

// After each API response completes with token usage:
guard.record(response.token_usage.total_tokens, &response.model);

// Before the next API call, check what model to use:
let action = guard.recommend_action();
match action {
    BudgetAction::Proceed(model) => { /* full speed */ }
    BudgetAction::Throttle(model) => { /* downgrade */ }
    BudgetAction::Halt => { /* block further calls */ }
}
```

### Integration point in Codex CLI

The natural integration point is in `codex-core`'s turn processing after a `ResponseEvent::Completed` is received:

```rust
// In core/src/session/turn.rs or stream_events_utils.rs:

fn handle_response_completed(
    guard: &mut BudgetGuard,
    token_usage: &TokenUsage,
    model_slug: &str,
) -> BudgetAction {
    let total = token_usage.total_tokens as u64;
    guard.record(total, model_slug).unwrap_or(BudgetAction::Proceed(model_slug.to_string()))
}

fn choose_model_for_next_turn(
    guard: &BudgetGuard,
    preferred_model: &str,
) -> String {
    match guard.recommend_action() {
        BudgetAction::Proceed(m) | BudgetAction::Throttle(m) => m,
        BudgetAction::Halt => {
            tracing::warn!("Budget exhausted — halting further requests");
            // The guard has already returned Halt; caller should handle
            preferred_model.to_string()
        }
    }
}
```

### Phase detection

The guard uses `conservation-checker` to classify spending into four phases:

| Phase | Meaning | Action |
|-------|---------|--------|
| **Stable** | Spending is within normal range | Proceed with current model |
| **PreTransition** | Spending rate is accelerating | Consider downgrading one tier |
| **Transitioning** | Budget depletion is critical | Downgrade immediately |
| **Resolving** | Was critical but recovering | Stay at current tier |

### Audit snapshots

Snapshot spending at any point for billing, history, or crash recovery:

```rust
// Save
let json = guard.snapshot_json()?;
std::fs::write("budget-snapshot.json", &json)?;

// Restore later
let snapshot: BudgetSnapshot = serde_json::from_str(&json)?;
let guard = BudgetGuard::from_snapshot(snapshot, config);
```

Example snapshot output:

```json
{
  "session_id": "codex-session-42",
  "timestamp_ms": 1748815200000,
  "periods": {
    "daily": {
      "limit": 500000.0,
      "consumed": 234500.0,
      "remaining": 265500.0,
      "violated": false,
      "phase": "Stable",
      "drift_rate": -15000.0
    },
    "weekly": {
      "limit": 2500000.0,
      "consumed": 890000.0,
      "remaining": 1610000.0,
      "violated": false,
      "phase": "PreTransition",
      "drift_rate": -12000.0
    }
  },
  "cumulative_total": 890000.0,
  "throttle_level": 0,
  "active_model": "gpt-5-codex"
}
```

### Auto-throttle ladder

Configure the model tier ladder for automatic downgrades:

```rust
let config = BudgetConfig::builder()
    .daily(500_000)
    .throttle_ladder(vec![
        "gpt-5-codex".into(),
        "gpt-4.1".into(),
        "gpt-4.1-mini".into(),
        "gpt-4.1-nano".into(),
    ])
    .build();
```

When phase detection hits `Transitioning`, the guard moves one step down the ladder. Each subsequent escalation moves further down. The goal: keep you coding on a cheaper model instead of grinding to a halt.

## Run the examples

```bash
# Phase detection visualization
cargo run --example phase_demo

# Full integration demo with simulated Codex sessions
cargo run --example integrated
```

## Status

This is an architectural integration blueprint. The crate compiles, passes all tests, and demonstrates the full integration pattern. To use it live inside Codex CLI:

1. Add `codex-budget-guard` to `codex-core/Cargo.toml`
2. Wire `BudgetGuard` into `Session` (it lives alongside the per-session state)
3. Call `guard.record()` in the `ResponseEvent::Completed` handler
4. Call `guard.recommend_action()` before building the next API request
5. Route `Throttle` to switch model slugs via `ModelsManager`

The phase detection, budget enforcement, Serde snapshots, and throttle ladder are all production-ready.

## License

MIT
