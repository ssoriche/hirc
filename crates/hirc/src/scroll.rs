//! Scrolling the message view on a line grid. Every message row is a whole
//! number of text lines tall and the viewport is a whole number of lines
//! (see `views::viewport`), so a scroll offset that is a multiple of the
//! line height shows only whole rows. Keys and the wheel both move by
//! whole lines; reaching the bottom resumes following new lines.

use gpui::{ListOffset, ListState, point, px};

/// Where the view should rest.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pos {
    /// Pinned to the newest line, following as lines arrive.
    Tail,
    /// This many px from the top of the content.
    Offset(f32),
}

/// Lines (rounded off float error) in `px`.
fn lines_in(px: f32, line: f32) -> f32 {
    (px / line * 100.).round() / 100.
}

/// Move `lines` (negative is up) from `cur` (`None` while following the
/// tail, which is the same place as `max`), on the line grid. A position
/// that was off the grid snaps in the direction of travel, so a move always
/// shows at least one new line; the bottom resumes following.
pub fn target(cur: Option<f32>, max: f32, lines: i32, line: f32) -> Pos {
    let cur = cur.unwrap_or(max).clamp(0., max);
    let at = lines_in(cur, line);
    let to = if lines < 0 { at.ceil() + lines as f32 } else { at.floor() + lines as f32 };
    let to = (to * line).clamp(0., max);
    if to >= max { Pos::Tail } else { Pos::Offset(to) }
}

/// Whole lines that fit in `height`, and the px left over.
pub fn whole_lines(height: f32, line: f32) -> (usize, f32) {
    if height <= 0. || line <= 0. {
        return (0, height.max(0.));
    }
    let n = (height / line + 1e-3).floor();
    (n as usize, (height - n * line).max(0.))
}

/// Whole rows from offset `cur` down to `max` (the bottom).
pub fn rows_between(cur: f32, max: f32, line: f32) -> usize {
    if line <= 0. { 0 } else { lines_in((max - cur).max(0.), line).round() as usize }
}

/// Feed a wheel movement of `px` (positive moves the content down, so the
/// view goes up) into `acc`, and take out the whole lines it adds up to
/// (negative is up).
pub fn wheel_lines(acc: &mut f32, px: f32, line: f32) -> i32 {
    *acc -= px;
    let n = (*acc / line).trunc();
    *acc -= n * line;
    n as i32
}

/// Is the list pinned to its newest line?
pub fn following(list: &ListState) -> bool {
    list.is_scrolled_to_end() != Some(false)
}

/// Scroll `list` by `lines` on the `line` grid.
pub fn by_lines(list: &ListState, lines: i32, line: f32) {
    if lines == 0 {
        return;
    }
    let max = f32::from(list.max_offset_for_scrollbar().y);
    let cur = (!following(list)).then(|| -f32::from(list.scroll_px_offset_for_scrollbar().y));
    set(list, target(cur, max, lines, line));
}

pub fn set(list: &ListState, pos: Pos) {
    match pos {
        Pos::Tail => list.scroll_to(ListOffset { item_ix: list.item_count(), offset_in_item: px(0.) }),
        Pos::Offset(y) => list.set_offset_from_scrollbar(point(px(0.), px(-y))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: f32 = 21.;

    #[test]
    fn up_from_the_tail_moves_one_line() {
        // The regression: following the tail used to read as offset 0, so
        // the first `k` jumped to the top.
        assert_eq!(target(None, 30. * L, -1, L), Pos::Offset(29. * L));
        assert_eq!(target(None, 30. * L, -3, L), Pos::Offset(27. * L));
        // Half a page (of 34 lines) from the tail.
        assert_eq!(target(None, 30. * L, -17, L), Pos::Offset(13. * L));
    }

    #[test]
    fn moves_are_whole_lines_and_clamped() {
        assert_eq!(target(Some(10. * L), 30. * L, -1, L), Pos::Offset(9. * L));
        assert_eq!(target(Some(10. * L), 30. * L, 2, L), Pos::Offset(12. * L));
        assert_eq!(target(Some(L), 30. * L, -5, L), Pos::Offset(0.));
        assert_eq!(target(Some(0.), 30. * L, -1, L), Pos::Offset(0.));
    }

    #[test]
    fn reaching_the_bottom_follows_again() {
        assert_eq!(target(Some(29. * L), 30. * L, 1, L), Pos::Tail);
        assert_eq!(target(Some(20. * L), 30. * L, 50, L), Pos::Tail);
        // Nothing to scroll: always the tail.
        assert_eq!(target(None, 0., -1, L), Pos::Tail);
    }

    #[test]
    fn off_grid_positions_snap_in_the_direction_of_travel() {
        // Half a line down from line 10: up shows line 10's top, then 9.
        assert_eq!(target(Some(10.5 * L), 30. * L, -1, L), Pos::Offset(10. * L));
        assert_eq!(target(Some(10.5 * L), 30. * L, 1, L), Pos::Offset(11. * L));
        // Float noise is not a half line.
        assert_eq!(target(Some(10. * L + 0.001), 30. * L, -1, L), Pos::Offset(9. * L));
    }

    #[test]
    fn viewport_in_whole_lines() {
        assert_eq!(whole_lines(720.5, L), (34, 720.5 - 34. * L));
        assert_eq!(whole_lines(42., L), (2, 0.));
        assert_eq!(whole_lines(-3., L), (0, 0.));
    }

    #[test]
    fn rows_below_count_grid_rows() {
        assert_eq!(rows_between(27. * L, 30. * L, L), 3);
        assert_eq!(rows_between(30. * L, 30. * L, L), 0);
        assert_eq!(rows_between(27. * L + 0.01, 30. * L, L), 3);
        assert_eq!(rows_between(31. * L, 30. * L, L), 0);
    }

    #[test]
    fn wheel_adds_up_to_whole_lines() {
        let mut acc = 0.;
        assert_eq!(wheel_lines(&mut acc, 10., L), 0);
        assert_eq!(wheel_lines(&mut acc, 15., L), -1);
        assert!((acc + 4.).abs() < 1e-4);
        assert_eq!(wheel_lines(&mut acc, -300., L), 14);
    }
}
