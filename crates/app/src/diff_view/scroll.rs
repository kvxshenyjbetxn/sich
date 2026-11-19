//! Horizontal scroll geometry in logical pixels.

use std::ops::Range;

const MIN_THUMB: f32 = 18.;
const OVERSCAN_COLUMNS: usize = 12;

#[derive(Clone, Copy, Debug)]
pub(super) struct HorizontalGeometry {
    pub content: f32,
    pub viewport: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Thumb {
    pub start: f32,
    pub width: f32,
}

#[derive(Clone, Debug)]
pub(super) struct CodeWindow {
    pub columns: Range<usize>,
    pub left: f32,
}

impl HorizontalGeometry {
    fn limit(self) -> f32 {
        if self.viewport > 0. {
            (self.content - self.viewport).max(0.)
        } else {
            0.
        }
    }

    pub fn clamp(self, offset: f32) -> f32 {
        offset.max(0.).min(self.limit())
    }

    pub fn thumb(self, offset: f32) -> Option<Thumb> {
        let limit = self.limit();
        if limit == 0. {
            return None;
        }
        let width =
            (self.viewport * (self.viewport / self.content)).max(MIN_THUMB.min(self.viewport / 2.));
        let start = self.clamp(offset) / limit * (self.viewport - width);
        Some(Thumb { start, width })
    }

    pub fn offset_for_thumb(self, start: f32) -> f32 {
        let Some(thumb) = self.thumb(0.) else {
            return 0.;
        };
        self.clamp(start / (self.viewport - thumb.width) * self.limit())
    }

    pub fn code_window(self, offset: f32, advance: f32) -> CodeWindow {
        let offset = self.clamp(offset);
        let start = ((offset / advance).floor() as usize).saturating_sub(OVERSCAN_COLUMNS);
        let end = ((offset + self.viewport) / advance).ceil() as usize + OVERSCAN_COLUMNS;
        CodeWindow {
            columns: start..end,
            left: start as f32 * advance - offset,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct ScrollDrag {
    pub track_left: f32,
    pub grab: f32,
    pub geometry: HorizontalGeometry,
}

impl ScrollDrag {
    pub fn begin(
        geometry: HorizontalGeometry,
        track_left: f32,
        pointer: f32,
        offset: f32,
    ) -> Option<Self> {
        let thumb = geometry.thumb(offset)?;
        let at = pointer - track_left;
        let grab = if (thumb.start..=thumb.start + thumb.width).contains(&at) {
            at - thumb.start
        } else {
            thumb.width / 2.
        };
        Some(Self {
            track_left,
            grab,
            geometry,
        })
    }

    pub fn offset(self, pointer: f32) -> f32 {
        self.geometry
            .offset_for_thumb(pointer - self.track_left - self.grab)
    }
}

/// GPUI deltas describe the movement of the content, opposite to the offset.
pub(super) fn wheel_offset(x: f32, y: f32, shift: bool) -> f32 {
    if x != 0. {
        -x
    } else if shift {
        -y
    } else {
        0.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_clamp_at_both_edges_and_when_content_fits() {
        let area = HorizontalGeometry {
            content: 1000.,
            viewport: 200.,
        };
        assert_eq!(area.clamp(-20.), 0.);
        assert_eq!(area.clamp(40.), 40.);
        assert_eq!(area.clamp(900.), 800.);
        for viewport in [0., 1000., 1200.] {
            let area = HorizontalGeometry { viewport, ..area };
            assert_eq!(area.clamp(90.), 0.);
            assert_eq!(area.thumb(90.), None);
        }
    }

    #[test]
    fn thumb_size_and_position_follow_the_content_ratio() {
        let area = HorizontalGeometry {
            content: 1000.,
            viewport: 200.,
        };
        assert_eq!(
            area.thumb(0.),
            Some(Thumb {
                start: 0.,
                width: 40.
            })
        );
        assert_eq!(
            area.thumb(400.),
            Some(Thumb {
                start: 80.,
                width: 40.
            })
        );
        assert_eq!(
            area.thumb(800.),
            Some(Thumb {
                start: 160.,
                width: 40.
            })
        );
        assert_eq!(area.offset_for_thumb(80.), 400.);
        assert_eq!(area.offset_for_thumb(-5.), 0.);
        assert_eq!(area.offset_for_thumb(300.), 800.);

        let very_long = HorizontalGeometry {
            content: 1_000_000.,
            viewport: 100.,
        };
        assert_eq!(very_long.thumb(0.).unwrap().width, MIN_THUMB);
        assert_eq!(very_long.offset_for_thumb(82.), 999_900.);
    }

    #[test]
    fn drag_keeps_the_grab_point_and_clicking_the_track_centers_the_thumb() {
        let area = HorizontalGeometry {
            content: 1000.,
            viewport: 200.,
        };
        let drag = ScrollDrag::begin(area, 500., 590., 400.).unwrap();
        assert_eq!(drag.offset(590.), 400.);
        assert_eq!(drag.offset(630.), 600.);
        assert_eq!(drag.offset(800.), 800.);
        let track = ScrollDrag::begin(area, 500., 600., 0.).unwrap();
        assert_eq!(track.offset(600.), 400.);
    }

    #[test]
    fn wheel_uses_native_horizontal_input_or_shift_vertical_input() {
        assert_eq!(wheel_offset(-15., 8., false), 15.);
        assert_eq!(wheel_offset(6., -14., true), -6.);
        assert_eq!(wheel_offset(0., -14., true), 14.);
        assert_eq!(wheel_offset(0., 14., true), -14.);
        assert_eq!(wheel_offset(0., -14., false), 0.);
    }

    #[test]
    fn code_window_includes_the_end_of_extremely_long_lines() {
        let area = HorizontalGeometry {
            content: 80_000.,
            viewport: 80.,
        };
        let cut = area.code_window(79_920., 8.);
        assert_eq!(cut.columns, 9978..10012);
        assert_eq!(cut.left, -96.);
    }
}
