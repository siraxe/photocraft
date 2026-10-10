//! The preset grid (the Brush Preset picker's and the Brushes tab's) is centred: every row has the
//! same left margin, and the spare width is split between the two sides.

use egui::{Rect, accesskit::Role, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};

use super::*;

#[test]
fn grid_columns_centre_whole_columns_past_the_indent() {
    // 6 × 44 + 5 × 3 = 279 of the 290 past a 4 pt indent: 5 spare on each side (rounded down).
    assert_eq!(grid_columns(294.0, 44.0, 4.0), (9.0, 6));
    // An exact fit leaves only the indent.
    assert_eq!(grid_columns(4.0 + 279.0, 44.0, 4.0), (4.0, 6));
    // One point short of a column: one fewer, centred.
    assert_eq!(grid_columns(4.0 + 278.0, 44.0, 4.0), (4.0 + 23.0, 5));
    for (w, cell, indent) in [(294.0, 44.0, 4.0), (360.0, 52.0, 20.0), (500.0, 52.0, 20.0)] {
        let (left, cols) = grid_columns(w, cell, indent);
        let right = w - left - (cols as f32 * cell + (cols - 1) as f32 * GRID_GAP);
        assert!(((left - indent) - right).abs() <= 1.0, "{w}: left {left}, right {right}");
    }
}

#[test]
fn grid_columns_never_panic_or_return_no_columns() {
    for w in [0.0, -50.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e30] {
        for cell in [0.0, -1.0, f32::NAN, f32::INFINITY, 44.0, 1e-30] {
            let (left, cols) = grid_columns(w, cell, 4.0);
            assert!((1..=256).contains(&cols), "{w} {cell}: {cols}");
            assert!(left.is_finite() && left >= 0.0, "{w} {cell}: {left}");
        }
    }
    // Narrower than one cell: one column at the indent.
    assert_eq!(grid_columns(30.0, 44.0, 4.0), (4.0, 1));
}

/// Checks each of the first `groups` groups' rows against its header (which spans the list's
/// width): the same left margin on every row, and on full rows the spare width split evenly.
fn rows_are_centred(h: &Harness<'_, PhotocraftApp>, indent: f32, groups: usize) {
    let presets = h.state().session.tools.presets.clone();
    let grouped = grouped_presets(&presets);
    let headers: Vec<Rect> = grouped
        .iter()
        .take(groups + 1)
        .filter_map(|(label, _)| {
            h.query_all_by_role(Role::Button)
                .chain(h.query_all_by_label(label))
                .find(|n| n.accesskit_node().label().as_deref() == Some(label.as_str()))
                .map(|n| n.rect())
        })
        .collect();
    assert!(headers.len() > groups, "{} group headers", headers.len());
    let tiles: Vec<Rect> = h
        .query_all_by_role(Role::Button)
        .filter(|n| n.accesskit_node().label().is_some_and(|l| presets.iter().any(|p| p.name == l)))
        .map(|n| n.rect())
        .collect();
    // Each group's rows of tiles, top to bottom.
    let mut groups_rows: Vec<Vec<Vec<Rect>>> = Vec::new();
    for (g, header) in headers.iter().take(groups).enumerate() {
        let next = headers[g + 1].top();
        let mut rows: Vec<Vec<Rect>> = Vec::new();
        for r in tiles.iter().filter(|r| r.top() >= header.bottom() && r.bottom() <= next) {
            match rows.iter_mut().find(|row| (row[0].top() - r.top()).abs() < 0.5) {
                Some(row) => row.push(*r),
                None => rows.push(vec![*r]),
            }
        }
        assert!(!rows.is_empty(), "group {g}: no tiles");
        groups_rows.push(rows);
    }
    let wrapped = groups_rows.iter().any(|rows| rows.len() > 1);
    // A full row is as wide as the widest in any group (a short group can fill only part of one).
    let cols = groups_rows.iter().flatten().map(Vec::len).max().unwrap_or(0);
    let (list_left, list_right) = (headers[0].left(), headers[0].right());
    let left = |row: &Vec<Rect>| row.iter().map(|r| r.left()).fold(f32::MAX, f32::min) - list_left;
    let first = left(&groups_rows[0][0]);
    for (g, rows) in groups_rows.iter().enumerate() {
        for row in rows {
            assert!((left(row) - first).abs() < 0.5, "group {g}: a row starts {} pt in, the first {first}", left(row));
            if row.len() == cols {
                let right = list_right - row.iter().map(|r| r.right()).fold(f32::MIN, f32::max);
                assert!(((first - indent) - right).abs() <= 1.0, "group {g}: {} pt left of the grid, {right} pt right", first - indent);
            }
        }
    }
    assert!(wrapped, "no group wrapped onto a second row, so the rows weren't compared");
}

/// The picker's cards: one full-width 2 × 2 card per preset (tip and stroke on top, the name
/// across the bottom), and a hidden part shrinks the card — with only the tip and the name on,
/// the name takes the stroke's cell and the card takes its own height ([`CARD_TIP_NAME_H`]) and
/// width ([`CARD_TIP_NAME_W`]); with only the tip on, the card takes the tip-only cell's own
/// height.
#[test]
fn brush_picker_cards_follow_the_part_boxes() {
    let mut h = Harness::builder().with_size(vec2(1440.0, 900.0)).with_max_steps(64).build_eframe(|cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, Default::default());
        PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default())
    });
    h.state_mut().run("file.new", serde_json::json!({"width": 400, "height": 300})).unwrap();
    h.state_mut().ui.tool = crate::state::Tool::Brush;
    h.state_mut().ui.brush_picker = Some([300.0, 120.0]);
    h.run_steps(8);
    let presets = h.state().session.tools.presets.clone();
    let cards = |h: &Harness<'_, PhotocraftApp>| -> Vec<Rect> {
        h.query_all_by_role(Role::Button)
            .filter(|n| n.accesskit_node().label().is_some_and(|l| presets.iter().any(|p| p.name == l)))
            .map(|n| n.rect())
            .collect()
    };
    let full = CARD_MAX_H;
    let one_row = CARD_MAX_H - CARD_NAME_H;
    let all_on = cards(&h);
    assert!(all_on.len() >= 2, "{} cards", all_on.len());
    assert!(all_on.iter().all(|r| (r.height() - full).abs() < 0.5 && r.width() > 250.0), "{all_on:?}");
    h.state_mut().ui.brush_picker_list.show_stroke = false;
    h.run_steps(2);
    assert!(
        cards(&h).iter().all(|r| (r.height() - CARD_TIP_NAME_H).abs() < 0.5 && (r.width() - CARD_TIP_NAME_W).abs() < 0.5),
        "tip + name: the name in the stroke's cell, its own cell size"
    );
    h.state_mut().ui.brush_picker_list.show_name = false;
    h.run_steps(2);
    assert!(cards(&h).iter().all(|r| (r.height() - CARD_TIP_ONLY_H).abs() < 0.5), "tip only: its own cell");
    h.state_mut().ui.brush_picker_list.show_stroke = true;
    h.run_steps(2);
    assert!(cards(&h).iter().all(|r| (r.height() - one_row).abs() < 0.5), "tip + stroke: one row");
}

#[test]
fn the_brushes_tab_grid_is_centred_past_its_indent() {
    let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
    app.ui.panels.brush_settings = true;
    app.ui.brush_tab = 1;
    app.ui.brushes_panel.view = BrushesView::Grid;
    let mut h = Harness::builder().with_size(vec2(1200.0, 900.0)).build_ui_state(
        |ui, app: &mut PhotocraftApp| {
            let ctx = ui.ctx().clone();
            if !ctx.fonts(|f| f.families().contains(&egui::FontFamily::Name("medium".into()))) {
                return;
            }
            crate::brush_panel::window(app, &ctx);
        },
        app,
    );
    PhotocraftApp::setup_context(&h.ctx, crate::theme::ThemeKind::ALL[0]);
    h.run_steps(6);
    rows_are_centred(&h, PANEL_LIST.indent, 2);
}
