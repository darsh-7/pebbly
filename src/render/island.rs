//! The in-game island's motion: the taskbar pill's own painter in the island's window at the top
//! centre of the fullscreen monitor, coming down from the screen's top edge on the pill's spring
//! and going back up.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use windows::core::{h, Result};
use windows::Foundation::TypedEventHandler;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
use windows::UI::Composition::{CompositionBatchTypes, ContainerVisual};
use windows_numerics::Vector2;

use super::gfx::Gfx;
use super::layout::{RectPx, Scale};
use super::motion::{scaled_ticks, spring_samples, SPRING_DURATION_S};
use super::pill::{PillContent, PillVisual};
use crate::platform::windows::IslandWindow;

/// The island's gap below the screen's top edge.
const TOP_DIP: f32 = 14.0;
/// Going back up: quicker than coming down, as a moment ending.
const OUT_MS: f32 = 240.0;
const KEY_FRAMES: usize = 32;

pub struct Island {
    window: IslandWindow,
    stage: ContainerVisual,
    pill: PillVisual,
    /// What it shows (`None`: up and away), and the window's rect.
    shown: Option<PillContent>,
    rect: Option<RectPx>,
    /// Bumped by every show: a hide that finishes after a new show leaves the window alone.
    generation: Arc<AtomicU32>,
}

impl Island {
    pub fn new(g: &Gfx) -> Result<Self> {
        let window = IslandWindow::create(g)?;
        let stage = g.compositor.CreateContainerVisual()?;
        stage.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;
        window.root.Children()?.InsertAtTop(&stage)?;
        let pill = PillVisual::new(g, &stage)?;
        Ok(Island { window, stage, pill, shown: None, rect: None, generation: Arc::new(AtomicU32::new(0)) })
    }

    pub fn is_shown(&self) -> bool {
        self.shown.is_some()
    }

    /// The window and where it is (the islandsheet dev tool photographs it).
    pub fn window(&self) -> (HWND, Option<RectPx>) {
        (self.window.hwnd, self.rect)
    }

    /// Springs the stage's top to `y` (px) from wherever it is.
    fn spring_to(&self, y: f32) -> Result<()> {
        let c = self.stage.Compositor()?;
        let anim = c.CreateScalarKeyFrameAnimation()?;
        let linear = c.CreateLinearEasingFunction()?;
        for (progress, k) in spring_samples(KEY_FRAMES) {
            let expr = format!("this.StartingValue+({y}-this.StartingValue)*{k}");
            anim.InsertExpressionKeyFrameWithEasingFunction(progress, &windows::core::HSTRING::from(expr), &linear)?;
        }
        anim.SetDuration(windows::Foundation::TimeSpan { Duration: scaled_ticks(SPRING_DURATION_S * 1000.0) })?;
        self.stage.StartAnimation(h!("Offset.Y"), &anim)
    }

    fn fade_to(&self, to: f32, ms: f32) -> Result<()> {
        let c = self.stage.Compositor()?;
        let anim = c.CreateScalarKeyFrameAnimation()?;
        anim.InsertKeyFrame(1.0, to)?;
        anim.SetDuration(windows::Foundation::TimeSpan { Duration: scaled_ticks(ms) })?;
        self.stage.StartAnimation(h!("Opacity"), &anim)
    }

    /// Shows `front` at the top centre of `monitor`: the first time it comes down from the edge;
    /// after that a change of content is the pill's own (width spring, cross-fade).
    pub fn show(&mut self, g: &Gfx, front: &PillContent, s: &Scale, monitor: RectPx) -> Result<()> {
        let fade = self.shown.as_ref().is_some_and(|f| !f.same_kind(front));
        let l = self.pill.show(g, front, &[], super::pill::Extras::default(), s, fade, false)?;
        let top = s.px(TOP_DIP) as i32;
        // From the screen's edge down to the pill's bottom: the pill comes into it from above.
        let r = RectPx { x: monitor.x + (monitor.w - l.frame_w) / 2, y: monitor.y, w: l.frame_w, h: top + l.pill_h };
        if self.shown.is_none() {
            self.generation.fetch_add(1, Ordering::AcqRel);
            self.stage.StopAnimation(h!("Offset.Y"))?;
            self.stage.SetOffset(windows_numerics::Vector3 { X: 0.0, Y: -(l.pill_h as f32), Z: 0.0 })?;
            self.stage.SetOpacity(0.0)?;
            self.window.show_at(r)?;
            self.spring_to(top as f32)?;
            self.fade_to(1.0, 160.0)?;
        } else if self.rect != Some(r) {
            self.window.show_at(r)?;
        }
        self.rect = Some(r);
        self.shown = Some(front.clone());
        Ok(())
    }

    /// Back up past the edge, then the window hides (unless shown again meanwhile).
    pub fn hide(&mut self) -> Result<()> {
        if self.shown.take().is_none() {
            return Ok(());
        }
        let c = self.stage.Compositor()?;
        let h = self.rect.map_or(0, |r| r.h) as f32;
        let batch = c.CreateScopedBatch(CompositionBatchTypes::Animation)?;
        let up = c.CreateScalarKeyFrameAnimation()?;
        up.InsertKeyFrame(1.0, -h)?;
        up.SetDuration(windows::Foundation::TimeSpan { Duration: scaled_ticks(OUT_MS) })?;
        self.stage.StartAnimation(h!("Offset.Y"), &up)?;
        self.fade_to(0.0, OUT_MS)?;
        batch.End()?;
        let (hwnd, generation, at) = (self.window.hwnd.0 as isize, self.generation.clone(), self.generation.load(Ordering::Acquire));
        batch.Completed(&TypedEventHandler::new(move |_, _| {
            if generation.load(Ordering::Acquire) == at {
                unsafe {
                    let _ = ShowWindow(HWND(hwnd as *mut _), SW_HIDE);
                }
            }
            Ok(())
        }))?;
        Ok(())
    }
}
