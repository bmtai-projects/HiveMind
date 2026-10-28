//! Scroll math for side-panel lists whose items can be several rows tall.
//! Mirrors how ratatui's `List` lays items out, so a selection kept visible
//! here is never moved again when drawn.

/// One past the last item that fits entirely when drawing from `offset`.
pub(crate) fn visible_end(heights: &[usize], offset: usize, rows: usize) -> usize {
    let start = offset.min(heights.len());
    let mut used = 0;
    let mut end = start;
    while end < heights.len() && used + heights[end] <= rows {
        used += heights[end];
        end += 1;
    }
    // An item taller than the whole viewport still counts as shown.
    if end == start && start < heights.len() {
        start + 1
    } else {
        end
    }
}

/// The largest offset that still fills the viewport down to the last item.
pub(crate) fn max_offset(heights: &[usize], rows: usize) -> usize {
    let mut used = 0;
    let mut start = heights.len();
    while start > 0 && used + heights[start - 1] <= rows {
        used += heights[start - 1];
        start -= 1;
    }
    start.min(heights.len().saturating_sub(1))
}

/// The smallest move of `offset` that keeps `selected` fully in view.
pub(crate) fn offset_showing(
    heights: &[usize],
    offset: usize,
    selected: usize,
    rows: usize,
) -> usize {
    let Some(last) = heights.len().checked_sub(1) else {
        return 0;
    };
    let selected = selected.min(last);
    let mut offset = offset.min(max_offset(heights, rows));
    if selected < offset {
        return selected;
    }
    while offset < selected && selected >= visible_end(heights, offset, rows) {
        offset += 1;
    }
    offset
}

/// Mouse wheel: moves the view, dragging the selection along only if it would leave.
pub(crate) fn scroll(
    heights: &[usize],
    offset: usize,
    selected: usize,
    rows: usize,
    delta: isize,
) -> (usize, usize) {
    if heights.is_empty() {
        return (0, 0);
    }
    let max = max_offset(heights, rows);
    let offset = offset.min(max).saturating_add_signed(delta).min(max);
    let last_shown = visible_end(heights, offset, rows)
        .saturating_sub(1)
        .max(offset);
    (offset, selected.clamp(offset, last_shown))
}

/// The item under `row`, counted from the first visible row of the list.
pub(crate) fn index_at(heights: &[usize], offset: usize, row: usize) -> Option<usize> {
    let mut top = 0;
    for (index, height) in heights.iter().enumerate().skip(offset) {
        if row < top + height {
            return Some(index);
        }
        top += height;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARDS: [usize; 6] = [2, 2, 3, 2, 2, 2];

    #[test]
    fn visible_end_counts_only_items_that_fit_whole() {
        assert_eq!(
            visible_end(&CARDS, 0, 6),
            2,
            "2+2 fit, the 3-row card does not"
        );
        assert_eq!(
            visible_end(&CARDS, 2, 7),
            5,
            "3+2+2 fills seven rows exactly"
        );
        assert_eq!(visible_end(&[10], 0, 4), 1, "an oversized item still shows");
        assert_eq!(visible_end(&[], 0, 4), 0);
    }

    #[test]
    fn max_offset_keeps_the_last_item_at_the_bottom() {
        assert_eq!(max_offset(&CARDS, 6), 3);
        assert_eq!(max_offset(&CARDS, 100), 0, "everything fits");
        assert_eq!(max_offset(&[], 5), 0);
    }

    #[test]
    fn the_selection_is_scrolled_into_view_both_ways() {
        assert_eq!(offset_showing(&CARDS, 0, 5, 6), 3);
        assert_eq!(offset_showing(&CARDS, 3, 1, 6), 1);
        assert_eq!(
            offset_showing(&CARDS, 1, 2, 6),
            1,
            "already visible: unchanged"
        );
    }

    #[test]
    fn the_wheel_moves_the_view_and_drags_the_selection_only_when_needed() {
        assert_eq!(
            scroll(&CARDS, 0, 0, 6, 1),
            (1, 1),
            "selection pushed off the top follows"
        );
        assert_eq!(
            scroll(&CARDS, 0, 1, 6, 1),
            (1, 1),
            "still visible: stays put"
        );
        assert_eq!(
            scroll(&CARDS, 3, 5, 6, -2),
            (1, 2),
            "pulled up from the bottom"
        );
        assert_eq!(scroll(&CARDS, 0, 0, 6, 99).0, 3, "clamped to the end");
        assert_eq!(scroll(&[], 0, 0, 6, 1), (0, 0));
    }

    #[test]
    fn a_row_maps_to_the_item_drawn_there() {
        assert_eq!(index_at(&CARDS, 0, 0), Some(0));
        assert_eq!(index_at(&CARDS, 0, 3), Some(1));
        assert_eq!(index_at(&CARDS, 1, 2), Some(2));
        assert_eq!(index_at(&CARDS, 0, 99), None);
    }
}
