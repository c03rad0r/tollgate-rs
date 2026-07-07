# Quota-Aware Kanban Dispatch

**Date:** 2026-07-07
**Status:** Design — not yet implemented
**Related skill:** `kanban-quota-aware-dispatch` (Hermes skill)

## Problem

Kanban workers always use `glm-5.2` (expensive model). During peak hours (06:00–10:00 UTC) the z.ai API charges **3× quota** for this model. Both API keys exhaust rapidly. Workers crash with HTTP 429. The fallback to PPQ costs real money.

**Before this design:** All workers → glm-5.2 → proxy → key A/B → retry → PPQ fallback

**After this design:** Workers → X-Model-Tier header → proxy picks cheapest viable model per tier + quota headroom → key A/B → retry only (no PPQ for workers)

## Four Levers

1. **Model tier per task** — cheap models for simple work, expensive only when justified
2. **Quota-aware routing** — Kalman filter predictions drive model downgrade when budget is tight
3. **Peak-hour scheduling** — cheap-model tasks during peak, expensive tasks off-peak
4. **Kanban task metadata** — each task carries a `model_tier` field so the dispatcher knows its requirements

## Model Tiers

| Tier | Model | Relative cost vs glm-5.2 | Peak multiplier | Effective cost during peak | Best for |
|------|-------|-------------------------|-----------------|---------------------------|----------|
| `flash` | glm-4.5-flash | 0.11× | 3× | 0.33× | Formatting, grep/search, simple edits, test runs |
| `air` | glm-4.5-air | 0.22× | 3× | 0.66× | Mid-complexity, boilerplate generation |
| `mid` | glm-4.5 | 0.33× | 3× | 1.0× | Moderate coding, refactoring |
| `heavy` | glm-5.2 | 1.0× | 3× | 3.0× | Complex reasoning, architecture, debugging |

Running a flash-tier task during peak costs **0.33×** vs a heavy-tier task at **3.0×** — a **9× savings**.

## Quota State Machine (from Kalman Filter)

The Kalman filter (`burn_predictor.py`) produces per-key predictions: `hours_left`, `will_exhaust`, `burn_rate_tph`. These map to a `quota_state`:

```
PLENTYFUL ──→ MODERATE ──→ TIGHT ──→ CRITICAL
   ↑            ↑            ↑           ↑
hours>48     hours>12     hours>2     hours<2
                                    or will_exhaust
```

| State | Allowed tiers | Behavior |
|-------|--------------|----------|
| `PLENTYFUL` | All | No restrictions |
| `MODERATE` | `flash`, `air`, `mid` | Heavy reserved for when it matters |
| `TIGHT` | `flash`, `air` | Lightweight work only |
| `CRITICAL` | `flash` | Minimum viable — retry/backoff if even flash fails |

### Peak Hours Override

06:00–10:00 UTC — cap at `air` regardless of quota state. Heavy tasks are never dispatched during peak; they queue for the 10:01 UTC off-peak cron.

## Kanban Task Metadata

Each task carries an optional `model_tier` field:

```json
{
  "id": "t_abc123",
  "title": "Fix typo in README",
  "model_tier": "flash",    // default when absent
  "status": "ready"
}
```

### Recommended tier by task type

| Task type | Recommended tier |
|-----------|-----------------|
| Fix typo / formatting | `flash` |
| Add CI workflow | `air` |
| Implement basic endpoint | `air` |
| Refactor module | `mid` |
| Add auth middleware | `mid` |
| Design protocol extension | `heavy` |
| Debug payment channel race | `heavy` |
| Architecture decision | `heavy` |

## Dispatcher Algorithm

```
function dispatch_tick():
    quota_state = kalman.get_quota_state()
    peak_hours = 06:00 <= UTC.now() < 10:00
    
    for task in kanban.ready_tasks():
        required = task.model_tier or "flash"
        
        // Apply peak-hours cap
        max_allowed = peak_hours ? "air" : "heavy"
        
        // Compute available tiers = quota_state allowed ∩ max_allowed
        tiers = available_tiers(quota_state, max_allowed)
        
        // Pick cheapest tier >= required
        chosen = lowest_tier_meeting(tiers, required)
        
        if chosen:
            spawn_worker(task, model_tier=chosen,
                         header="X-Model-Tier: {chosen}")
        else:
            defer(task, reason=f"no tier available: req={required}, state={quota_state}, peak={peak_hours}")
```

## Implementation Plan

### Phase 1: Task Metadata
- Add `model_tier` field to kanban task schema
- Update `kanban_create`/`kanban_show` tools
- Default: `flash`

### Phase 2: Proxy Tier Rewrite
- Implement `X-Model-Tier` header in z.ai proxy (planned feature, see tiered-model-selection-design.md)
- Header → model mapping:
  - `flash` → `glm-4.5-flash`
  - `air` → `glm-4.5-air`
  - `mid` → `glm-4.5`
  - `heavy` → `glm-5.2`
- Proxy MAY upgrade within tier when quota is plentiful

### Phase 3: Kalman Quota State
- Add `quota_state()` to `burn_predictor.py`
- Expose via proxy `/quota-state` endpoint
- Returns the enum + hours_left for each key

### Phase 4: Smart Dispatcher
- Wire dispatch daemon to query Kalman state before each tick
- Read `task.model_tier` from each ready task
- Apply peak-hours cap
- Pass `X-Model-Tier` to spawned workers
- Defer tasks that can't run at available tier

### Phase 5: Off-Peak Queue
- Heavy tasks deferred during peak get queued
- One-shot cron at 10:01 UTC dispatches them
- Tasks with `model_tier: heavy` auto-scheduled for off-peak windows

## Related Documents

- `docs/design/core/quota-api.md` — z.ai quota API structure
- Hermes skill: `kanban-quota-aware-dispatch` — implementation guide
- Hermes skill: `zai-proxy-management` — proxy architecture, key rotation
- Hermes skill: `kalman-convergence-check` — Kalman filter health
- `kanban-worker-management/references/peak-hours-dispatch.md` — peak hours dispatch pattern
