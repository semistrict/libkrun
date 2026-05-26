// Snapshot/restore orchestrator (capture-side wired up; restore-side
// rehydration helper is provided but the `build_microvm_for_restore`
// integration into the boot builder is a separate change).
//
// Capture flow:
//   1. Send Pause to every vCPU. Force-exit via hv_vcpus_exit so the vCPU
//      thread returns from hv_vcpu_run and processes the event.
//   2. Each vCPU thread serializes its HvfVcpuState and replies Paused(bytes).
//   3. Walk virtio MMIO transports: pause() each underlying device, then
//      serialize_state(). Collect MmioTransportState too.
//   4. Capture GICv3 state (distributor + per-vCPU pending IRQ bitmaps).
//   5. Write pages.img from guest memory.
//   6. Assemble vmstate.bin (META + per-vcpu + GICDIST + GICVCPU + per-virtio).
//   7. Atomic publish: write into a staging directory, rename to <path>.
//   8. Resume devices, then vCPUs.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use log::info;

use crossbeam_channel::RecvTimeoutError;
use devices::legacy::{GicV3, GicV3State, IrqChip, VcpuList, VcpuListState};
use devices::virtio::{DeviceSnapshot, MmioTransport, MmioTransportState};
use serde::{Deserialize, Serialize};
use vm_memory::{Address, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};

use super::container::{SectionId, SnapshotWriter};
use super::ram::{clone_and_patch_dirty_pages_img, write_sparse_pages_img, RamLayout};
use super::{Result, SnapshotError};

const VCPU_PAUSE_TIMEOUT_MS: u64 = 2000;

/// Top-level meta section: layout + acked-features-ish per-device summary
/// (the per-device payloads carry the device-specific detail).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetaSection {
    pub ram: RamLayout,
    /// MMIO base addresses, in registration order. The orchestrator emits
    /// one VirtioMmio section per entry with `index = position in this list`.
    pub virtio_bases: Vec<u64>,
    pub vcpu_count: u32,
    pub nested_enabled: bool,
    /// `CNTVCT_EL0` at capture, for timer re-arm on restore.
    pub capture_mach_time: u64,
}

/// Snapshot of a single virtio-mmio device: transport-side state + the
/// per-device payload returned by `VirtioDevice::serialize_state`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VirtioMmioSection {
    pub mmio_base: u64,
    pub device_type: u32,
    pub transport: MmioTransportState,
    /// None when the device doesn't implement per-device snapshot. The
    /// transport state is still recorded so the guest driver sees the
    /// expected MMIO programming on resume.
    pub device: Option<DeviceSnapshot>,
}

/// Inputs the orchestrator needs from the Vmm. Wired up in a dedicated method
/// so the orchestrator stays decoupled from the rest of the Vmm internals.
pub struct CaptureInputs<'a> {
    pub guest_memory: &'a GuestMemoryMmap,
    pub vcpu_handles: &'a [crate::vstate::VcpuHandle],
    pub vcpu_ids: &'a [u64],
    pub vcpu_list: &'a Arc<VcpuList>,
    pub irqchip: Option<&'a IrqChip>,
    pub gic: Option<&'a Arc<Mutex<GicV3>>>,
    pub virtio_transports: &'a [(u64, Arc<Mutex<MmioTransport>>)],
    pub nested_enabled: bool,
}

fn cntvct_el0() -> u64 {
    extern "C" {
        fn mach_absolute_time() -> u64;
    }
    unsafe { mach_absolute_time() }
}

/// Capture a complete snapshot into a staging directory, then publish it to `dir`.
pub fn capture(inputs: CaptureInputs<'_>, dir: &Path) -> Result<()> {
    // 1. Quiesce: pause all vCPUs and collect their state.
    let vcpu_states = match pause_vcpus(inputs.vcpu_handles, inputs.vcpu_ids) {
        Ok(states) => states,
        Err(e) => {
            let _ = resume_vcpus(inputs.vcpu_handles);
            return Err(e);
        }
    };

    let result = capture_paused(&inputs, dir, &vcpu_states);

    // Always attempt to resume every device and vCPU before returning. A failed
    // snapshot must not strand the caller's running VM in a paused state.
    let device_resume = resume_devices(&inputs);
    let vcpu_resume = resume_vcpus(inputs.vcpu_handles);

    result?;
    device_resume?;
    vcpu_resume?;
    Ok(())
}

pub fn arm_dirty_tracking(inputs: &CaptureInputs<'_>) -> Result<()> {
    let vcpu_states = match pause_vcpus(inputs.vcpu_handles, inputs.vcpu_ids) {
        Ok(states) => states,
        Err(e) => {
            let _ = resume_vcpus(inputs.vcpu_handles);
            return Err(e);
        }
    };
    drop(vcpu_states);

    let result = enable_dirty_tracking(inputs.guest_memory);
    let resume = resume_vcpus(inputs.vcpu_handles);
    result?;
    resume?;
    Ok(())
}

fn capture_paused(inputs: &CaptureInputs<'_>, dir: &Path, vcpu_states: &[Vec<u8>]) -> Result<()> {
    // 2. Capture transport-side state for EVERY virtio device, then attempt
    // to pause + serialize the device-specific payload. Devices that don't
    // implement snapshot reject the operation; otherwise they could continue
    // touching guest memory while RAM is being copied.
    let mut virtio_sections = Vec::new();
    for (base, transport_arc) in inputs.virtio_transports {
        let transport = transport_arc.lock().unwrap();
        let device_type = transport.locked_device().device_type();
        let device_arc = transport.device();
        let mut device = device_arc.lock().unwrap();
        let device_snap = match device.pause() {
            Ok(()) => match device.serialize_state() {
                Ok(s) => Some(s),
                Err(devices::virtio::DeviceSnapshotError::Unsupported(e)) => {
                    return Err(SnapshotError::DeviceRefused(format!(
                        "base=0x{base:x}: {e}"
                    )));
                }
                Err(e) => {
                    return Err(SnapshotError::DeviceRefused(format!(
                        "base=0x{base:x}: {e}"
                    )));
                }
            },
            Err(devices::virtio::DeviceSnapshotError::Unsupported(e)) => {
                return Err(SnapshotError::DeviceRefused(format!(
                    "base=0x{base:x}: {e}"
                )));
            }
            Err(e) => {
                return Err(SnapshotError::DeviceRefused(format!(
                    "base=0x{base:x}: {e}"
                )));
            }
        };
        let transport_state = transport.to_state();
        drop(device);
        drop(transport);
        virtio_sections.push(VirtioMmioSection {
            mmio_base: *base,
            device_type,
            transport: transport_state,
            device: device_snap,
        });
    }

    // 3. Capture GIC state.
    let hvf_gic_state = match inputs.irqchip {
        Some(irqchip) => irqchip
            .lock()
            .unwrap()
            .snapshot_state()
            .map_err(|e| SnapshotError::DeviceRefused(format!("irqchip snapshot: {e:?}")))?,
        None => None,
    };
    let gic_state = inputs.gic.map(|g| g.lock().unwrap().to_state());
    let vcpu_list_state = inputs.vcpu_list.to_state();

    // 4. Write RAM.
    let stage_dir = staging_dir(dir);
    if stage_dir.exists() {
        std::fs::remove_dir_all(&stage_dir)?;
    }
    std::fs::create_dir_all(&stage_dir)?;

    let result = (|| {
        let dirty_blocks = hvf::take_dirty_blocks_and_reprotect()
            .map_err(|e| SnapshotError::Io(std::io::Error::other(format!("dirty RAM: {e}"))))?;
        let ram = if dir.join(super::PAGES_IMG).exists() {
            clone_and_patch_dirty_pages_img(inputs.guest_memory, dir, &stage_dir, &dirty_blocks)?
        } else {
            write_sparse_pages_img(inputs.guest_memory, &stage_dir, &dirty_blocks)?
        };

        // 5. Assemble vmstate.bin.
        let mut total_ram: u64 = 0;
        let mut ram_base: u64 = u64::MAX;
        for region in inputs.guest_memory.iter() {
            total_ram += region.len();
            if region.start_addr().0 < ram_base {
                ram_base = region.start_addr().0;
            }
        }

        let meta = MetaSection {
            ram,
            virtio_bases: virtio_sections.iter().map(|s| s.mmio_base).collect(),
            vcpu_count: inputs.vcpu_handles.len() as u32,
            nested_enabled: inputs.nested_enabled,
            capture_mach_time: cntvct_el0(),
        };

        let mut writer = SnapshotWriter::new(total_ram, ram_base, meta.vcpu_count);
        writer.add_bincode(SectionId::Meta, 0, &meta)?;

        for (i, bytes) in vcpu_states.iter().enumerate() {
            writer.add_raw(SectionId::Vcpu, i as u32, bytes.clone());
        }
        if let Some(gic) = &gic_state {
            writer.add_bincode(SectionId::GicDist, 0, gic)?;
        }
        if let Some(hvf_gic) = hvf_gic_state {
            writer.add_raw(SectionId::HvfGic, 0, hvf_gic);
        }
        writer.add_bincode(SectionId::GicVcpu, 0, &vcpu_list_state)?;
        for (i, section) in virtio_sections.iter().enumerate() {
            writer.add_bincode(SectionId::VirtioMmio, i as u32, section)?;
        }

        writer.write_to_dir(&stage_dir)?;
        publish_snapshot_dir(&stage_dir, dir)?;
        enable_dirty_tracking(inputs.guest_memory)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_dir_all(&stage_dir);
    }

    result
}

fn resume_devices(inputs: &CaptureInputs<'_>) -> Result<()> {
    for (_base, transport_arc) in inputs.virtio_transports {
        let transport = transport_arc.lock().unwrap();
        let device_arc = transport.device();
        let mut device = device_arc.lock().unwrap();
        device
            .resume()
            .map_err(|e| SnapshotError::DeviceRefused(format!("resume: {e}")))?;
    }
    Ok(())
}

fn enable_dirty_tracking(mem: &GuestMemoryMmap) -> Result<()> {
    let ranges = mem
        .iter()
        .map(|region| (region.start_addr().raw_value(), region.len()))
        .collect::<Vec<_>>();
    hvf::enable_dirty_tracking(&ranges)
        .map_err(|e| SnapshotError::Io(std::io::Error::other(format!("enable dirty RAM: {e}"))))
}

fn staging_dir(dir: &Path) -> PathBuf {
    let name = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("snapshot");
    let stage_name = format!(".{name}.tmp.{}", std::process::id());
    match dir.parent() {
        Some(parent) => parent.join(stage_name),
        None => PathBuf::from(stage_name),
    }
}

fn publish_snapshot_dir(stage_dir: &Path, dir: &Path) -> Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::rename(stage_dir, dir)?;
    Ok(())
}

/// Sends Pause to every vCPU, forces them out of hv_vcpu_run, and collects
/// their serialized state.
fn pause_vcpus(handles: &[crate::vstate::VcpuHandle], vcpu_ids: &[u64]) -> Result<Vec<Vec<u8>>> {
    use crate::vstate::{VcpuEvent, VcpuResponse};

    for h in handles {
        h.send_event(VcpuEvent::Pause)
            .map_err(|e| SnapshotError::Io(std::io::Error::other(format!("send Pause: {e:?}"))))?;
    }
    // Kick each vCPU so it returns from hv_vcpu_run and picks up the event.
    for &id in vcpu_ids {
        let _ = hvf::vcpu_request_exit(id);
    }

    let mut out = Vec::with_capacity(handles.len());
    for (i, h) in handles.iter().enumerate() {
        match h
            .response_receiver()
            .recv_timeout(std::time::Duration::from_millis(VCPU_PAUSE_TIMEOUT_MS))
        {
            Ok(VcpuResponse::Paused(bytes)) => out.push(bytes),
            Ok(other) => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: unexpected response {other:?}"
                ))));
            }
            Err(RecvTimeoutError::Timeout) => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: pause timeout"
                ))));
            }
            Err(e) => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: {e}"
                ))));
            }
        }
    }
    Ok(out)
}

fn resume_vcpus(handles: &[crate::vstate::VcpuHandle]) -> Result<()> {
    use crate::vstate::{VcpuEvent, VcpuResponse};
    for h in handles {
        h.send_event(VcpuEvent::Resume)
            .map_err(|e| SnapshotError::Io(std::io::Error::other(format!("send Resume: {e:?}"))))?;
    }
    for (i, h) in handles.iter().enumerate() {
        match h
            .response_receiver()
            .recv_timeout(std::time::Duration::from_millis(VCPU_PAUSE_TIMEOUT_MS))
        {
            Ok(VcpuResponse::Resumed) => {}
            Ok(other) => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: unexpected resume response {other:?}"
                ))));
            }
            Err(e) => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: resume recv: {e}"
                ))));
            }
        }
    }
    Ok(())
}

/// Restore-side: given a fully-built (post-activate but pre-vCPU-run) VMM and
/// a SnapshotReader, push the captured state into vCPUs, GIC, and devices,
/// then re-arm the virtual timer. Caller has already constructed memory from
/// `pages.img`, so guest RAM is in place.
pub fn restore(inputs: &CaptureInputs<'_>, reader: &super::SnapshotReader) -> Result<()> {
    use crate::vstate::{VcpuEvent, VcpuResponse};

    info!("snapshot restore: starting");
    let meta: MetaSection = reader.get_bincode(SectionId::Meta, 0)?;
    info!(
        "snapshot restore: meta loaded — vcpu_count={}, ram={} bytes, virtio_devs={}",
        meta.vcpu_count,
        meta.ram.regions.iter().map(|r| r.size).sum::<u64>(),
        meta.virtio_bases.len()
    );
    if meta.vcpu_count != inputs.vcpu_handles.len() as u32 {
        return Err(SnapshotError::ConfigMismatch(format!(
            "snapshot vcpu_count {} != configured {}",
            meta.vcpu_count,
            inputs.vcpu_handles.len()
        )));
    }
    if meta.nested_enabled != inputs.nested_enabled {
        return Err(SnapshotError::ConfigMismatch(
            "nested_enabled differs between snapshot and current ctx".into(),
        ));
    }

    // vCPUs were pre-paused by the builder (queue_initial_pause), so they're
    // already blocked at the top of their first loop iteration. Drain their
    // initial Paused responses before sending RestoreState.
    for (i, h) in inputs.vcpu_handles.iter().enumerate() {
        match h
            .response_receiver()
            .recv_timeout(std::time::Duration::from_millis(VCPU_PAUSE_TIMEOUT_MS))
        {
            Ok(VcpuResponse::Paused(_)) => {}
            Ok(other) => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: expected initial Paused, got {other:?}"
                ))));
            }
            Err(RecvTimeoutError::Timeout) => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: initial-pause timeout"
                ))));
            }
            Err(e) => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: {e}"
                ))));
            }
        }
    }

    // Restore GIC state.
    if let Some(irqchip) = inputs.irqchip {
        if let Ok(st) = reader.get_raw(SectionId::HvfGic, 0) {
            irqchip
                .lock()
                .unwrap()
                .restore_snapshot_state(st)
                .map_err(|e| SnapshotError::DeviceRefused(format!("irqchip restore: {e:?}")))?;
        }
    }
    if let Some(gic) = inputs.gic {
        if let Ok(st) = reader.get_bincode::<GicV3State>(SectionId::GicDist, 0) {
            gic.lock().unwrap().restore_state(&st);
        }
    }
    if let Ok(st) = reader.get_bincode::<VcpuListState>(SectionId::GicVcpu, 0) {
        inputs.vcpu_list.restore_state(&st);
    }

    // Push HvfVcpuState into each vcpu after the global GIC state is restored:
    // the per-vCPU CPU-interface and ICH registers are owned by the vCPU
    // thread and must be the final GIC state written for that vCPU.
    for (i, h) in inputs.vcpu_handles.iter().enumerate() {
        let bytes = reader.get_raw(SectionId::Vcpu, i as u32)?.to_vec();
        h.send_event(VcpuEvent::RestoreState(bytes)).map_err(|e| {
            SnapshotError::Io(std::io::Error::other(format!("send RestoreState: {e:?}")))
        })?;
        match h
            .response_receiver()
            .recv_timeout(std::time::Duration::from_millis(VCPU_PAUSE_TIMEOUT_MS))
        {
            Ok(VcpuResponse::Restored) => {}
            Ok(VcpuResponse::Error(s)) => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: restore: {s}"
                ))));
            }
            other => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: unexpected {other:?}"
                ))));
            }
        }
    }

    // Restore virtio devices — match by MMIO base, not by index, so out-of-scope
    // devices in the current ctx (e.g. virtio-balloon) don't shift the mapping.
    for i in 0..meta.virtio_bases.len() {
        let section: VirtioMmioSection = reader.get_bincode(SectionId::VirtioMmio, i as u32)?;
        let transport_arc = inputs
            .virtio_transports
            .iter()
            .find_map(|(b, t)| {
                if *b == section.mmio_base {
                    Some(t)
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                SnapshotError::ConfigMismatch(format!(
                    "no virtio device at base 0x{:x} in current ctx",
                    section.mmio_base
                ))
            })?;
        {
            let mut transport = transport_arc.lock().unwrap();
            if let Some(device_snap) = &section.device {
                transport
                    .restore_queues_and_activate(&section.transport, &device_snap.queues)
                    .map_err(|e| {
                        SnapshotError::DeviceRefused(format!(
                            "base=0x{:x}: activate: {e}",
                            section.mmio_base
                        ))
                    })?;
            } else {
                transport.restore_state(&section.transport);
            }
        }
        if let Some(device_snap) = &section.device {
            let transport = transport_arc.lock().unwrap();
            let device_arc = transport.device();
            let mut device = device_arc.lock().unwrap();
            if let Err(e) = device.pause() {
                return Err(SnapshotError::DeviceRefused(format!(
                    "base=0x{:x}: pause: {e}",
                    section.mmio_base
                )));
            }
            device.restore_state(device_snap).map_err(|e| {
                SnapshotError::DeviceRefused(format!(
                    "base=0x{:x}: restore: {e}",
                    section.mmio_base
                ))
            })?;
            device.resume().map_err(|e| {
                SnapshotError::DeviceRefused(format!("base=0x{:x}: resume: {e}", section.mmio_base))
            })?;
        }
        transport_arc.lock().unwrap().replay_pending_interrupt();
    }

    let timer_delta = cntvct_el0().wrapping_sub(meta.capture_mach_time);
    for (i, h) in inputs.vcpu_handles.iter().enumerate() {
        h.send_event(VcpuEvent::RebaseTimer(timer_delta))
            .map_err(|e| {
                SnapshotError::Io(std::io::Error::other(format!("send RebaseTimer: {e:?}")))
            })?;
        match h
            .response_receiver()
            .recv_timeout(std::time::Duration::from_millis(VCPU_PAUSE_TIMEOUT_MS))
        {
            Ok(VcpuResponse::TimerRebased) => {}
            Ok(VcpuResponse::Error(s)) => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: timer rebase: {s}"
                ))));
            }
            other => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: unexpected {other:?}"
                ))));
            }
        }
    }

    info!("snapshot restore: device state restored, resuming vcpus");
    resume_vcpus(inputs.vcpu_handles)?;
    for (_, transport) in inputs.virtio_transports {
        transport.lock().unwrap().replay_pending_interrupt();
    }
    info!("snapshot restore: complete");

    Ok(())
}
