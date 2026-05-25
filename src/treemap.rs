use egui::Rect;
use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub struct Tile<T> {
    pub item: T,
    pub size: u128,
    pub rect: Rect,
}

#[derive(Debug, Clone)]
pub struct TreemapItem<T> {
    pub item: T,
    pub size: u128,
}

const MAX_DOMINANT_SHARE: f32 = 0.5;
const TILE_GAP: f32 = 3.0;
const MIN_READABLE_TILE_SIDE: f32 = 56.0;
const READABILITY_FLOOR_BUDGET_SHARE: f32 = 0.38;
const MAX_PAINTED_ROW_RATIO: f32 = 2.13;

pub fn layout<T: Clone>(items: &[TreemapItem<T>], rect: Rect) -> Vec<Tile<T>> {
    if items.is_empty() || rect.width() <= 0.0 || rect.height() <= 0.0 {
        return Vec::new();
    }

    let mut weighted = items
        .iter()
        .filter(|item| item.size > 0)
        .map(|item| WeightedItem {
            item: item.item.clone(),
            size: item.size,
            layout_size: item.size,
        })
        .collect::<Vec<_>>();

    if weighted.is_empty() {
        return Vec::new();
    }

    weighted.sort_by_key(|item| std::cmp::Reverse(item.size));
    cap_dominant_layout_weight(&mut weighted);
    let mut out = Vec::with_capacity(weighted.len());
    layout_squarified(&weighted, rect, &mut out);
    out
}

#[derive(Debug, Clone)]
struct WeightedItem<T> {
    item: T,
    size: u128,
    layout_size: u128,
}

#[derive(Debug, Clone)]
struct AreaItem<T> {
    item: T,
    size: u128,
    area: f32,
}

fn cap_dominant_layout_weight<T>(items: &mut [WeightedItem<T>]) {
    if items.len() < 2 {
        return;
    }

    let rest_size = items[1..].iter().map(|item| item.layout_size).sum::<u128>();
    let max_dominant =
        (rest_size as f32 * MAX_DOMINANT_SHARE / (1.0 - MAX_DOMINANT_SHARE)).round() as u128;
    if max_dominant > 0 && items[0].layout_size > max_dominant {
        items[0].layout_size = max_dominant;
    }
}

fn layout_squarified<T: Clone>(items: &[WeightedItem<T>], rect: Rect, out: &mut Vec<Tile<T>>) {
    if rect.width() < 1.0 || rect.height() < 1.0 {
        return;
    }

    if items.len() == 1 {
        out.push(Tile {
            item: items[0].item.clone(),
            size: items[0].size,
            rect: apply_gap(rect),
        });
        return;
    }

    let total_size = items.iter().map(|item| item.layout_size).sum::<u128>();
    if total_size == 0 {
        return;
    }

    let total_area = rect.width() * rect.height();
    let mut pending = area_items(items, total_size, total_area);

    let mut remaining_rect = rect;
    let mut row = Vec::new();
    while let Some(next) = pending.pop_front() {
        if row.is_empty() {
            row.push(next);
            continue;
        }

        let side = remaining_rect.width().min(remaining_rect.height()).max(1.0);
        let current_score = worst_aspect_ratio(&row, side);
        let mut candidate = row.clone();
        candidate.push(next.clone());
        let current_painted_score = best_row_score(&row, remaining_rect);
        let candidate_painted_score = best_row_score(&candidate, remaining_rect);
        let keeps_row_square = worst_aspect_ratio(&candidate, side) <= current_score
            && candidate_painted_score <= MAX_PAINTED_ROW_RATIO;
        let repairs_painted_strip = current_painted_score > MAX_PAINTED_ROW_RATIO
            && candidate_painted_score < current_painted_score;
        let avoids_singleton_tail = pending.is_empty()
            && candidate_painted_score
                <= split_with_singleton_tail_score(&row, &next, remaining_rect);
        let avoids_tiny_tail_strip = should_fold_tiny_tail(&candidate, &pending, remaining_rect);
        if avoids_tiny_tail_strip {
            row.push(next);
            row.extend(pending.drain(..));
            break;
        }
        if keeps_row_square || repairs_painted_strip || avoids_singleton_tail {
            row.push(next);
        } else {
            remaining_rect = layout_row(&row, remaining_rect, out);
            row.clear();
            row.push(next);
        }
    }

    if !row.is_empty() {
        layout_final_row(&row, remaining_rect, out);
    }
}

fn area_items<T: Clone>(
    items: &[WeightedItem<T>],
    total_size: u128,
    total_area: f32,
) -> VecDeque<AreaItem<T>> {
    let minimum_area = readable_area_floor(total_area, items.len());
    let mut area_items = items
        .iter()
        .map(|item| AreaItem {
            item: item.item.clone(),
            size: item.size,
            area: (total_area * item.layout_size as f32 / total_size as f32).max(minimum_area),
        })
        .collect::<Vec<_>>();

    // Normalize after applying the readability floor so row layout still consumes
    // exactly the viewport area instead of leaking past the parent rect.
    let floored_area = area_items.iter().map(|item| item.area).sum::<f32>();
    if floored_area > f32::EPSILON {
        let scale = total_area / floored_area;
        for item in &mut area_items {
            item.area *= scale;
        }
    }

    area_items.into()
}

fn readable_area_floor(total_area: f32, item_count: usize) -> f32 {
    if item_count == 0 || total_area <= 0.0 {
        return 0.0;
    }

    let ideal_floor = MIN_READABLE_TILE_SIDE * MIN_READABLE_TILE_SIDE;
    let budgeted_floor = total_area * READABILITY_FLOOR_BUDGET_SHARE / item_count as f32;
    ideal_floor.min(budgeted_floor).max(1.0)
}

fn best_row_score<T>(row: &[AreaItem<T>], rect: Rect) -> f32 {
    vertical_row_score(row, rect).min(horizontal_row_score(row, rect))
}

fn split_with_singleton_tail_score<T>(row: &[AreaItem<T>], next: &AreaItem<T>, rect: Rect) -> f32 {
    let row_score = best_row_score(row, rect);
    let tail_rect = remaining_after_row(row, rect);
    let tail_score = if tail_rect.width() > 0.0 && tail_rect.height() > 0.0 {
        gapped_aspect_ratio_for_area(next.area, tail_rect)
    } else {
        f32::INFINITY
    };

    row_score.max(tail_score)
}

fn should_fold_tiny_tail<T>(
    candidate: &[AreaItem<T>],
    pending: &VecDeque<AreaItem<T>>,
    rect: Rect,
) -> bool {
    if pending.len() == 2 || pending.len() > 8 || candidate.len() + pending.len() > 10 {
        return false;
    }

    let tail_rect = remaining_after_row(candidate, rect);
    if tail_rect.width() < 1.0 || tail_rect.height() < 1.0 {
        return false;
    }

    let tail_grid_score = best_grid_score(pending.len(), tail_rect);
    let combined_grid_score = best_grid_score(candidate.len() + pending.len(), rect);
    let tail_is_visibly_skinny =
        aspect_ratio(tail_rect.width(), tail_rect.height()) > MAX_PAINTED_ROW_RATIO;
    (tail_grid_score > MAX_PAINTED_ROW_RATIO || tail_is_visibly_skinny)
        && combined_grid_score < tail_grid_score
}

fn remaining_after_row<T>(row: &[AreaItem<T>], rect: Rect) -> Rect {
    let row_area = row.iter().map(|item| item.area).sum::<f32>();
    if vertical_row_score(row, rect) <= horizontal_row_score(row, rect) {
        let row_width = (row_area / rect.height().max(1.0)).min(rect.width());
        Rect::from_min_max(egui::pos2(rect.left() + row_width, rect.top()), rect.max)
    } else {
        let row_height = (row_area / rect.width().max(1.0)).min(rect.height());
        Rect::from_min_max(egui::pos2(rect.left(), rect.top() + row_height), rect.max)
    }
}

fn gapped_aspect_ratio_for_area(area: f32, rect: Rect) -> f32 {
    if rect.width() <= 0.0 || rect.height() <= 0.0 || area <= 0.0 {
        return f32::INFINITY;
    }

    if rect.width() >= rect.height() {
        gapped_aspect_ratio((area / rect.height()).min(rect.width()), rect.height())
    } else {
        gapped_aspect_ratio(rect.width(), (area / rect.width()).min(rect.height()))
    }
}

fn worst_aspect_ratio<T>(row: &[AreaItem<T>], side: f32) -> f32 {
    let sum = row.iter().map(|item| item.area).sum::<f32>();
    if sum <= f32::EPSILON {
        return f32::INFINITY;
    }
    let min_area = row
        .iter()
        .map(|item| item.area)
        .fold(f32::INFINITY, f32::min)
        .max(f32::EPSILON);
    let max_area = row
        .iter()
        .map(|item| item.area)
        .fold(0.0_f32, f32::max)
        .max(f32::EPSILON);
    let side_squared = side * side;
    ((side_squared * max_area) / (sum * sum)).max((sum * sum) / (side_squared * min_area))
}

fn layout_row<T: Clone>(row: &[AreaItem<T>], rect: Rect, out: &mut Vec<Tile<T>>) -> Rect {
    if row.is_empty() || rect.width() < 1.0 || rect.height() < 1.0 {
        return rect;
    }

    let row_area = row.iter().map(|item| item.area).sum::<f32>();
    if row_area <= f32::EPSILON {
        return rect;
    }

    if vertical_row_score(row, rect) <= horizontal_row_score(row, rect) {
        layout_vertical_row(row, rect, out, row_area)
    } else {
        layout_horizontal_row(row, rect, out, row_area)
    }
}

fn layout_final_row<T: Clone>(row: &[AreaItem<T>], rect: Rect, out: &mut Vec<Tile<T>>) {
    if should_grid_final_row(row, rect) {
        layout_balanced_grid(row, rect, out);
    } else {
        layout_row(row, rect, out);
    }
}

fn should_grid_final_row<T>(row: &[AreaItem<T>], rect: Rect) -> bool {
    if row.len() < 2 || rect.width() < 1.0 || rect.height() < 1.0 {
        return false;
    }

    let row_score = best_row_score(row, rect);
    row_score > MAX_PAINTED_ROW_RATIO && best_grid_score(row.len(), rect) < row_score
}

fn layout_balanced_grid<T: Clone>(row: &[AreaItem<T>], rect: Rect, out: &mut Vec<Tile<T>>) {
    let (columns, rows) = best_grid_shape(row.len(), rect);
    let cell_height = rect.height() / rows as f32;
    let row_lengths = balanced_grid_row_lengths(row.len(), columns, rows);

    let mut index = 0;
    for (grid_row, row_len) in row_lengths.into_iter().enumerate() {
        let cell_width = rect.width() / row_len as f32;
        for column in 0..row_len {
            let item = &row[index];
            let min = egui::pos2(
                rect.left() + column as f32 * cell_width,
                rect.top() + grid_row as f32 * cell_height,
            );
            let max = egui::pos2(
                if column == row_len - 1 {
                    rect.right()
                } else {
                    min.x + cell_width
                },
                if grid_row == rows - 1 {
                    rect.bottom()
                } else {
                    min.y + cell_height
                },
            );
            out.push(Tile {
                item: item.item.clone(),
                size: item.size,
                rect: apply_gap(Rect::from_min_max(min, max)),
            });
            index += 1;
        }
    }
}

fn best_grid_shape(item_count: usize, rect: Rect) -> (usize, usize) {
    let mut best = (1, item_count);
    let mut best_score = f32::INFINITY;

    for columns in 1..=item_count {
        let rows = item_count.div_ceil(columns);
        let score = grid_score_for_shape(item_count, columns, rows, rect);
        if score < best_score {
            best = (columns, rows);
            best_score = score;
        }
    }

    best
}

fn best_grid_score(item_count: usize, rect: Rect) -> f32 {
    if item_count == 0 || rect.width() <= 0.0 || rect.height() <= 0.0 {
        return f32::INFINITY;
    }

    let (columns, rows) = best_grid_shape(item_count, rect);
    grid_score_for_shape(item_count, columns, rows, rect)
}

fn grid_score_for_shape(item_count: usize, columns: usize, rows: usize, rect: Rect) -> f32 {
    if item_count == 0 || columns == 0 || rows == 0 {
        return f32::INFINITY;
    }

    let cell_height = rect.height() / rows as f32;
    balanced_grid_row_lengths(item_count, columns, rows)
        .into_iter()
        .map(|row_len| gapped_aspect_ratio(rect.width() / row_len as f32, cell_height))
        .fold(1.0, f32::max)
}

fn balanced_grid_row_lengths(item_count: usize, columns: usize, rows: usize) -> Vec<usize> {
    let mut remaining = item_count;
    let mut lengths = Vec::with_capacity(rows);
    for remaining_rows in (1..=rows).rev() {
        let row_len = remaining.div_ceil(remaining_rows).min(columns);
        lengths.push(row_len);
        remaining = remaining.saturating_sub(row_len);
    }
    lengths
}

fn vertical_row_score<T>(row: &[AreaItem<T>], rect: Rect) -> f32 {
    let row_area = row.iter().map(|item| item.area).sum::<f32>();
    let width = (row_area / rect.height().max(1.0)).max(1.0);
    row.iter()
        .map(|item| {
            let height = item.area / width;
            gapped_aspect_ratio(width, height)
        })
        .fold(1.0, f32::max)
}

fn horizontal_row_score<T>(row: &[AreaItem<T>], rect: Rect) -> f32 {
    let row_area = row.iter().map(|item| item.area).sum::<f32>();
    let height = (row_area / rect.width().max(1.0)).max(1.0);
    row.iter()
        .map(|item| {
            let width = item.area / height;
            gapped_aspect_ratio(width, height)
        })
        .fold(1.0, f32::max)
}

fn layout_vertical_row<T: Clone>(
    row: &[AreaItem<T>],
    rect: Rect,
    out: &mut Vec<Tile<T>>,
    row_area: f32,
) -> Rect {
    let row_width = (row_area / rect.height().max(1.0)).min(rect.width());
    let mut top = rect.top();
    for (index, item) in row.iter().enumerate() {
        let height = if index == row.len() - 1 {
            rect.bottom() - top
        } else {
            (item.area / row_width.max(1.0)).min(rect.bottom() - top)
        };
        let tile_rect = Rect::from_min_max(
            egui::pos2(rect.left(), top),
            egui::pos2(rect.left() + row_width, top + height),
        );
        out.push(Tile {
            item: item.item.clone(),
            size: item.size,
            rect: apply_gap(tile_rect),
        });
        top = tile_rect.bottom();
    }
    Rect::from_min_max(egui::pos2(rect.left() + row_width, rect.top()), rect.max)
}

fn layout_horizontal_row<T: Clone>(
    row: &[AreaItem<T>],
    rect: Rect,
    out: &mut Vec<Tile<T>>,
    row_area: f32,
) -> Rect {
    let row_height = (row_area / rect.width().max(1.0)).min(rect.height());
    let mut left = rect.left();
    for (index, item) in row.iter().enumerate() {
        let width = if index == row.len() - 1 {
            rect.right() - left
        } else {
            (item.area / row_height.max(1.0)).min(rect.right() - left)
        };
        let tile_rect = Rect::from_min_max(
            egui::pos2(left, rect.top()),
            egui::pos2(left + width, rect.top() + row_height),
        );
        out.push(Tile {
            item: item.item.clone(),
            size: item.size,
            rect: apply_gap(tile_rect),
        });
        left = tile_rect.right();
    }
    Rect::from_min_max(egui::pos2(rect.left(), rect.top() + row_height), rect.max)
}

fn aspect_ratio(width: f32, height: f32) -> f32 {
    let width = width.max(1.0);
    let height = height.max(1.0);
    width.max(height) / width.min(height)
}

fn gapped_aspect_ratio(width: f32, height: f32) -> f32 {
    let gap = tile_gap(width, height);
    aspect_ratio(width - gap * 2.0, height - gap * 2.0)
}

fn apply_gap(rect: Rect) -> Rect {
    let gap = tile_gap(rect.width(), rect.height());
    rect.shrink(gap)
}

fn tile_gap(width: f32, height: f32) -> f32 {
    let base_gap = TILE_GAP.min(width / 8.0).min(height / 8.0);
    let ratio = aspect_ratio(width, height);
    if ratio > 1.75 {
        base_gap * 0.25
    } else {
        base_gap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN_VISIBLE_TILE_SIDE: f32 = 24.0;

    fn worst_visible_aspect_ratio<T>(tiles: &[Tile<T>], min_area: f32) -> f32 {
        tiles
            .iter()
            .filter(|tile| tile.rect.area() >= min_area)
            .map(|tile| aspect_ratio(tile.rect.width(), tile.rect.height()))
            .fold(1.0_f32, f32::max)
    }

    fn assert_visible_tiles_are_squareish(tiles: &[Tile<usize>], min_area: f32) {
        let worst_visible_ratio = worst_visible_aspect_ratio(tiles, min_area);
        assert!(
            worst_visible_ratio < MAX_PAINTED_ROW_RATIO,
            "visible tiles should avoid line-like strips, got {worst_visible_ratio}: {tiles:#?}"
        );
        assert!(
            tiles
                .iter()
                .filter(|tile| tile.rect.area() >= min_area)
                .all(|tile| tile.rect.width().min(tile.rect.height()) >= MIN_VISIBLE_TILE_SIDE),
            "visible tiles should not have hairline sides under {MIN_VISIBLE_TILE_SIDE}px: {tiles:#?}"
        );
    }

    fn generated_sizes(mut seed: u64, count: usize) -> Vec<u128> {
        let mut sizes = Vec::with_capacity(count);
        for index in 0..count {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let raw = ((seed >> 24) % 7_500 + 1) as u128;
            let curve = (count - index).max(1) as u128;
            sizes.push(raw * curve * curve);
        }
        sizes
    }

    fn items_from_sizes(sizes: &[u128]) -> Vec<TreemapItem<usize>> {
        sizes
            .iter()
            .copied()
            .enumerate()
            .map(|(item, size)| TreemapItem { item, size })
            .collect()
    }

    #[test]
    fn creates_non_overlapping_tiles_inside_parent() {
        let parent = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(500.0, 300.0));
        let items = vec![
            TreemapItem { item: 1, size: 50 },
            TreemapItem { item: 2, size: 30 },
            TreemapItem { item: 3, size: 20 },
        ];

        let tiles = layout(&items, parent);

        assert_eq!(tiles.len(), 3);
        for tile in &tiles {
            assert!(parent.contains(tile.rect.min));
            assert!(parent.contains(tile.rect.max));
        }
        for (idx, a) in tiles.iter().enumerate() {
            for b in tiles.iter().skip(idx + 1) {
                assert!(!a.rect.intersects(b.rect) || a.rect.intersect(b.rect).area() < 0.01);
            }
        }
    }

    #[test]
    fn ignores_zero_sized_items() {
        let parent = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(100.0, 100.0));
        let tiles = layout(
            &[
                TreemapItem { item: "a", size: 0 },
                TreemapItem {
                    item: "b",
                    size: 10,
                },
            ],
            parent,
        );

        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].item, "b");
    }

    #[test]
    fn skips_subpixel_remainders_without_panicking() {
        let parent = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(0.9, 200.0));
        let tiles = layout(
            &[
                TreemapItem { item: 1, size: 100 },
                TreemapItem { item: 2, size: 1 },
            ],
            parent,
        );

        assert!(tiles.is_empty());
    }

    #[test]
    fn caps_dominant_tile_at_half_of_parent() {
        let parent = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1000.0, 600.0));
        let tiles = layout(
            &[
                TreemapItem {
                    item: "large",
                    size: 900,
                },
                TreemapItem {
                    item: "medium",
                    size: 80,
                },
                TreemapItem {
                    item: "small-a",
                    size: 10,
                },
                TreemapItem {
                    item: "small-b",
                    size: 10,
                },
            ],
            parent,
        );

        let large = tiles
            .iter()
            .find(|tile| tile.item == "large")
            .expect("large tile");
        assert!(large.rect.area() <= parent.area() * 0.51);
        assert!(large.rect.area() >= parent.area() * 0.48);
    }

    #[test]
    fn single_child_fills_parent_except_gap() {
        let parent = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(100.0, 100.0));
        let tiles = layout(
            &[TreemapItem {
                item: "only",
                size: 1,
            }],
            parent,
        );

        assert_eq!(tiles.len(), 1);
        assert!(tiles[0].rect.area() > parent.area() * 0.85);
    }

    #[test]
    fn gap_does_not_turn_small_tiles_into_lines() {
        let raw = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(16.0, 64.0));
        let gapped = apply_gap(raw);

        assert!(gapped.width() >= raw.width() * 0.7);
        assert!(gapped.height() >= raw.height() * 0.9);
        assert!(aspect_ratio(gapped.width(), gapped.height()) < aspect_ratio(8.0, 62.0));
    }

    #[test]
    fn elongated_tiles_use_smaller_gap_to_preserve_shape() {
        let squareish = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(120.0, 100.0));
        let elongated = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(205.0, 100.0));

        let squareish_gap = squareish.width() - apply_gap(squareish).width();
        let elongated_gap = elongated.width() - apply_gap(elongated).width();

        assert!(squareish_gap > elongated_gap * 3.0);
        assert!(aspect_ratio(apply_gap(elongated).width(), apply_gap(elongated).height()) < 2.1);
    }

    #[test]
    fn many_similar_items_form_squarish_tiles() {
        let parent = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1000.0, 600.0));
        let items = (0..18)
            .map(|item| TreemapItem { item, size: 100 })
            .collect::<Vec<_>>();

        let tiles = layout(&items, parent);

        assert_eq!(tiles.len(), items.len());
        let worst_ratio = worst_visible_aspect_ratio(&tiles, 1.0);
        assert!(
            worst_ratio < 2.4,
            "similar-size tiles should avoid skinny strips, got {worst_ratio}"
        );
    }

    #[test]
    fn descending_items_avoid_extreme_vertical_slivers() {
        let parent = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(900.0, 600.0));
        let items = [400, 220, 160, 120, 90, 70, 55, 45, 35, 25]
            .into_iter()
            .enumerate()
            .map(|(item, size)| TreemapItem { item, size })
            .collect::<Vec<_>>();

        let tiles = layout(&items, parent);

        let worst_visible_ratio = worst_visible_aspect_ratio(&tiles, 3_000.0);
        assert!(
            worst_visible_ratio < MAX_PAINTED_ROW_RATIO,
            "visible tiles should stay reasonably square, got {worst_visible_ratio}"
        );
    }

    #[test]
    fn varied_real_world_distribution_stays_readable() {
        let parent = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1100.0, 680.0));
        let sizes = [
            144_000, 31_000, 12_100, 9_750, 5_900, 3_940, 2_210, 1_960, 977, 640, 512, 384, 256,
            196, 128, 96, 64, 48, 32, 24,
        ];
        let items = sizes
            .into_iter()
            .enumerate()
            .map(|(item, size)| TreemapItem { item, size })
            .collect::<Vec<_>>();

        let tiles = layout(&items, parent);
        let worst_visible_ratio = worst_visible_aspect_ratio(&tiles, 4_500.0);

        assert!(
            worst_visible_ratio < MAX_PAINTED_ROW_RATIO,
            "large visible tiles should avoid line-like strips, got {worst_visible_ratio}"
        );
    }

    #[test]
    fn tiny_tail_entries_get_readable_geometry() {
        let parent = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1180.0, 760.0));
        let sizes = [
            780_000, 245_000, 81_000, 19_000, 6_200, 2_900, 1_400, 640, 320, 160, 80, 40, 20,
        ];
        let items = sizes
            .into_iter()
            .enumerate()
            .map(|(item, size)| TreemapItem { item, size })
            .collect::<Vec<_>>();

        let tiles = layout(&items, parent);
        let visible_tiles = tiles
            .iter()
            .filter(|tile| tile.rect.area() >= 400.0)
            .collect::<Vec<_>>();

        assert_eq!(tiles.len(), items.len());
        assert!(visible_tiles.len() >= 12);
        assert!(
            visible_tiles
                .iter()
                .all(|tile| aspect_ratio(tile.rect.width(), tile.rect.height())
                    < MAX_PAINTED_ROW_RATIO),
            "readable tail tiles should not render as line-like strips: {visible_tiles:#?}"
        );
    }

    #[test]
    fn orientation_scoring_accounts_for_painted_gap() {
        let parent = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(960.0, 540.0));
        let sizes = [
            610_000, 233_000, 144_000, 89_000, 55_000, 34_000, 21_000, 13_000, 8_000, 5_000, 3_000,
            1_900, 1_200, 760, 480, 300, 190, 120,
        ];
        let items = sizes
            .into_iter()
            .enumerate()
            .map(|(item, size)| TreemapItem { item, size })
            .collect::<Vec<_>>();

        let tiles = layout(&items, parent);
        let worst_visible_ratio = worst_visible_aspect_ratio(&tiles, 360.0);

        assert!(
            worst_visible_ratio < MAX_PAINTED_ROW_RATIO,
            "painted gap should not turn visible tiles into strips, got {worst_visible_ratio}: {tiles:#?}"
        );
    }

    #[test]
    fn portrait_tail_remainder_is_folded_into_squareish_grid() {
        let parent = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(720.0, 960.0));
        let items = generated_sizes(17, 13)
            .into_iter()
            .enumerate()
            .map(|(item, size)| TreemapItem { item, size })
            .collect::<Vec<_>>();

        let tiles = layout(&items, parent);

        assert_eq!(tiles.len(), items.len());
        assert_visible_tiles_are_squareish(&tiles, parent.area() * 0.001);
    }

    #[test]
    fn generated_distributions_avoid_visible_strips() {
        let parents = [
            Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1200.0, 720.0)),
            Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(720.0, 960.0)),
            Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(820.0, 620.0)),
            Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1280.0, 1280.0)),
            Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1600.0, 520.0)),
            Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(520.0, 1600.0)),
            Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(420.0, 760.0)),
            Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1238.0, 1348.0)),
            Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2504.0, 1348.0)),
            Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(960.0, 540.0)),
            Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(540.0, 960.0)),
            Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(360.0, 640.0)),
            Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(640.0, 360.0)),
            Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(3440.0, 1440.0)),
            Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1440.0, 3440.0)),
        ];

        for (parent_index, parent) in parents.into_iter().enumerate() {
            for seed in 1..=384 {
                let count = 12 + seed as usize;
                let items = generated_sizes(seed * 17 + parent_index as u64, count)
                    .into_iter()
                    .enumerate()
                    .map(|(item, size)| TreemapItem { item, size })
                    .collect::<Vec<_>>();
                let tiles = layout(&items, parent);
                let visible_cutoff = parent.area() * 0.001;
                let worst_visible_ratio = worst_visible_aspect_ratio(&tiles, visible_cutoff);

                assert!(
                    worst_visible_ratio < MAX_PAINTED_ROW_RATIO,
                    "generated case parent={parent_index} seed={seed} produced strip ratio {worst_visible_ratio}: {tiles:#?}"
                );
            }
        }
    }

    #[test]
    fn wide_generated_layout_stays_below_squareish_limit() {
        let parent = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1200.0, 720.0));
        let count = 142;
        let items = generated_sizes(130 * 17, count)
            .into_iter()
            .enumerate()
            .map(|(item, size)| TreemapItem { item, size })
            .collect::<Vec<_>>();

        let tiles = layout(&items, parent);

        assert_visible_tiles_are_squareish(&tiles, parent.area() * 0.001);
    }

    #[test]
    fn small_portrait_generated_layout_stays_below_squareish_limit() {
        let parent = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(420.0, 760.0));
        let count = 14;
        let items = generated_sizes(40, count)
            .into_iter()
            .enumerate()
            .map(|(item, size)| TreemapItem { item, size })
            .collect::<Vec<_>>();

        let tiles = layout(&items, parent);

        assert_visible_tiles_are_squareish(&tiles, parent.area() * 0.001);
    }

    #[test]
    fn small_landscape_generated_layout_stays_below_squareish_limit() {
        let parent = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(640.0, 360.0));
        let count = 16;
        let items = generated_sizes(216, count)
            .into_iter()
            .enumerate()
            .map(|(item, size)| TreemapItem { item, size })
            .collect::<Vec<_>>();

        let tiles = layout(&items, parent);

        assert_visible_tiles_are_squareish(&tiles, parent.area() * 0.001);
    }

    #[test]
    fn pathological_disk_distributions_avoid_visible_strips() {
        let parents = [
            Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1180.0, 720.0)),
            Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(520.0, 1500.0)),
        ];
        let distributions = [
            vec![
                1_200_000, 60_000, 45_000, 33_000, 24_000, 18_000, 13_000, 9_000, 6_000, 4_000,
                2_000, 1_000, 500, 250, 125,
            ],
            (0..40)
                .map(|index| 1_000_000_u128 / 2_u128.pow((index / 4).min(12)))
                .collect::<Vec<_>>(),
            (0..64)
                .map(|index| {
                    let bucket = (index / 8 + 1) as u128;
                    480_000 / (bucket * bucket)
                })
                .collect::<Vec<_>>(),
        ];

        for parent in parents {
            for sizes in &distributions {
                let tiles = layout(&items_from_sizes(sizes), parent);
                assert_visible_tiles_are_squareish(&tiles, parent.area() * 0.001);
            }
        }
    }

    #[test]
    fn balanced_grid_tail_uses_full_row_width() {
        let parent = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(240.0, 240.0));
        let row = (0..3)
            .map(|item| AreaItem {
                item,
                size: 1,
                area: 1.0,
            })
            .collect::<Vec<_>>();
        let mut tiles = Vec::new();

        layout_balanced_grid(&row, parent, &mut tiles);

        assert_eq!(tiles.len(), 3);
        assert!(tiles[2].rect.width() > tiles[0].rect.width() * 1.8);
        assert_visible_tiles_are_squareish(&tiles, 1.0);
    }

    #[test]
    fn layout_consumes_row_remainders_without_visible_cracks() {
        let parent = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(977.0, 613.0));
        let items = [333, 222, 111, 88, 77, 66, 55, 44, 33, 22, 11]
            .into_iter()
            .enumerate()
            .map(|(item, size)| TreemapItem { item, size })
            .collect::<Vec<_>>();

        let tiles = layout(&items, parent);

        assert_eq!(tiles.len(), items.len());
        assert!(tiles.iter().all(|tile| tile.rect.width() > 0.0));
        assert!(tiles.iter().all(|tile| tile.rect.height() > 0.0));
        assert!(tiles.iter().all(|tile| parent.contains(tile.rect.min)));
        assert!(tiles.iter().all(|tile| parent.contains(tile.rect.max)));
    }
}
