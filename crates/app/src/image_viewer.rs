//! Pure zoom/pan state of the image viewer in a file tab. No gpui here: `file_view` feeds it the
//! viewport size and the mouse/keyboard input and draws the image at `offset` with size `natural * scale`.

pub(crate) const MIN_SCALE: f32 = 0.05;
pub(crate) const MAX_SCALE: f32 = 32.;
/// Factor of one zoom-in / zoom-out step (buttons and keys).
pub(crate) const ZOOM_STEP: f32 = 1.25;
/// How much of the image must stay inside the viewport on each axis while panning.
const KEEP_VISIBLE: f32 = 64.;
/// Zoom per wheel pixel: a 120 px notch is about +20%.
const WHEEL_BASE: f32 = 1.0015;
/// A line of a wheel in pixels, for devices that report lines.
pub(crate) const WHEEL_LINE_PX: f32 = 40.;

/// Zoom factor for a wheel movement of `dy` pixels (positive = away from the user = zoom in).
pub(crate) fn wheel_factor(dy: f32) -> f32 {
    WHEEL_BASE.powf(dy.clamp(-600., 600.))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyAction {
    ZoomIn,
    ZoomOut,
    Fit,
    Actual,
}

/// The viewer's keyboard shortcuts: `+`/`=` in, `-` out, `0` fit, `1` 100%. Nothing with ctrl/alt/cmd.
pub(crate) fn key_action(key: &str, ctrl_alt_cmd: bool) -> Option<KeyAction> {
    if ctrl_alt_cmd {
        return None;
    }
    match key {
        "+" | "=" | "add" => Some(KeyAction::ZoomIn),
        "-" | "subtract" => Some(KeyAction::ZoomOut),
        "0" => Some(KeyAction::Fit),
        "1" => Some(KeyAction::Actual),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ImageViewer {
    natural: (f32, f32),
    viewport: (f32, f32),
    scale: f32,
    /// Top-left corner of the image relative to the viewport's top-left corner.
    offset: (f32, f32),
    fit: bool,
}

impl ImageViewer {
    pub(crate) fn new(width: u32, height: u32) -> Self {
        let mut viewer = Self {
            natural: (width.max(1) as f32, height.max(1) as f32),
            viewport: (0., 0.),
            scale: 1.,
            offset: (0., 0.),
            fit: true,
        };
        viewer.apply_fit();
        viewer
    }

    #[cfg(test)]
    pub(crate) fn scale(&self) -> f32 {
        self.scale
    }

    pub(crate) fn offset(&self) -> (f32, f32) {
        self.offset
    }

    pub(crate) fn is_fit(&self) -> bool {
        self.fit
    }

    pub(crate) fn viewport(&self) -> (f32, f32) {
        self.viewport
    }

    /// True once the viewport has a real size (nothing sensible can be drawn before that).
    pub(crate) fn has_viewport(&self) -> bool {
        self.viewport.0 > 0. && self.viewport.1 > 0.
    }

    /// Size of the image on screen.
    pub(crate) fn display_size(&self) -> (f32, f32) {
        (self.natural.0 * self.scale, self.natural.1 * self.scale)
    }

    /// The zoom as a whole percent for display.
    pub(crate) fn percent(&self) -> u32 {
        (self.scale * 100.).round() as u32
    }

    /// Scale that shows the whole image; never above 1 (a small image is not enlarged).
    fn fit_scale(&self) -> f32 {
        if !self.has_viewport() {
            return 1.;
        }
        (self.viewport.0 / self.natural.0).min(self.viewport.1 / self.natural.1).min(1.)
    }

    fn apply_fit(&mut self) {
        self.scale = self.fit_scale();
        self.offset = self.clamp_offset(self.offset);
    }

    /// One axis: centered when the image fits, otherwise at least `KEEP_VISIBLE` of it stays visible.
    fn clamp_axis(image: f32, viewport: f32, offset: f32) -> f32 {
        if image <= viewport {
            return (viewport - image) / 2.;
        }
        let keep = KEEP_VISIBLE.min(viewport);
        offset.clamp(keep - image, viewport - keep)
    }

    fn clamp_offset(&self, offset: (f32, f32)) -> (f32, f32) {
        let (w, h) = self.display_size();
        (Self::clamp_axis(w, self.viewport.0, offset.0), Self::clamp_axis(h, self.viewport.1, offset.1))
    }

    /// New viewport size; true when it changed. Fit mode refits, manual mode only re-clamps.
    pub(crate) fn set_viewport(&mut self, width: f32, height: f32) -> bool {
        let next = (width.max(0.), height.max(0.));
        if next == self.viewport {
            return false;
        }
        self.viewport = next;
        if self.fit {
            self.apply_fit();
        } else {
            self.offset = self.clamp_offset(self.offset);
        }
        true
    }

    /// Sets the scale keeping the image point under `cursor` (viewport coordinates) fixed.
    fn set_scale_at(&mut self, cursor: (f32, f32), scale: f32) {
        let scale = scale.clamp(MIN_SCALE, MAX_SCALE);
        let point = ((cursor.0 - self.offset.0) / self.scale, (cursor.1 - self.offset.1) / self.scale);
        self.scale = scale;
        self.fit = false;
        self.offset = self.clamp_offset((cursor.0 - point.0 * scale, cursor.1 - point.1 * scale));
    }

    pub(crate) fn zoom_at(&mut self, cursor: (f32, f32), factor: f32) {
        if factor.is_finite() && factor > 0. {
            self.set_scale_at(cursor, self.scale * factor);
        }
    }

    fn center(&self) -> (f32, f32) {
        (self.viewport.0 / 2., self.viewport.1 / 2.)
    }

    /// One step in (`true`) or out (`false`) around the middle of the viewport.
    pub(crate) fn zoom_step(&mut self, zoom_in: bool) {
        let factor = if zoom_in { ZOOM_STEP } else { 1. / ZOOM_STEP };
        self.zoom_at(self.center(), factor);
    }

    /// 100% around the middle of the current view.
    pub(crate) fn set_actual(&mut self) {
        self.set_scale_at(self.center(), 1.);
    }

    pub(crate) fn set_fit(&mut self) {
        self.fit = true;
        self.apply_fit();
    }

    /// Double click: fit becomes 100% around the cursor, anything else goes back to fit.
    pub(crate) fn toggle_at(&mut self, cursor: (f32, f32)) {
        if self.fit {
            self.set_scale_at(cursor, 1.);
        } else {
            self.set_fit();
        }
    }

    pub(crate) fn pan(&mut self, delta: (f32, f32)) {
        self.offset = self.clamp_offset((self.offset.0 + delta.0, self.offset.1 + delta.1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    fn viewer(w: u32, h: u32, vw: f32, vh: f32) -> ImageViewer {
        let mut v = ImageViewer::new(w, h);
        v.set_viewport(vw, vh);
        v
    }

    #[test]
    fn small_image_is_not_upscaled_and_is_centered() {
        let v = viewer(100, 50, 400., 300.);
        assert!(v.is_fit());
        assert_eq!(v.scale(), 1.);
        assert_eq!(v.offset(), (150., 125.));
        assert_eq!(v.percent(), 100);
    }

    #[test]
    fn large_image_is_downscaled_keeping_aspect() {
        let v = viewer(2000, 1000, 400., 400.);
        assert!(close(v.scale(), 0.2));
        let (w, h) = v.display_size();
        assert!(close(w, 400.) && close(h, 200.));
        assert!(close(v.offset().0, 0.) && close(v.offset().1, 100.));
        assert_eq!(v.percent(), 20);
        // The tall side limits instead.
        assert!(close(viewer(1000, 2000, 400., 400.).scale(), 0.2));
    }

    #[test]
    fn no_viewport_yet_means_scale_one() {
        let v = ImageViewer::new(300, 200);
        assert!(!v.has_viewport());
        assert_eq!(v.scale(), 1.);
    }

    #[test]
    fn zoom_at_keeps_the_point_under_the_cursor() {
        let mut v = viewer(4000, 3000, 800., 600.);
        v.zoom_at((0., 0.), 1.); // leaves fit
        v.set_actual();
        let cursor = (500., 300.);
        let before = ((cursor.0 - v.offset().0) / v.scale(), (cursor.1 - v.offset().1) / v.scale());
        v.zoom_at(cursor, 2.);
        assert!(!v.is_fit());
        assert!(close(v.scale(), 2.));
        let after = ((cursor.0 - v.offset().0) / v.scale(), (cursor.1 - v.offset().1) / v.scale());
        assert!(close(before.0, after.0) && close(before.1, after.1));
    }

    #[test]
    fn zoom_is_clamped() {
        let mut v = viewer(100, 100, 400., 400.);
        for _ in 0..40 {
            v.zoom_at((200., 200.), 2.);
        }
        assert_eq!(v.scale(), MAX_SCALE);
        for _ in 0..80 {
            v.zoom_at((200., 200.), 0.5);
        }
        assert_eq!(v.scale(), MIN_SCALE);
        // Garbage factors are ignored.
        v.zoom_at((0., 0.), f32::NAN);
        v.zoom_at((0., 0.), 0.);
        v.zoom_at((0., 0.), -1.);
        assert_eq!(v.scale(), MIN_SCALE);
    }

    #[test]
    fn pan_is_clamped_so_the_image_stays_reachable() {
        let mut v = viewer(2000, 2000, 400., 400.);
        v.set_actual();
        v.pan((100000., 100000.));
        assert_eq!(v.offset(), (400. - 64., 400. - 64.));
        v.pan((-100000., -100000.));
        assert_eq!(v.offset(), (64. - 2000., 64. - 2000.));
    }

    #[test]
    fn pan_keeps_a_small_image_centered() {
        let mut v = viewer(100, 100, 400., 400.);
        v.set_actual();
        v.pan((50., -70.));
        assert_eq!(v.offset(), (150., 150.));
        // Wide image, short viewport: only the long axis moves.
        let mut v = viewer(2000, 100, 400., 400.);
        v.set_actual();
        v.pan((-30., 80.));
        assert!(close(v.offset().0, -830.));
        assert_eq!(v.offset().1, 150.);
    }

    #[test]
    fn tiny_viewport_never_inverts_the_clamp() {
        let mut v = viewer(1000, 1000, 30., 30.);
        v.set_actual();
        v.pan((5000., -5000.));
        assert_eq!(v.offset(), (0., 30. - 1000.));
    }

    #[test]
    fn viewport_resize_refits_only_in_fit_mode() {
        let mut v = viewer(2000, 1000, 400., 400.);
        assert!(v.set_viewport(800., 400.));
        assert!(close(v.scale(), 0.4));
        assert!(!v.set_viewport(800., 400.));
        v.zoom_step(true);
        let scale = v.scale();
        assert!(v.set_viewport(1000., 700.));
        assert!(!v.is_fit());
        assert_eq!(v.scale(), scale);
        // Manual mode re-clamps the offset into the new viewport.
        let (w, _) = v.display_size();
        assert!(v.offset().0 >= 64f32.min(1000.) - w);
    }

    #[test]
    fn actual_size_and_fit_round_trip() {
        let mut v = viewer(2000, 1000, 400., 400.);
        v.set_actual();
        assert_eq!(v.scale(), 1.);
        assert!(!v.is_fit());
        // 100% is centered on the middle of the view: the image center stays there.
        assert!(close(v.offset().0 + 1000., 200.) && close(v.offset().1 + 500., 200.));
        v.set_fit();
        assert!(v.is_fit());
        assert!(close(v.scale(), 0.2));
    }

    #[test]
    fn zoom_step_goes_around_the_center() {
        let mut v = viewer(2000, 2000, 400., 400.);
        v.set_actual();
        let before = v.offset();
        v.zoom_step(true);
        assert!(close(v.scale(), 1.25));
        v.zoom_step(false);
        assert!(close(v.scale(), 1.));
        assert!(close(v.offset().0, before.0) && close(v.offset().1, before.1));
    }

    #[test]
    fn double_click_toggles_between_fit_and_actual() {
        let mut v = viewer(2000, 1000, 400., 400.);
        v.toggle_at((100., 150.));
        assert!(!v.is_fit());
        assert_eq!(v.scale(), 1.);
        v.toggle_at((100., 150.));
        assert!(v.is_fit());
    }

    #[test]
    fn wheel_factor_is_smooth_and_symmetric() {
        assert_eq!(wheel_factor(0.), 1.);
        assert!(wheel_factor(120.) > 1.15 && wheel_factor(120.) < 1.25);
        assert!(close(wheel_factor(50.) * wheel_factor(-50.), 1.));
        assert!(wheel_factor(-120.) < 1.);
        assert!(wheel_factor(1e9).is_finite());
    }

    #[test]
    fn keys_map_to_actions() {
        assert_eq!(key_action("+", false), Some(KeyAction::ZoomIn));
        assert_eq!(key_action("=", false), Some(KeyAction::ZoomIn));
        assert_eq!(key_action("-", false), Some(KeyAction::ZoomOut));
        assert_eq!(key_action("0", false), Some(KeyAction::Fit));
        assert_eq!(key_action("1", false), Some(KeyAction::Actual));
        assert_eq!(key_action("a", false), None);
        assert_eq!(key_action("1", true), None);
    }

    #[test]
    fn zero_sized_image_does_not_divide_by_zero() {
        let v = viewer(0, 0, 100., 100.);
        assert!(v.scale().is_finite() && v.scale() > 0.);
    }
}
