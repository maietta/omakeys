//! One-shot screenshot capture via wlr-screencopy-unstable-v1 -- a **separate** raw
//! `wayland-client` connection from GTK's (same pattern as `pointer.rs`). Used by the
//! vision-based hint fallback ([crate::vision]) to find candidate regions in apps AT-SPI
//! can't see into at all (GTK4's position bug, Electron apps without a11y force-enabled --
//! see atspi_scan.rs's module docs).

use std::collections::HashMap;
use std::os::fd::AsFd;

use anyhow::{anyhow, Context, Result};
use rustix::fs::{ftruncate, memfd_create, MemfdFlags};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

/// A captured frame, already converted to 8-bit grayscale for edge detection.
pub struct Capture {
    pub width: u32,
    pub height: u32,
    pub gray: Vec<u8>,
}

#[derive(Default)]
struct State {
    outputs_by_name: HashMap<String, wl_output::WlOutput>,
    buffer_info: Option<(wl_shm::Format, u32, u32, u32)>,
    /// `Some(true)` once `ready` arrives, `Some(false)` once `failed` arrives.
    done: Option<bool>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.outputs_by_name.insert(name, proxy.clone());
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for State {
    fn event(_: &mut Self, _: &wl_shm::WlShm, _: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(_: &mut Self, _: &wl_buffer::WlBuffer, _: wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrScreencopyManagerV1,
        _: wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer { format, width, height, stride } => {
                if let WEnum::Value(format) = format {
                    state.buffer_info = Some((format, width, height, stride));
                }
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => state.done = Some(true),
            zwlr_screencopy_frame_v1::Event::Failed => state.done = Some(false),
            _ => {}
        }
    }
}

/// Capture the given output (by connector name, e.g. "DP-3") and return it as grayscale
/// pixel data. Only the two common opaque/alpha 32-bit shm formats are supported
/// (`Argb8888`/`Xrgb8888`, which is what wlroots compositors advertise in practice).
pub fn capture_output_gray(output_name: &str) -> Result<Capture> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let mut state = State::default();

    for global in globals.contents().clone_list() {
        if global.interface == "wl_output" {
            let _: wl_output::WlOutput = globals.registry().bind(global.name, global.version.min(4), &qh, ());
        }
    }
    let shm: wl_shm::WlShm =
        globals.bind(&qh, 1..=1, ()).map_err(|e| anyhow!("compositor has no wl_shm: {e}"))?;
    queue.roundtrip(&mut state)?;

    let output = state
        .outputs_by_name
        .get(output_name)
        .with_context(|| format!("output {output_name:?} not found"))?
        .clone();

    let manager: ZwlrScreencopyManagerV1 = globals.bind(&qh, 1..=1, ()).map_err(|e| {
        anyhow!("compositor does not support zwlr_screencopy_manager_v1 (needed to screenshot for hints): {e}")
    })?;

    let frame = manager.capture_output(0, &output, &qh, ());

    while state.buffer_info.is_none() {
        queue.blocking_dispatch(&mut state)?;
    }
    let (format, width, height, stride) =
        state.buffer_info.context("compositor never sent buffer info")?;
    if !matches!(format, wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888) {
        return Err(anyhow!("unsupported shm buffer format: {format:?}"));
    }

    let size = (stride * height) as usize;
    let fd = memfd_create("omg-keys-screencap", MemfdFlags::CLOEXEC)?;
    ftruncate(&fd, size as u64)?;
    let file = std::fs::File::from(fd);
    let mmap = unsafe { memmap2::MmapMut::map_mut(&file)? };

    let pool = shm.create_pool(file.as_fd(), size as i32, &qh, ());
    let buffer = pool.create_buffer(0, width as i32, height as i32, stride as i32, format, &qh, ());
    frame.copy(&buffer);

    while state.done.is_none() {
        queue.blocking_dispatch(&mut state)?;
    }
    pool.destroy();
    buffer.destroy();
    frame.destroy();
    if state.done != Some(true) {
        return Err(anyhow!("compositor failed to copy the frame"));
    }

    // BGRA/BGRX byte order in memory (little-endian Argb8888/Xrgb8888) -> luminance.
    let mut gray = Vec::with_capacity((width * height) as usize);
    for row in 0..height {
        let row_start = (row * stride) as usize;
        for col in 0..width {
            let px = row_start + col as usize * 4;
            let (b, g, r) = (mmap[px] as u32, mmap[px + 1] as u32, mmap[px + 2] as u32);
            gray.push(((r * 299 + g * 587 + b * 114) / 1000) as u8);
        }
    }
    let _ = mmap.flush();

    Ok(Capture { width, height, gray })
}
