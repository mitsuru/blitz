use std::{ops::Range, sync::Arc};

use atomic_refcell::AtomicRefCell;
use markup5ever::local_name;
use style::properties::style_structs::Border;
use style::servo_arc::Arc as ServoArc;
use style::values::specified::box_::{DisplayInside, DisplayOutside};
use style::{
    Atom, computed_values::border_collapse::T as BorderCollapse,
    computed_values::table_layout::T as TableLayout,
};
use taffy::{
    DetailedGridInfo, LayoutPartialTree as _, ResolveOrZero, TrackSizingFunction, style_helpers,
};

use crate::BaseDocument;

use super::damage::{CONSTRUCT_BOX, CONSTRUCT_DESCENDENT, CONSTRUCT_FC};
use super::resolve_calc_value;

pub struct TableTreeWrapper<'doc> {
    pub(crate) doc: &'doc mut BaseDocument,
    pub(crate) ctx: Arc<TableContext>,
    /// Per-layout-call container style. When set, its `grid_template_columns`
    /// holds the L3-resolved fixed pixel column widths and is served to Taffy in
    /// place of `ctx.style` (whose template tracks are only a fallback). Computed
    /// by [`TableTreeWrapper::prepare_columns`] before `compute_grid_layout`.
    pub(crate) style_override: Option<taffy::Style<Atom>>,
}

#[derive(Debug, Clone)]
pub struct TableContext {
    pub style: taffy::Style<Atom>,
    pub cells: Vec<TableCell>,
    pub rows: Vec<TableRow>,
    pub computed_grid_info: AtomicRefCell<Option<DetailedGridInfo>>,
    pub border_style: Option<ServoArc<Border>>,
    pub border_collapse: BorderCollapse,
}

// #[derive(Debug, Clone, Eq, PartialEq)]
// pub enum TableItemKind {
//     Row,
//     Cell,
// }

#[derive(Debug, Clone)]
pub struct TableCell {
    // kind: TableItemKind,
    node_id: usize,
    style: taffy::Style<Atom>,
    /// 0-based grid column of the cell's left edge.
    col_start: u16,
    /// Number of columns the cell spans (>= 1).
    colspan: u16,
    /// The cell's authored inline size, captured before `style.size.width` is
    /// reset to `auto()` for Taffy placement. Drives the L3 column algorithm
    /// (PERCENT_TAG / LENGTH_TAG / AUTO_TAG carried verbatim).
    specified_width: taffy::Dimension,
}

#[derive(Debug, Clone)]
pub struct TableRow {
    // kind: TableItemKind,
    pub node_id: usize,
    pub height: f32,
}

pub(crate) fn build_table_context(
    doc: &mut BaseDocument,
    table_root_node_id: usize,
) -> (TableContext, Vec<usize>) {
    let mut cells: Vec<TableCell> = Vec::new();
    let mut rows: Vec<TableRow> = Vec::new();
    let mut row = 0u16;
    let mut col = 0u16;

    let root_node = &mut doc.nodes[table_root_node_id];

    let children = std::mem::take(&mut root_node.children);

    let Some(stylo_styles) = root_node.primary_styles() else {
        panic!("Ignoring table because it has no styles");
    };

    let mut style = stylo_taffy::to_taffy_style(&stylo_styles);
    style.item_is_table = true;
    // Use `dense` row-flow so that each cell scans the row from its
    // leftmost column for the first free track. Without `dense`,
    // `place_definite_secondary_axis_item` keeps a per-item secondary
    // cursor across rows, which means cells in later rows do not
    // backfill columns freed up by rowspan cells from earlier rows.
    style.grid_auto_flow = taffy::GridAutoFlow::RowDense;
    style.grid_auto_columns = Vec::new();
    style.grid_auto_rows = Vec::new();

    let is_fixed = match stylo_styles.clone_table_layout() {
        TableLayout::Fixed => true,
        TableLayout::Auto => false,
    };

    let border_collapse = stylo_styles.clone_border_collapse();
    let border_spacing = stylo_styles.clone_border_spacing().0;

    drop(stylo_styles);

    let mut column_sizes: Vec<taffy::TrackSizingFunction> = Vec::new();
    let mut first_cell_border: Option<ServoArc<Border>> = None;
    for child_id in children.iter().copied() {
        collect_table_cells(
            doc,
            child_id,
            is_fixed,
            border_collapse,
            &mut row,
            &mut col,
            &mut cells,
            &mut rows,
            &mut column_sizes,
            &mut first_cell_border,
        );
    }
    column_sizes.resize(col as usize, style_helpers::auto());

    style.grid_template_columns = column_sizes.into_iter().map(|dim| dim.into()).collect();
    style.grid_template_rows = vec![style_helpers::auto(); row as usize];

    style.gap = match border_collapse {
        BorderCollapse::Separate => taffy::Size {
            width: style_helpers::length(border_spacing.width.px()),
            height: style_helpers::length(border_spacing.height.px()),
        },
        BorderCollapse::Collapse => first_cell_border
            .as_ref()
            .map(|border| {
                let x = border
                    .border_left_width
                    .0
                    .max(border.border_right_width.0)
                    .to_f32_px();
                let y = border
                    .border_top_width
                    .0
                    .max(border.border_bottom_width.0)
                    .to_f32_px();
                taffy::Size {
                    width: style_helpers::length(x),
                    height: style_helpers::length(y),
                }
            })
            .unwrap_or(taffy::Size::ZERO.map(style_helpers::length)),
    };

    if border_collapse == BorderCollapse::Collapse {
        style.border = taffy::Rect {
            left: style.gap.width,
            right: style.gap.width,
            top: style.gap.height,
            bottom: style.gap.height,
        };
    }

    let layout_children = cells.iter().map(|cell| cell.node_id).collect();
    let root_node = &mut doc.nodes[table_root_node_id];
    root_node.children = children;

    (
        TableContext {
            style,
            cells,
            rows,
            computed_grid_info: AtomicRefCell::new(None),
            border_collapse,
            border_style: first_cell_border,
        },
        layout_children,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_table_cells(
    doc: &mut BaseDocument,
    node_id: usize,
    is_fixed: bool,
    border_collapse: BorderCollapse,
    row: &mut u16,
    col: &mut u16,
    cells: &mut Vec<TableCell>,
    rows: &mut Vec<TableRow>,
    columns: &mut Vec<TrackSizingFunction>,
    first_cell_border: &mut Option<ServoArc<Border>>,
) {
    let node = &mut doc.nodes[node_id];

    if !node.is_element() {
        return;
    }

    let Some(display) = node.primary_styles().map(|s| s.clone_display()) else {
        #[cfg(feature = "tracing")]
        tracing::info!("Ignoring table descendent because it has no styles");
        return;
    };

    if display.outside() == DisplayOutside::None {
        node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
        return;
    }

    match display.inside() {
        DisplayInside::TableRowGroup
        | DisplayInside::TableHeaderGroup
        | DisplayInside::TableFooterGroup
        | DisplayInside::Contents => {
            let children = std::mem::take(&mut doc.nodes[node_id].children);
            for child_id in children.iter().copied() {
                doc.nodes[child_id]
                    .remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                collect_table_cells(
                    doc,
                    child_id,
                    is_fixed,
                    border_collapse,
                    row,
                    col,
                    cells,
                    rows,
                    columns,
                    first_cell_border,
                );
            }
            doc.nodes[node_id].children = children;
        }
        DisplayInside::TableRow => {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            *row += 1;
            *col = 0;

            rows.push(TableRow {
                node_id,
                height: 0.0,
            });

            let children = std::mem::take(&mut doc.nodes[node_id].children);
            for child_id in children.iter().copied() {
                collect_table_cells(
                    doc,
                    child_id,
                    is_fixed,
                    border_collapse,
                    row,
                    col,
                    cells,
                    rows,
                    columns,
                    first_cell_border,
                );
            }
            doc.nodes[node_id].children = children;
        }
        DisplayInside::TableCell => {
            // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            let stylo_style = &node.primary_styles().unwrap();
            let colspan: u16 = node
                .attr(local_name!("colspan"))
                .and_then(|val| val.parse().ok())
                .unwrap_or(1);
            let rowspan: u16 = node
                .attr(local_name!("rowspan"))
                .and_then(|val| val.parse::<u16>().ok())
                .map(|v| v.clamp(1, 65534))
                .unwrap_or(1);
            let mut style = stylo_taffy::to_taffy_style(stylo_style);

            if first_cell_border.is_none() {
                *first_cell_border = Some(stylo_style.clone_border());
            }

            // TODO: account for padding/border/margin
            if *row == 1 {
                let column = match style.size.width.tag() {
                    taffy::CompactLength::LENGTH_TAG => {
                        let len = style.size.width.value();
                        let padding = style.padding.resolve_or_zero(None, resolve_calc_value);
                        style_helpers::length(len + padding.left + padding.right)
                    }
                    taffy::CompactLength::PERCENT_TAG => {
                        if is_fixed {
                            style_helpers::percent(style.size.width.value())
                        } else {
                            style_helpers::auto()
                        }
                    }
                    taffy::CompactLength::AUTO_TAG => style_helpers::auto(),
                    _ => unreachable!(),
                };
                columns.push(column);
            }

            // Zero-out cell borders is BorderCollapse is Collapse
            // Borders are handled at the table level in this mode
            if border_collapse == BorderCollapse::Collapse {
                style.border = taffy::Rect::ZERO.map(style_helpers::length);
            }

            // Let Taffy auto-place the column. Combined with
            // `grid_auto_flow: RowDense` set on the table root, each cell
            // scans from the first track in its row for a free position,
            // which makes cells automatically skip columns occupied by
            // rowspan cells from earlier rows.
            style.grid_column = taffy::Line {
                start: style_helpers::auto(),
                end: style_helpers::span(colspan),
            };
            style.grid_row = taffy::Line {
                start: style_helpers::line(*row as i16),
                end: style_helpers::span(rowspan),
            };
            let specified_width = style.size.width;
            style.size.width = style_helpers::auto();
            cells.push(TableCell {
                node_id,
                style,
                col_start: *col,
                colspan,
                specified_width,
            });

            *col += colspan;
        }
        DisplayInside::Flow
        | DisplayInside::FlowRoot
        | DisplayInside::Flex
        | DisplayInside::Grid => {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            // Probably a table caption: ignore
            // println!(
            //     "Warning: ignoring non-table typed descendent of table ({:?})",
            //     display.inside()
            // );
        }
        DisplayInside::TableColumnGroup | DisplayInside::TableColumn | DisplayInside::Table => {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            //Ignore
        }
        DisplayInside::None => {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            // Ignore
        }
    }
}

pub struct RangeIter(Range<usize>);

impl Iterator for RangeIter {
    type Item = taffy::NodeId;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(taffy::NodeId::from)
    }
}

impl TableTreeWrapper<'_> {
    /// Compute the table's column widths with the CSS Tables L3 algorithm and
    /// stash them in [`Self::style_override`] as fixed pixel `length()` tracks,
    /// so the subsequent `compute_grid_layout` call only does cell placement and
    /// row sizing rather than (mis)sizing columns via generic grid track rules.
    pub(crate) fn prepare_columns(&mut self, inputs: taffy::tree::LayoutInput) {
        let n = self.ctx.style.grid_template_columns.len();
        if n == 0 {
            return;
        }

        // (1) Snapshot per-cell facts so we can measure without borrowing `ctx`.
        struct CellFact {
            idx: usize,
            col: usize,
            span: usize,
            pct: Option<f32>,
        }
        let facts: Vec<CellFact> = self
            .ctx
            .cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let pct = (c.specified_width.tag() == taffy::CompactLength::PERCENT_TAG)
                    .then(|| c.specified_width.value());
                CellFact {
                    idx: i,
                    col: c.col_start as usize,
                    span: (c.colspan.max(1)) as usize,
                    pct,
                }
            })
            .collect();

        // Only take over column sizing where Taffy's generic grid track sizing is
        // actually wrong for tables: percentage column widths and/or spanning
        // (colspan) cells. Plain auto/length tables keep Taffy's native sizing
        // (and its exact sub-pixel results), so this change can't regress them.
        let needs_l3 = facts.iter().any(|f| f.pct.is_some() || f.span > 1);
        if !needs_l3 {
            return;
        }

        // (2) Measure each cell's content min/max-content inline size with an
        // INDEFINITE parent width, so the cell's own `width:%` resolves to auto
        // (percentages don't resolve during intrinsic sizing) and the true
        // content — including wide fixed-size children — surfaces. Absolute
        // `width:<len>` still resolves to its length.
        let mut cell_min = vec![0f32; facts.len()];
        let mut cell_max = vec![0f32; facts.len()];
        for f in &facts {
            cell_min[f.idx] = self.measure_cell(f.idx, taffy::AvailableSpace::MinContent);
            cell_max[f.idx] = self.measure_cell(f.idx, taffy::AvailableSpace::MaxContent);
        }

        // (3) Aggregate per-column min/max from non-spanning (colspan==1) cells.
        // These single-cell content sizes are the floors used when the table has a
        // definite width: a spanning cell's own content must NOT inflate an
        // individual column there (it overflows instead — matching browsers), so
        // spanning min/max only feed the table's intrinsic size (the `_full`
        // arrays below, used in the indefinite/measure passes).
        let mut col_min = vec![0f32; n];
        let mut col_max = vec![0f32; n];
        let mut col_pct = vec![0f32; n];
        for f in &facts {
            if f.span == 1 && f.col < n {
                col_min[f.col] = col_min[f.col].max(cell_min[f.idx]);
                col_max[f.col] = col_max[f.col].max(cell_max[f.idx]);
                if let Some(p) = f.pct {
                    col_pct[f.col] = col_pct[f.col].max(p);
                }
            }
        }
        let mut col_min_full = col_min.clone();
        let mut col_max_full = col_max.clone();
        // Spanning cells distribute any excess min/max/percent across the columns
        // they span (smallest spans first), never dropping a single column below
        // its own intrinsic size — so a colspan cell is not the sole determiner of
        // any one column's width (CSS Tables 3 spanning distribution). Percent is
        // shared by both code paths; min/max feed only the intrinsic arrays.
        let mut spans: Vec<&CellFact> = facts.iter().filter(|f| f.span > 1).collect();
        spans.sort_by_key(|f| f.span);
        for f in spans {
            let end = (f.col + f.span).min(n);
            if f.col >= end {
                continue;
            }
            let cnt = (end - f.col) as f32;
            let cur_min: f32 = (f.col..end).map(|i| col_min_full[i]).sum();
            if cell_min[f.idx] > cur_min {
                let add = (cell_min[f.idx] - cur_min) / cnt;
                for i in f.col..end {
                    col_min_full[i] += add;
                }
            }
            let cur_max: f32 = (f.col..end).map(|i| col_max_full[i]).sum();
            if cell_max[f.idx] > cur_max {
                let add = (cell_max[f.idx] - cur_max) / cnt;
                for i in f.col..end {
                    col_max_full[i] += add;
                }
            }
            if let Some(p) = f.pct {
                let cur_p: f32 = (f.col..end).map(|i| col_pct[i]).sum();
                if p > cur_p {
                    let add = (p - cur_p) / cnt;
                    for i in f.col..end {
                        col_pct[i] += add;
                    }
                }
            }
        }
        for i in 0..n {
            col_max[i] = col_max[i].max(col_min[i]);
            col_max_full[i] = col_max_full[i].max(col_min_full[i]);
        }

        // (4) Resolve the inset (table border/padding + inter-column gaps) that
        // sits between the table border-box width and the column area.
        let parent = inputs.parent_size;
        let border = self.ctx.style.border.resolve_or_zero(parent, resolve_calc_value);
        let padding = self.ctx.style.padding.resolve_or_zero(parent, resolve_calc_value);
        let gap = self
            .ctx
            .style
            .gap
            .width
            .resolve_or_zero(parent.width, resolve_calc_value);
        let insets =
            border.left + border.right + padding.left + padding.right + gap * (n as f32 - 1.0);

        // (5) Pick column pixel widths.
        // Resolve the table's own definite width (length, or percentage of a
        // definite containing block). `auto` stays `None` → shrink-to-fit.
        let own_width = |avail_basis: Option<f32>| -> Option<f32> {
            let w = &self.ctx.style.size.width;
            match w.tag() {
                taffy::CompactLength::LENGTH_TAG => Some(w.value()),
                taffy::CompactLength::PERCENT_TAG => avail_basis.map(|b| b * w.value()),
                _ => None,
            }
        };
        let widths: Vec<f32> = if let Some(table_w) = inputs.known_dimensions.width {
            // Definite border-box width already resolved by the parent.
            distribute_columns((table_w - insets).max(0.0), &col_min, &col_max, &col_pct)
        } else {
            match inputs.available_space.width {
                // Intrinsic-size passes: percentages are auto, spanning content
                // contributes — report the table's own min/max-content columns.
                taffy::AvailableSpace::MinContent => col_min_full.clone(),
                taffy::AvailableSpace::MaxContent => col_max_full.clone(),
                taffy::AvailableSpace::Definite(avail_w) => {
                    if let Some(table_w) = own_width(Some(avail_w)) {
                        // Table has a definite width → fill it.
                        distribute_columns((table_w - insets).max(0.0), &col_min, &col_max, &col_pct)
                    } else {
                        // Auto width → shrink-to-fit, capped at the available space.
                        let avail = (avail_w - insets).max(0.0);
                        if col_max_full.iter().sum::<f32>() <= avail {
                            col_max_full.clone()
                        } else {
                            distribute_columns(avail, &col_min, &col_max, &col_pct)
                        }
                    }
                }
            }
        };

        // (6) Serve the resolved fixed-length tracks; Taffy now only places cells
        // and sizes rows.
        let mut style = self.ctx.style.clone();
        style.grid_template_columns = widths
            .into_iter()
            .map(|w| {
                let track: TrackSizingFunction = style_helpers::length(w);
                track.into()
            })
            .collect();
        self.style_override = Some(style);
    }

    fn measure_cell(&mut self, idx: usize, width: taffy::AvailableSpace) -> f32 {
        use taffy::tree::LayoutInput;
        self.compute_child_layout(
            idx.into(),
            LayoutInput {
                run_mode: taffy::RunMode::ComputeSize,
                sizing_mode: taffy::SizingMode::InherentSize,
                axis: taffy::RequestedAxis::Horizontal,
                known_dimensions: taffy::Size::NONE,
                parent_size: taffy::Size {
                    width: None,
                    height: None,
                },
                available_space: taffy::Size {
                    width,
                    height: taffy::AvailableSpace::MaxContent,
                },
                vertical_margins_are_collapsible: taffy::Line {
                    start: false,
                    end: false,
                },
            },
        )
        .size
        .width
    }
}

/// Distribute a definite `avail` width across columns described by their
/// content `min`/`max` and intrinsic `pct` (0.0 = no percentage). Percentage
/// columns claim `pct * avail` (floored by their content min, scaled down if the
/// percentages exceed 100%); the remaining columns take at least their content
/// min and then absorb leftover space (first growing toward max-content, then
/// stretching). When percentages over-allocate, columns shrink toward their min.
fn distribute_columns(avail: f32, min: &[f32], max: &[f32], pct: &[f32]) -> Vec<f32> {
    let n = min.len();
    let psum: f32 = pct.iter().sum();
    let scale = if psum > 1.0 { 1.0 / psum } else { 1.0 };

    let mut w = vec![0f32; n];
    for i in 0..n {
        w[i] = if pct[i] > 0.0 {
            (pct[i] * scale * avail).max(min[i])
        } else {
            min[i]
        };
    }

    let assigned: f32 = w.iter().sum();
    if assigned + 0.01 < avail {
        let mut remaining = avail - assigned;
        // Grow non-percentage columns toward their max-content first.
        let grow: Vec<f32> = (0..n)
            .map(|i| if pct[i] == 0.0 { (max[i] - w[i]).max(0.0) } else { 0.0 })
            .collect();
        let gsum: f32 = grow.iter().sum();
        if gsum > 0.0 {
            let take = remaining.min(gsum);
            for i in 0..n {
                w[i] += take * grow[i] / gsum;
            }
            remaining -= take;
        }
        // Stretch any leftover across non-percentage columns (or all columns if
        // every column is a percentage column).
        if remaining > 0.0 {
            let targets: Vec<usize> = {
                let np: Vec<usize> = (0..n).filter(|&i| pct[i] == 0.0).collect();
                if np.is_empty() { (0..n).collect() } else { np }
            };
            let share = remaining / targets.len() as f32;
            for i in targets {
                w[i] += share;
            }
        }
    } else if assigned > avail + 0.01 {
        // Percentages over-allocated: shrink columns toward their min.
        let excess = assigned - avail;
        let shrink: Vec<f32> = (0..n).map(|i| (w[i] - min[i]).max(0.0)).collect();
        let ssum: f32 = shrink.iter().sum();
        if ssum > 0.0 {
            let take = excess.min(ssum);
            for i in 0..n {
                w[i] -= take * shrink[i] / ssum;
            }
        }
    }
    w
}

impl taffy::TraversePartialTree for TableTreeWrapper<'_> {
    type ChildIter<'a>
        = RangeIter
    where
        Self: 'a;

    #[inline(always)]
    fn child_ids(&self, _node_id: taffy::NodeId) -> Self::ChildIter<'_> {
        RangeIter(0..self.ctx.cells.len())
    }

    #[inline(always)]
    fn child_count(&self, _node_id: taffy::NodeId) -> usize {
        self.ctx.cells.len()
    }

    #[inline(always)]
    fn get_child_id(&self, _node_id: taffy::NodeId, index: usize) -> taffy::NodeId {
        index.into()
    }
}
impl taffy::TraverseTree for TableTreeWrapper<'_> {}

impl taffy::LayoutPartialTree for TableTreeWrapper<'_> {
    type CoreContainerStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    type CustomIdent = Atom;

    fn get_core_container_style(&self, _node_id: taffy::NodeId) -> &taffy::Style<Atom> {
        self.style_override.as_ref().unwrap_or(&self.ctx.style)
    }

    fn resolve_calc_value(&self, calc_ptr: *const (), parent_size: f32) -> f32 {
        resolve_calc_value(calc_ptr, parent_size)
    }

    fn set_unrounded_layout(&mut self, node_id: taffy::NodeId, layout: &taffy::Layout) {
        let node_id = taffy::NodeId::from(self.ctx.cells[usize::from(node_id)].node_id);
        self.doc.set_unrounded_layout(node_id, layout)
    }

    fn compute_child_layout(
        &mut self,
        node_id: taffy::NodeId,
        inputs: taffy::tree::LayoutInput,
    ) -> taffy::LayoutOutput {
        let cell = &self.ctx.cells[usize::from(node_id)];
        let node_id = taffy::NodeId::from(cell.node_id);
        self.doc.compute_child_layout(node_id, inputs)
    }
}

impl taffy::LayoutGridContainer for TableTreeWrapper<'_> {
    type GridContainerStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    type GridItemStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    fn get_grid_container_style(&self, node_id: taffy::NodeId) -> Self::GridContainerStyle<'_> {
        self.get_core_container_style(node_id)
    }

    fn get_grid_child_style(&self, child_node_id: taffy::NodeId) -> Self::GridItemStyle<'_> {
        &self.ctx.cells[usize::from(child_node_id)].style
    }

    fn set_detailed_grid_info(
        &mut self,
        _node_id: taffy::NodeId,
        detailed_grid_info: DetailedGridInfo,
    ) {
        *self.ctx.computed_grid_info.borrow_mut() = Some(detailed_grid_info);
    }
}
