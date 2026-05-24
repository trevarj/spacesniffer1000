use egui::Rect;

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
        })
        .collect::<Vec<_>>();

    if weighted.is_empty() {
        return Vec::new();
    }

    weighted.sort_by_key(|item| std::cmp::Reverse(item.size));
    let mut out = Vec::with_capacity(weighted.len());
    layout_capped_binary(&weighted, rect, &mut out);
    out
}

#[derive(Debug, Clone)]
struct WeightedItem<T> {
    item: T,
    size: u128,
}

fn layout_capped_binary<T: Clone>(items: &[WeightedItem<T>], rect: Rect, out: &mut Vec<Tile<T>>) {
    if rect.width() < 1.0 || rect.height() < 1.0 {
        return;
    }

    let Some((first, rest)) = items.split_first() else {
        return;
    };

    if rest.is_empty() {
        out.push(Tile {
            item: first.item.clone(),
            size: first.size,
            rect: apply_gap(rect),
        });
        return;
    }

    let total_size: u128 = items.iter().map(|item| item.size).sum();
    if total_size == 0 {
        return;
    }
    let true_share = first.size as f64 / total_size as f64;
    let first_share = (true_share as f32).clamp(0.0, MAX_DOMINANT_SHARE);

    if rect.width() >= rect.height() {
        let first_width = (rect.width() * first_share).clamp(0.0, rect.width());
        let first_rect = Rect::from_min_max(
            rect.min,
            egui::pos2((rect.left() + first_width).min(rect.right()), rect.bottom()),
        );
        let rest_rect = Rect::from_min_max(egui::pos2(first_rect.right(), rect.top()), rect.max);
        out.push(Tile {
            item: first.item.clone(),
            size: first.size,
            rect: apply_gap(first_rect),
        });
        layout_capped_binary(rest, rest_rect, out);
    } else {
        let first_height = (rect.height() * first_share).clamp(0.0, rect.height());
        let first_rect = Rect::from_min_max(
            rect.min,
            egui::pos2(rect.right(), (rect.top() + first_height).min(rect.bottom())),
        );
        let rest_rect = Rect::from_min_max(egui::pos2(rect.left(), first_rect.bottom()), rect.max);
        out.push(Tile {
            item: first.item.clone(),
            size: first.size,
            rect: apply_gap(first_rect),
        });
        layout_capped_binary(rest, rest_rect, out);
    }
}

fn apply_gap(rect: Rect) -> Rect {
    let gap = TILE_GAP.min(rect.width() / 4.0).min(rect.height() / 4.0);
    rect.shrink(gap)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
