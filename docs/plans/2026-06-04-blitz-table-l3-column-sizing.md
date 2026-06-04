# blitz table: CSS Tables L3 column sizing (option B) Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make `blitz-dom` compute table column widths itself (CSS Tables Level 3 algorithm) and feed Taffy fixed pixel-length tracks, so Taffy is used only for cell placement / row heights — instead of (mis)using generic CSS Grid track sizing for table columns.

**Architecture:** Today the table root is laid out by `compute_grid_layout(&mut TableTreeWrapper, …)` (`packages/blitz-dom/src/layout/mod.rs:256`), which delegates BOTH column sizing and placement to Taffy's grid. We keep the grid for placement/row-heights but, in a pre-pass, (1) measure every cell's min/max-content width via `compute_child_layout`, (2) aggregate per-column min-content / max-content / intrinsic-percentage (distributing colspan>1 cells across spanned columns), (3) compute the used table width, (4) distribute it to columns per L3, and (5) overwrite `grid_template_columns` with resolved `length(px)` tracks before calling `compute_grid_layout`. This sidesteps the structural limitation that a single Taffy min/max track function cannot express (min-content, max-content, percentage) + table-specific distribution simultaneously.

**Tech Stack:** Rust, Taffy 0.9.2 (`compute_grid_layout`, `LayoutPartialTree`, `style_helpers`), stylo computed styles, blitz-dom layout module.

**Verification oracle (read this — there are NO layout unit tests in this repo):**
- Authoritative: the WPT runner doing real reftest pixel comparison:
  `WPT_DIR=/home/ubuntu/blitz/wpt/tests cargo run --release -p wpt css/css-tables`
  and `… css/CSS2/tables`. Compare PASS/FAIL per test against the saved baselines.
- Baselines (status snapshots, name→status) live at:
  `$CLAUDE_JOB_DIR/tmp/baseline_status.txt` (css-tables, **62 PASS** pre-hack)
  `$CLAUDE_JOB_DIR/tmp/css2_baseline_status.txt` (CSS2/tables, **61 PASS**)
  Current main + the shipped 1-line hack = **63 PASS** css-tables (only `table-colspan-percent-auto` flipped FAIL→PASS).
- Fast inner loop: a probe example (see Task 1) printing each cell's `final_layout` for the target test, its ref, and the EDGE cases, asserting expected column widths. Debug build, ~30s vs the release WPT run.

**Acceptance for the whole plan:** css-tables PASS count strictly > 63 with zero regressions vs `baseline_status.txt` (the FAIL→PASS set must include `table-colspan-percent-auto`, `colspan-004`, and ideally `table_grid_size_col_colspan`); CSS2/tables PASS count ≥ 61 with zero regressions; the EDGE1 content-overflow trade-off (auto table, narrow percent column + wider content) no longer overflows.

**This plan supersedes the shipped 1-line hack** (`table.rs` PERCENT branch → `style_helpers::percent`). Task 9 removes it.

---

## VALIDATED design spike (done — the load-bearing assumption is confirmed)

Measuring a cell with `compute_child_layout(run_mode=ComputeSize, axis=Horizontal)` and
**`parent_size.width = None` (INDEFINITE)** makes the cell's own `width:%`/length resolve to
auto during intrinsic sizing, so the TRUE content min/max-content emerges:

| case | content min/max measured |
|---|---|
| EDGE1: `td width:30%` containing `<div width:150px>` | **152 / 152** (= 150 + 2px padding) — the wide child DOES surface |
| EDGE1b: `td 30%` with 24-char nowrap text | 397 / 397 |
| TARGET col0: `td 90%` containing empty `<div width:100%>` | ~2 / ~2 (no intrinsic content; 90% is a *specified* width, not content) |
| TARGET colspan=2: nowrap "Lorem ipsum…" | 426 / 426 |

If you pass a *definite* `parent_size.width`, the cell's `%` resolves and MASKS the content
(EDGE1 → 62, wrong). This is exactly why Taffy's own grid track sizing produced the EDGE1
clip — it measures with the cell's percent applied. **Our pre-pass must use indefinite
parent_size for the content measurement, then combine with the specified width separately:**

```
column_width(col) = max( content_contribution(col),  resolve(specified_width(col), table_used_width) )
```
- EDGE1: max(152, 30%×200=60) = 152 → no clip (fixes the trade-off).
- TARGET col0: max(~2, 90%×400=360) = 360 → correct.

Also note: the table's used width is NOT `inputs.known_dimensions.width` blindly — during a
parent measure pass that is the parent's offer (e.g. body 1000px), not the table's own
`width:400px`. Resolve the table's specified width against its containing block first.

`LayoutPartialTreeExt::measure_child_size` is **private** in Taffy 0.9.2 — build `LayoutInput`
by hand and call the public `compute_child_layout` (the wrapper already implements it).

## Background facts established during investigation (don't re-derive)

- `is_fixed` (table-layout: fixed vs auto) is currently read in exactly ONE place: the PERCENT branch of `collect_table_cells`. Taffy's grid has NO table-specific code; `item_is_table` only affects the table's outer box in `block.rs`, not column sizing.
- The percentage reaches cells today via the cell's OWN stylo `width:%` during `compute_child_layout`, NOT via the grid track (tracks are often `[auto,auto]`). The L3 rewrite makes the column width authoritative (fixed length track), so the cell's own percent becomes redundant for column sizing.
- UA stylesheet: `td/th { padding: 1px }`. The "+4px overflow" (§2 of the findings) is because percent is resolved content-box then padding is added; L3 must resolve column widths as border-box and place padding INSIDE → fixes it.
- Cells currently store only `{ node_id, style }`. `colspan`/`rowspan`/column index are encoded only inside `style.grid_column/grid_row`. The algorithm needs them explicitly (Task 2).
- Findings report: `$CLAUDE_JOB_DIR/tmp/blitz-table-colspan-FINDINGS.md`.

---

## Execution order (per design review)

1. **Task 1** probe harness, **Task 2** data model.
2. **Task 3** measurement helper (parent_size=None, validated above).
3. **Skeleton-first:** a MINIMAL Task 5 — compute columns with a dumb algorithm (equal split, or
   replicate today), inject as fixed `length()` tracks via a per-call style, run full css-tables.
   Prove zero regression + that RowDense/rowspan placement still works with fixed tracks BEFORE
   building the real algorithm. This validates the risky mechanic (per-call style winning over the
   construction-time `grid_template_columns`; measure-pass vs layout-pass; cells filling fixed tracks).
4. **Task 4** per-column aggregation + colspan distribution, then **full Task 5** distribution.
5. **Task 6** gaps/insets. **Land auto-mode (Tasks 1–6) as its own checkpoint/PR.**
6. **Task 7** fixed-mode + `<col>` as a SEPARATE change (its own blast radius on passing fixed tests).
7. Gate every step on the WPT css-tables/CSS2-tables status diff, NOT on EDGE1's exact number
   (EDGE1's only hard requirement: it must not OVERFLOW; its precise width is uncovered by WPT).

## Task 1: Probe harness for the fast inner loop

**Files:**
- Create: `examples/table_l3_probe.rs`

**Step 1:** Write a probe that lays out, via `HtmlDocument::from_html` + `resolve(0.0)`, and walks nodes printing `<table>/<td>/<th>` `final_layout` (x, width, right-edge). Include cases: TARGET (`table-colspan-percent-auto` test + ref bodies), EDGE1 (auto table 200px, `td width:30%` containing a 150px box — expect col1 grows ≥150, no overflow past 200), EDGE2 (`70%+70%` should scale to fit, not overflow), 60%+30% (sum<100%), and a plain `table-layout:fixed` percent table (must stay unchanged). Assert expected widths with `assert!`/`eprintln` and exit non-zero on mismatch.

**Step 2:** Run `cargo run --example table_l3_probe`; record current (hack) numbers as the "before".

**Step 3:** Commit.
```bash
git add examples/table_l3_probe.rs
git commit -m "test: add table L3 column-sizing probe harness"
```

---

## Task 2: Carry per-cell grid + width facts on `TableCell`

**Files:**
- Modify: `packages/blitz-dom/src/layout/table.rs` (struct `TableCell` ~L42; `collect_table_cells` TableCell arm ~L248-316)

**Step 1:** Extend `TableCell`:
```rust
pub struct TableCell {
    node_id: usize,
    style: taffy::Style<Atom>,
    col_start: u16,          // 0-based grid column of the cell's left edge
    colspan: u16,
    rowspan: u16,
    // Specified inline size as authored (before it is reset to auto for Taffy):
    // PERCENT_TAG / LENGTH_TAG / AUTO_TAG carried verbatim.
    specified_width: taffy::Dimension,
}
```
Populate in the TableCell arm: `col_start = *col` (captured before `*col += colspan`), `specified_width = style.size.width` (captured before it is reset to `auto()`), plus the existing `colspan`/`rowspan`.

**Step 2:** Build (`cargo build -p blitz-dom`), expected: compiles.

**Step 3:** Verify no behaviour change: run css-tables WPT, expect still **63 PASS**, zero status diff vs the post-hack run.

**Step 4:** Commit `"refactor(table): carry colspan/col-index/specified-width on TableCell"`.

---

## Task 3: Cell intrinsic measurement helper

**Files:**
- Modify: `packages/blitz-dom/src/layout/table.rs`

**Step 1:** Add a helper on `TableTreeWrapper` that measures a single cell's content-based inline size:
```rust
fn measure_cell_inline(&mut self, cell_index: usize, space: taffy::AvailableSpace) -> f32
```
It builds a `LayoutInput { run_mode: ComputeSize, axis: RequestedAxis::Horizontal,
available_space: Size { width: space, height: MaxContent }, known_dimensions: NONE,
parent_size: <table inner size>, sizing_mode: InherentSize, … }` and calls
`self.compute_child_layout(cell_index.into(), inputs).size.width`.
Use `AvailableSpace::MinContent` for min-content, `MaxContent` for max-content.
Mirror what `track_sizing.rs` `min_content_contribution`/`max_content_contribution` pass.

**Step 2:** Temporarily log measured min/max for the TARGET test cells; run the probe; sanity-check (e.g. the 90% marker cell max-content small; the nowrap colspan cell max-content large).

**Step 3:** Commit `"feat(table): add per-cell min/max-content measurement"`.

---

## Task 4: Per-column intrinsic aggregation (min/max/percent) with colspan distribution

**Files:**
- Modify: `packages/blitz-dom/src/layout/table.rs`

**Step 1:** Add a function building `Vec<ColumnInfo>` where
```rust
struct ColumnInfo { min: f32, max: f32, percent: Option<f32>, has_explicit: bool }
```
For each cell (sorted/iterated), border-box min = `measure_cell_inline(MinContent) + h_padding + h_border`, max likewise.
- colspan==1: `col.min = max(col.min, cell_min)`, `col.max = max(col.max, cell_max)`,
  `specified_width` PERCENT → `col.percent = max(col.percent, pct)`, LENGTH → fold into min/max as a preferred width.
- colspan>1: distribute. Min/max/percent that EXCEEDS the sum of the currently-spanned columns is distributed across them (CSS Tables 3 "distributing excess width to columns"); a spanning cell never sets a single column below the others. Implement the standard "distribute extra, preferring columns that already have intrinsic/percentage" rule; a simpler first cut: distribute excess equally, refine if WPT needs it.
Clamp `Σ percent ≤ 100%` (scale down proportionally if exceeded).

**Step 2:** Log the `ColumnInfo` vec for TARGET and `colspan-004`; eyeball against expectations.

**Step 3:** Commit `"feat(table): aggregate per-column min/max/percent with colspan distribution"`.

---

## Task 5: Table used-width + column distribution (auto mode) → px tracks

**Files:**
- Modify: `packages/blitz-dom/src/layout/table.rs`, `packages/blitz-dom/src/layout/mod.rs:256`

**Step 1:** Add `compute_table_columns(&mut self, inputs) -> Vec<f32>` (auto mode):
1. Inner available width = `inputs` definite width (or known) minus the table's own border/padding and the inter-column gaps/border-collapse insets — so percentages and distribution are border-box-correct (fixes §2).
2. If available width is definite: assigned = distribute(available):
   - give each percentage column `max(col.min, pct * available)`;
   - give remaining columns at least `col.min`;
   - distribute leftover (available − Σ assigned) to non-percentage columns by `col.max` weight (then to percentage columns if still leftover), never below `col.min`;
   - if Σmin > available, table overflows: use `col.min` (content floors win — fixes EDGE1).
3. If available width is indefinite (parent measuring the table's own min/max-content): return `col.min` (min-content pass) or `col.max` (max-content pass) per `inputs.available_space.width`.

**Step 2:** In the wrapper, store the resolved `Vec<f32>`; have `get_grid_container_style`/`get_core_container_style` return a per-call style whose `grid_template_columns = cols.map(length)`. (Add a `RefCell<Option<Style>>`/owned field on the wrapper; compute it at the top of the table branch in `mod.rs` before `compute_grid_layout`.)

**Step 3:** Run probe. Expect: TARGET TEST≈REF (col1≈360, col2≈40); EDGE1 col1≥150 no overflow; EDGE2 scaled to fit 200; sum<100% fills exactly.

**Step 4:** Run css-tables + CSS2/tables WPT. Expect PASS > 63, target/colspan-004 PASS, zero regressions vs baselines. Iterate the distribution rule (Task 4/5) against any regressions.

**Step 5:** Commit `"feat(table): L3 auto-mode column width distribution"`.

---

## Task 6: border-collapse / border-spacing correctness

**Files:** Modify `packages/blitz-dom/src/layout/table.rs`

**Step 1:** Ensure the inner-width math in Task 5 subtracts `(n_cols-1) * column_gap` (separate: border-spacing; collapse: the collapsed border the code already derives at L115-148) and the table's leading/trailing inset, so column px sum + gaps + insets == table width exactly (no ±4px). Add a probe case asserting `Σ col widths + gaps + insets == table content width`.

**Step 2:** WPT re-run; expect the `border-spacing`/`border-collapse` width tests and the §2 overflow class to improve or stay; zero regressions.

**Step 3:** Commit `"fix(table): account for gaps/border insets in column widths (border-box %)"`.

---

## Task 7: Fixed table-layout mode + `<col>` widths

**Files:** Modify `packages/blitz-dom/src/layout/table.rs` (the ignored `TableColumn`/`TableColumnGroup` arm at L322), `compute_table_columns`.

**Step 1:** Collect `<col>`/`<colgroup>` `width` (currently ignored) into a column-template vector.
**Step 2:** In `compute_table_columns`, when `is_fixed`: determine columns from `<col>` widths + first row only; percentages/lengths authoritative; remaining width to auto columns; content does NOT grow columns (overflow allowed). `is_fixed` is now genuinely used again.
**Step 3:** Probe: a plain fixed percent table unchanged; a fixed table whose content exceeds a column still clips (correct for fixed).
**Step 4:** WPT: expect `table_grid_size_col_colspan` (uses `<col>` + `table-layout:fixed`) FAIL→PASS; zero regressions.
**Step 5:** Commit `"feat(table): fixed table-layout column sizing + <col> width support"`.

---

## Task 8: Full regression sweep

**Step 1:** Run css-tables, CSS2/tables, and additionally `css/css-grid` and `css/css-flexbox` (the change must not touch non-table layout — confirm by diffing those suites' PASS counts vs a fresh baseline). Also spot-run a couple of real pages via `examples/screenshot`.
**Step 2:** Record final PASS deltas in the findings report (§7 update).
**Step 3:** Commit any fixups.

---

## Task 9: Cleanup — remove the hack and dead code

**Files:** Modify `packages/blitz-dom/src/layout/table.rs`; delete probe examples if not wanted in-tree.

**Step 1:** Remove the shipped 1-line PERCENT hack and the now-unused row==1 `columns` collection path that the L3 pass replaces (the old `grid_template_columns` build in `build_table_context`). Ensure `is_fixed` is either used (Task 7) or removed cleanly.
**Step 2:** Remove temporary logging. Keep or delete `examples/table_l3_probe.rs` per preference.
**Step 3:** Full build + clippy: `cargo clippy -p blitz-dom`. Final WPT sweep.
**Step 4:** Commit `"refactor(table): remove grid-track percent hack, superseded by L3 column sizing"`.

---

## Risks / open questions

- **Measure-pass interaction:** `compute_grid_layout` is itself invoked in measure passes (the table's own min/max-content for its parent). Resolving columns must behave for indefinite available width (Task 5 step 1.3). If injecting fixed tracks during a measure pass misbehaves, fall back to returning min/max sums directly without calling grid for those passes.
- **Colspan distribution rule:** the exact CSS Tables 3 distribution (which spanned columns absorb excess) may need iteration; start simple (equal/intrinsic-weighted), let WPT `colspan-001..004` drive refinement.
- **Performance:** the min/max measurement pre-pass adds per-cell layout calls; cache per-cell results within a single table layout. Acceptable (Taffy already measures items internally).
- **Scope:** captions, writing-modes, `visibility:collapse` rows/cols, and table-as-flex/grid-item are out of scope for this plan; keep their current behaviour.
