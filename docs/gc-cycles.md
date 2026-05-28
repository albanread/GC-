# GC Cycles

## Collection entry points

| Method | What it collects | When to call |
|--------|-----------------|--------------|
| `collect_minor` | G0, and conditionally cascades into G1→Tenured | Routine; called by `collect_auto` |
| `collect_major` | G1→Tenured then G0→G0 | Manual; "promote everything now" |
| `collect_full` | G0→G1, G1→Tenured, Tenured→Tenured (three passes) | When Tenured is full |
| `collect_auto` | Calls minor or major based on Tenured occupancy | Typical mutator-side call |
| `try_collect_*` | As above, returning `Result<_, GcError>` instead of panicking | OOM-safe contexts |

All collection methods take a `visit_roots: FnMut(&mut PageEvacuator)` closure. The caller is responsible for passing every live root slot to `evac.visit(&mut slot)`.

## Minor GC

```
collect_minor:
  minors_since_g0_promote += 1
  if counter >= G0_PROMOTION_THRESHOLD (3):
    dest = G1; reset counter
  else:
    dest = G0

  evacuate_with_roots(G0, dest, roots + dirty cards)

  if promoted to G1:
    g0_promotes_since_g1_promote += 1
    if g0_promotes_since_g1_promote >= G1_PROMOTION_THRESHOLD (5):
      evacuate_with_roots(G1, Tenured, roots + dirty cards)  // cascade
      reset g0_promotes_since_g1_promote

  rebuild_cards_for_old_gens()
```

The default thresholds mean:
- A G0 object survives at most **3** minor cycles before being promoted to G1.
- A G1 object survives at most **5 G0 promotions** (≈15 minor cycles) before being promoted to Tenured.

A minor cycle that promotes G0 can **cascade** into a G1→Tenured pass in the same stop-the-world pause if the G1 threshold is also reached. This is reported in `CollectResult::cascade` and `CollectResult::promoted_g1`.

### Cohort promotion

Promotion is **cohort-based**, not per-object. When the counter fires, all G0 objects alive at that moment promote together — it does not matter how old each individual object is. SBCL's per-page age tracking (where each source page independently decides its destination) is a planned refinement; the `PageDesc::age` field is reserved for it but currently unused.

## Major GC

```
collect_major:
  snapshot descs for card scan
  evacuate_with_roots(G1, Tenured, roots + dirty cards)  // pass 1
  re-snapshot descs after pass 1
  evacuate_with_roots(G0, G0,      roots + dirty cards)  // pass 2
  rebuild_cards_for_old_gens()
  reset both counters to 0
```

Order matters. The G1→Tenured pass runs first so the subsequent G0→G0 pass sees G1 references that have already been resolved to Tenured addresses. This avoids chasing a stale cross-generation reference during the G0 pass.

Major GC resets both counters (`minors_since_g0_promote = 0`, `g0_promotes_since_g1_promote = 0`), so the next series of minor cycles starts a fresh cohort accounting from zero.

## Full GC

```
collect_full:
  pass 1: evacuate_with_roots(G0, G1,      roots + dirty cards)
  pass 2: evacuate_with_roots(G1, Tenured, roots + dirty cards)
  pass 3: evacuate_with_roots(Tenured, Tenured, roots only — no card scan)
  rebuild_cards_for_old_gens()
  reset both counters to 0
```

After passes 1 and 2, G0 and G1 are empty. The Tenured→Tenured pass then uses only the caller's explicit roots — there are no younger-generation references to Tenured at this point, so no card scan is needed. Unreachable Tenured objects are reclaimed for the first time.

**Important:** `collect_full` does **not** preserve conservative-pin state across passes. Each evacuation pass ends with `clear_all_pins`. Callers relying on conservative pinning for Tenured objects must supply every live Tenured object through the explicit-root closure.

## Trigger policy

The trigger policy determines when `collect_auto` calls a minor vs. major cycle.

```rust
pub fn should_collect(&self) -> bool {
    self.bytes_alloc_since_gc >= self.auto_gc_trigger_bytes
}
```

After each cycle, the trigger threshold is recomputed:

```
auto_gc_trigger_bytes = max(gc_budget_min_bytes, 0.5 × tenured_used_bytes)
```

This grows the trigger proportionally with the size of the Tenured generation. A heap with a large Tenured set can absorb proportionally more allocation before triggering. A fresh heap with an empty Tenured set uses the minimum budget.

Default values:
- `gc_budget_min_bytes` = 8 MB
- `tenured_full_threshold_bps` = 7500 (75%)

`collect_auto` calls `collect_major` when Tenured occupancy exceeds `tenured_full_threshold_bps`, otherwise `collect_minor`.

## Return values

| Type | Fields |
|------|--------|
| `CollectResult` | `evac: EvacResult`, `cascade: Option<EvacResult>`, `promoted_g0: bool`, `promoted_g1: bool`, `minors_since_g0_promote_after: u32` |
| `FullCollectResult` | `g0_evac`, `g1_evac`, `tenured_evac: EvacResult`, `tenured_freed_bytes: usize` |
| `EvacResult` | `objects_copied`, `cells_copied`, `pages_freed`, `pages_flipped` |

`pages_flipped` counts pages whose generation was updated in place because they contained pinned objects — the objects were not copied but the page moved up a generation.

---

Back to [Home](index.md).
