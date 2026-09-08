use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Context as _;
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;

use crate::config::{ClockPosition, Config};

/// Everything the render thread needs to put on screen. Text fields are final
/// Pango markup: escaping (and the countdown label wrapper) happened in the
/// state thread; nothing here inspects or transforms them.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderSpec {
    pub bg_argb: u32,
    pub text_markup: String,
    pub text_argb: u32,
    /// Pixel size for the main text, already shrunk to fit by the state
    /// thread. `textoverlay` silently drops layouts taller than the frame, so
    /// the size reaching it must always fit.
    pub text_px: u32,
    pub clock_text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SinkKind {
    Kms,
    Auto,
    Png,
}

/// Platform default when `--sink` is not given: the appliance's unit file
/// passes only `--config`, so Linux must default to kms; a dev machine gets
/// a window. This sink factory and the ntp probe in status.rs are
/// deliberately the only two `cfg(target_os)` sites — platform differences
/// stay contained here.
pub fn default_sink() -> SinkKind {
    #[cfg(target_os = "linux")]
    {
        SinkKind::Kms
    }
    #[cfg(not(target_os = "linux"))]
    {
        SinkKind::Auto
    }
}

/// The one place that knows which sink is in use.
fn make_sink(kind: SinkKind, png_path: Option<&Path>) -> anyhow::Result<gst::Element> {
    match kind {
        SinkKind::Kms => {
            #[cfg(target_os = "linux")]
            {
                gst::ElementFactory::make("kmssink")
                    .build()
                    .context("creating kmssink (is gstreamer1.0-plugins-bad installed?)")
            }
            #[cfg(not(target_os = "linux"))]
            {
                anyhow::bail!("--sink kms is only available on Linux")
            }
        }
        SinkKind::Auto => gst::ElementFactory::make("autovideosink")
            .build()
            .context("creating autovideosink"),
        SinkKind::Png => {
            let path = png_path.context("png sink needs an output path")?;
            let bin = gst::Bin::new();
            let pngenc = gst::ElementFactory::make("pngenc")
                .property("snapshot", true)
                .build()
                .context("creating pngenc")?;
            let filesink = gst::ElementFactory::make("filesink")
                .property("location", path.display().to_string())
                .build()
                .context("creating filesink")?;
            bin.add_many([&pngenc, &filesink])?;
            gst::Element::link_many([&pngenc, &filesink])?;
            let pad = pngenc.static_pad("sink").context("pngenc sink pad")?;
            bin.add_pad(&gst::GhostPad::with_target(&pad)?)?;
            Ok(bin.upcast())
        }
    }
}

pub struct Renderer {
    pipeline: gst::Pipeline,
    bg: gst::Element,
    text: gst::Element,
    clock: gst::Element,
    text_font_family: String,
}

impl Renderer {
    /// Build the fixed pipeline: a compositor with three pads — background
    /// colour, text on a transparent canvas, wall clock. Layered from day
    /// one so fades, extra regions or media later are pad additions, not a
    /// re-architecture.
    pub fn build(
        config: &Config,
        sink_kind: SinkKind,
        png_path: Option<&Path>,
    ) -> anyhow::Result<Renderer> {
        gst::init().context("initialising GStreamer")?;

        let d = &config.display;
        let pipeline = gst::Pipeline::new();

        let comp = gst::ElementFactory::make("compositor")
            .name("comp")
            .build()
            .context("creating compositor")?;
        // Transparent regions of upper pads must composite over the background
        // pad, not over garbage.
        comp.set_property_from_str("background", "black");

        let out_caps = gst::Caps::builder("video/x-raw")
            .field("width", d.width as i32)
            .field("height", d.height as i32)
            .field("framerate", gst::Fraction::new(d.fps as i32, 1))
            .build();
        let capsfilter = gst::ElementFactory::make("capsfilter")
            .property("caps", &out_caps)
            .build()?;
        let convert = gst::ElementFactory::make("videoconvert").build()?;
        let sink = make_sink(sink_kind, png_path)?;

        pipeline.add_many([&comp, &capsfilter, &convert, &sink])?;
        gst::Element::link_many([&comp, &capsfilter, &convert, &sink])?;

        let branch_caps = gst::Caps::builder("video/x-raw")
            .field("format", "BGRA")
            .field("width", d.width as i32)
            .field("height", d.height as i32)
            .field("framerate", gst::Fraction::new(d.fps as i32, 1))
            .build();

        // A solid-colour BGRA source; the snapshot sink is not live so the
        // pipeline can run as fast as one frame needs.
        let make_src = |name: &str, argb: u32| -> anyhow::Result<gst::Element> {
            let src = gst::ElementFactory::make("videotestsrc")
                .name(name)
                .property("is-live", sink_kind != SinkKind::Png)
                .build()?;
            src.set_property_from_str("pattern", "solid-color");
            src.set_property("foreground-color", argb);
            Ok(src)
        };

        // Every branch ends in an explicit capsfilter so each source is
        // forced to full-frame BGRA; without it a lone videotestsrc happily
        // negotiates its 320×240 default with the compositor.
        let link_branch = |elements: &[&gst::Element]| -> anyhow::Result<()> {
            let filter = gst::ElementFactory::make("capsfilter")
                .property("caps", &branch_caps)
                .build()?;
            pipeline.add_many(elements.iter().copied())?;
            pipeline.add(&filter)?;
            for pair in elements.windows(2) {
                pair[0].link(pair[1])?;
            }
            elements.last().context("empty branch")?.link(&filter)?;
            let comp_pad = comp
                .request_pad_simple("sink_%u")
                .context("compositor pad")?;
            let src_pad = filter.static_pad("src").context("capsfilter src pad")?;
            src_pad.link(&comp_pad)?;
            Ok(())
        };

        // Layer 0: background colour (opaque).
        let bg = make_src("bg", 0xff000000)?;
        link_branch(&[&bg])?;

        // Layer 1: main text on a fully transparent canvas — alpha must be
        // exactly 0x00000000 or this layer occludes the background.
        let canvas = make_src("canvas", 0x00000000)?;
        let text = gst::ElementFactory::make("textoverlay")
            .name("text")
            .property(
                "font-desc",
                format!("{} {}px", d.font_family(), d.max_font_px()),
            )
            // Sizing is computed in the state thread (RenderSpec.text_px);
            // auto-resize would rescale it again, and clean placard text wants
            // no outline or drop shadow.
            .property("auto-resize", false)
            .property("draw-outline", false)
            .property("draw-shadow", false)
            .property("xpad", d.padding_x as i32)
            .property("ypad", d.padding_y as i32)
            .build()
            .context("creating textoverlay (pango plugin)")?;
        // gstpango's wrap-mode enum nick is `wordchar` (no hyphen), unlike
        // the `word-char` spelling most GStreamer docs suggest.
        text.set_property_from_str("wrap-mode", "wordchar");
        text.set_property_from_str("line-alignment", "center");
        text.set_property_from_str("halignment", "center");
        text.set_property_from_str("valignment", "center");
        link_branch(&[&canvas, &text])?;

        // Layer 2: wall clock.
        let clockcanvas = make_src("clockcanvas", 0x00000000)?;
        let clock = gst::ElementFactory::make("textoverlay")
            .name("clock")
            .property("font-desc", &config.clock.font)
            // auto-resize rescales relative to a reference height; the clock's
            // point size comes from config and must be literal.
            .property("auto-resize", false)
            .property("draw-outline", false)
            .property("draw-shadow", false)
            .property("xpad", 48i32)
            .property("ypad", 32i32)
            .build()?;
        let (halign, valign) = match config.clock.position {
            ClockPosition::TopLeft => ("left", "top"),
            ClockPosition::TopRight => ("right", "top"),
            ClockPosition::BottomLeft => ("left", "bottom"),
            ClockPosition::BottomRight => ("right", "bottom"),
        };
        clock.set_property_from_str("halignment", halign);
        clock.set_property_from_str("valignment", valign);
        link_branch(&[&clockcanvas, &clock])?;

        Ok(Renderer {
            pipeline,
            bg,
            text,
            clock,
            text_font_family: d.font_family().to_string(),
        })
    }

    /// Property sets take effect on the next frame; every command is a
    /// one-frame cut. Called on the render thread only.
    pub fn apply(&self, spec: &RenderSpec) {
        self.bg.set_property("foreground-color", spec.bg_argb);
        self.text.set_property("text", &spec.text_markup);
        self.text.set_property("color", spec.text_argb);
        self.text.set_property(
            "font-desc",
            format!("{} {}px", self.text_font_family, spec.text_px),
        );
        self.clock.set_property("text", &spec.clock_text);
    }

    /// Run the GLib main loop until a bus error (Err) or EOS (Ok — only the
    /// png sink ever ends the stream). Applies incoming specs, feeds the
    /// systemd watchdog. This function IS the render thread; nothing else may
    /// touch element properties while it runs.
    pub fn run(self, spec_rx: mpsc::Receiver<RenderSpec>) -> anyhow::Result<()> {
        let main_loop = glib::MainLoop::new(None, false);
        let failure: Rc<RefCell<Option<anyhow::Error>>> = Rc::new(RefCell::new(None));

        let pipeline = self.pipeline.clone();
        let bus = pipeline.bus().context("pipeline has no bus")?;
        let bus_watch = {
            let main_loop = main_loop.clone();
            let failure = failure.clone();
            bus.add_watch_local(move |_, msg| {
                match msg.view() {
                    gst::MessageView::Error(err) => {
                        *failure.borrow_mut() = Some(anyhow::anyhow!(
                            "gstreamer error from {:?}: {} ({:?})",
                            err.src().map(|s| s.path_string()),
                            err.error(),
                            err.debug()
                        ));
                        main_loop.quit();
                    }
                    gst::MessageView::Eos(_) => main_loop.quit(),
                    _ => {}
                }
                glib::ControlFlow::Continue
            })?
        };

        pipeline
            .set_state(gst::State::Playing)
            .context("setting pipeline to Playing")?;

        // Drain the spec channel from the main loop so property sets stay on
        // this thread. 20 ms ≈ one frame at 50 fps.
        let apply_source = {
            let main_loop = main_loop.clone();
            let renderer = Rc::new(self);
            let renderer2 = renderer.clone();
            glib::timeout_add_local(Duration::from_millis(20), move || {
                loop {
                    match spec_rx.try_recv() {
                        Ok(spec) => renderer2.apply(&spec),
                        Err(mpsc::TryRecvError::Empty) => break glib::ControlFlow::Continue,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            // State thread is gone; die and let systemd restart us.
                            main_loop.quit();
                            break glib::ControlFlow::Break;
                        }
                    }
                }
            })
        };

        let _ = sd_notify::notify(&[sd_notify::NotifyState::Ready]);
        let heartbeat = glib::timeout_add_seconds_local(1, || {
            let _ = sd_notify::notify(&[sd_notify::NotifyState::Watchdog]);
            glib::ControlFlow::Continue
        });

        main_loop.run();

        heartbeat.remove();
        apply_source.remove();
        drop(bus_watch);
        let _ = pipeline.set_state(gst::State::Null);

        match failure.borrow_mut().take() {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}
