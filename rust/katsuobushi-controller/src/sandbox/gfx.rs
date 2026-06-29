//! GPU selection — the role-preference ladder (§7).
//!
//! Config exposes an ordered preference list of GPU *roles*
//! ([`GpuRole`](crate::sandbox::spec::GpuRole), e.g. `[integrated, discrete,
//! software]`). [`resolve_gpu`] walks that list and picks the first rung that is
//! satisfiable on this host: a render node that classifies as the requested role
//! *and* is openable by the QEMU uid, or the always-available `software` floor.
//! The resolved choice drives both the launch path (#036, `start.rs`) and the
//! preflight row (#037, `status.rs`), so the algorithm lives here once.
//!
//! The world is reached only through the [`Host`] seam ([`render_nodes`] /
//! [`can_open`]), so the resolver is exercised end to end against the `FakeHost`
//! without touching real `/dev/dri`. The integrated-vs-discrete [`classify`]
//! step reads sysfs in production, but is injected into the resolver so its unit
//! tests never touch `/sys` either.
//!
//! [`Host`]: crate::sandbox::host::Host
//! [`render_nodes`]: crate::sandbox::host::Host::render_nodes
//! [`can_open`]: crate::sandbox::host::Host::can_open
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use crate::sandbox::host::Host;
use crate::sandbox::spec::GpuRole;

/// The outcome of resolving the `gpu` role ladder against a host (§7.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// A usable hardware GPU rung: the runner exports
    /// `KATSU_GFX_RENDERNODE=<node>` and (when `venus`) `KATSU_GFX_VENUS=1`, and
    /// `extraArgsScript` emits the `virtio-gpu-gl` + `egl-headless` lines. `role`
    /// is the rung this node satisfied (§7.2), surfaced verbatim in the §12/§18
    /// preflight row.
    Gpu {
        node: PathBuf,
        role: GpuRole,
        venus: bool,
    },
    /// The `software` rung: Mesa llvmpipe in-guest, **no GPU device and no host
    /// render node at all** (§7.3). The runner exports nothing GPU-related.
    Software,
    /// The list was exhausted with no `software` tail: no usable GPU and no
    /// fallback ⇒ a hard error at launch / a `MISSING` preflight row (§12). This
    /// is a project's deliberate "fail loud, never silently slow" choice.
    Unavailable,
}

/// Resolve the ordered GPU role preference list against `host` (§7.2).
///
/// Walks `prefs` in order, first match wins:
/// - `software` is always satisfiable ⇒ [`Resolution::Software`];
/// - `integrated`/`discrete` require a render node that [`classify`]s as that
///   role **and** that the QEMU uid can `open(O_RDWR)`
///   ([`can_open`](crate::sandbox::host::Host::can_open)) ⇒
///   [`Resolution::Gpu`] with `venus: true`;
/// - exhausting the list with no `software` tail ⇒ [`Resolution::Unavailable`].
///
/// A node present but *not* openable does not satisfy its role, so the walk
/// continues to the next preferred role (the §14.3 permission prerequisite).
pub fn resolve_gpu(prefs: &[GpuRole], host: &dyn Host) -> Resolution {
    resolve_gpu_with(prefs, host, classify)
}

/// [`resolve_gpu`] over an injected classifier, so the ladder logic is unit-tested
/// against the `FakeHost` without the production [`classify`] ever touching
/// `/sys` (the per-node role is mocked; openability comes from the host seam).
fn resolve_gpu_with(
    prefs: &[GpuRole],
    host: &dyn Host,
    classify: impl Fn(&Path) -> GpuRole,
) -> Resolution {
    // A missing DRI subsystem yields `[]` (not an error), which simply means no
    // GPU rung can match and resolution falls through to `software`/`Unavailable`.
    let nodes = host.render_nodes().unwrap_or_default();
    let classified: Vec<(PathBuf, GpuRole)> = nodes
        .into_iter()
        .map(|node| {
            let role = classify(&node);
            (node, role)
        })
        .collect();

    for role in prefs {
        match role {
            GpuRole::Software => return Resolution::Software,
            GpuRole::Integrated | GpuRole::Discrete => {
                if let Some((node, _)) = classified
                    .iter()
                    .find(|(node, kind)| kind == role && host.can_open(node))
                {
                    return Resolution::Gpu {
                        node: node.clone(),
                        role: *role,
                        venus: true,
                    };
                }
            }
        }
    }

    Resolution::Unavailable
}

/// A discrete GPU reports its own dedicated VRAM (multiple GiB); an iGPU either
/// omits `mem_info_vram_total` or reports a small UMA carve-out. 1 GiB sits well
/// above any iGPU carve-out and well below any dGPU's VRAM, so it cleanly splits
/// the two on the hosts we target.
const DISCRETE_VRAM_MIN: u64 = 1 << 30;

/// Classify a render node as `integrated` or `discrete` (§7.4) — the one fiddly
/// bit of selection. Returns only [`GpuRole::Integrated`] or
/// [`GpuRole::Discrete`]; a render node is always real hardware, never the
/// `software` rung.
///
/// This is the **zero-dependency sysfs heuristic** (§7.4 option 2): it reads
/// `/sys/class/drm/<node>/device/{boot_vga,mem_info_vram_total,driver}` and never
/// shells out. The signals, in priority order:
///
/// 1. `boot_vga == 1` — the firmware primary scanout. On the hybrid-graphics
///    laptops this targets, that is the iGPU ⇒ `integrated`.
/// 2. `mem_info_vram_total` — dedicated VRAM ≥ [`DISCRETE_VRAM_MIN`] ⇒ `discrete`;
///    a smaller carve-out ⇒ `integrated`.
/// 3. `driver` (the bound kernel driver, read via the `device/driver` symlink) —
///    a paravirtual GPU (`virtio_gpu`/`vmwgfx`/`qxl`/`bochs`/`simpledrm`) presents
///    as a single low-power device with no dedicated VRAM, so it maps to
///    `integrated`.
/// 4. Fallback ⇒ `integrated`, the safer, lower-power rung that is always present
///    on an APU host.
///
/// **Escalation path (§7.4 option 1).** The authoritative signal is the Vulkan/GL
/// device type (`VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU` vs `…_DISCRETE_GPU`). If
/// a real host misclassifies under the sysfs heuristic, escalate to shelling a
/// tiny probe (`vulkaninfo`/`drm_info`) and mapping the device type →
/// `drmRenderMajor/Minor` (via `VK_EXT_physical_device_drm`) → node path. That
/// adds a runtime dependency, so it is deferred until a host actually needs it.
pub fn classify(node: &Path) -> GpuRole {
    classify_at(node, Path::new("/sys/class/drm"))
}

/// [`classify`] over an injected `/sys/class/drm` root, so the heuristic is
/// exercisable against a fixture tree without touching the real `/sys`.
fn classify_at(node: &Path, sys_class_drm: &Path) -> GpuRole {
    // The render node's sysfs entry keys off its file name (e.g. `renderD128`):
    // `/sys/class/drm/<name>/device/`. A node we cannot name has no sysfs entry,
    // so fall back to the safe rung.
    let Some(name) = node.file_name() else {
        return GpuRole::Integrated;
    };
    let device = sys_class_drm.join(name).join("device");

    // 1. The firmware primary scanout is the iGPU on a hybrid-graphics host.
    if read_trimmed(&device.join("boot_vga")).as_deref() == Some("1") {
        return GpuRole::Integrated;
    }

    // 2. Dedicated VRAM size: a discrete card carries GiB of its own VRAM.
    if let Some(vram) =
        read_trimmed(&device.join("mem_info_vram_total")).and_then(|s| s.parse::<u64>().ok())
    {
        return if vram >= DISCRETE_VRAM_MIN {
            GpuRole::Discrete
        } else {
            GpuRole::Integrated
        };
    }

    // 3. The bound kernel driver: a paravirtual GPU has no dedicated VRAM and is
    //    a single low-power device, so treat it as integrated. Any other driver
    //    that reached this far (no boot_vga, no VRAM file) is an unannotated real
    //    GPU; fall back to the safe rung.
    if let Some(driver) = read_driver(&device.join("driver")) {
        if matches!(
            driver.as_str(),
            "virtio_gpu" | "vmwgfx" | "qxl" | "bochs" | "simpledrm"
        ) {
            return GpuRole::Integrated;
        }
    }

    GpuRole::Integrated
}

/// Read a small sysfs attribute and trim trailing whitespace, or `None` if it is
/// absent/unreadable (sysfs attributes are tiny single-line files).
fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// The bound kernel driver name from a `device/driver` symlink (its target's
/// final path component, e.g. `amdgpu`), or `None` if the link is absent.
fn read_driver(driver_link: &Path) -> Option<String> {
    std::fs::read_link(driver_link)
        .ok()
        .and_then(|target| target.file_name().map(|n| n.to_string_lossy().into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::host::FakeHost;

    const INTEGRATED: &str = "/dev/dri/renderD128";
    const DISCRETE: &str = "/dev/dri/renderD129";

    /// A classifier driven by an explicit node→role table, so the resolver tests
    /// never invoke the real sysfs-reading [`classify`].
    fn fake_classify(node: &Path) -> GpuRole {
        match node.to_str() {
            Some(INTEGRATED) => GpuRole::Integrated,
            Some(DISCRETE) => GpuRole::Discrete,
            other => panic!("unexpected node in test: {other:?}"),
        }
    }

    /// A host with both nodes present and both openable.
    fn both_nodes_openable() -> FakeHost {
        let mut host = FakeHost::new();
        host.with_render_node(INTEGRATED)
            .with_render_node(DISCRETE)
            .with_openable(INTEGRATED)
            .with_openable(DISCRETE);
        host
    }

    #[test]
    fn it_picks_integrated_first_when_preferred_and_present() {
        let host = both_nodes_openable();
        let res = resolve_gpu_with(
            &[GpuRole::Integrated, GpuRole::Discrete, GpuRole::Software],
            &host,
            fake_classify,
        );
        assert_eq!(
            res,
            Resolution::Gpu {
                node: PathBuf::from(INTEGRATED),
                role: GpuRole::Integrated,
                venus: true,
            }
        );
    }

    #[test]
    fn it_falls_back_to_discrete_when_integrated_absent() {
        // Only the discrete node is present; `integrated` cannot match, so the
        // walk advances to `discrete`.
        let mut host = FakeHost::new();
        host.with_render_node(DISCRETE).with_openable(DISCRETE);
        let res = resolve_gpu_with(
            &[GpuRole::Integrated, GpuRole::Discrete, GpuRole::Software],
            &host,
            fake_classify,
        );
        assert_eq!(
            res,
            Resolution::Gpu {
                node: PathBuf::from(DISCRETE),
                role: GpuRole::Discrete,
                venus: true,
            }
        );
    }

    #[test]
    fn it_falls_to_the_software_tail_when_no_gpu_matches() {
        // No render nodes at all ⇒ neither GPU rung matches ⇒ the `software`
        // tail satisfies the list.
        let host = FakeHost::new();
        let res = resolve_gpu_with(
            &[GpuRole::Integrated, GpuRole::Discrete, GpuRole::Software],
            &host,
            fake_classify,
        );
        assert_eq!(res, Resolution::Software);
    }

    #[test]
    fn it_returns_unavailable_with_no_software_tail() {
        // A security-pinned-but-GPU-less host: the list exhausts with no
        // `software` rung ⇒ fail loud (§7.2).
        let host = FakeHost::new();
        let res = resolve_gpu_with(
            &[GpuRole::Integrated, GpuRole::Discrete],
            &host,
            fake_classify,
        );
        assert_eq!(res, Resolution::Unavailable);
    }

    #[test]
    fn it_skips_an_unopenable_preferred_node_for_the_next_role() {
        // The integrated node is present but the QEMU uid cannot open it (not in
        // the `render` group, §14.3); the discrete node is openable. `integrated`
        // must be skipped and `discrete` chosen.
        let mut host = FakeHost::new();
        host.with_render_node(INTEGRATED)
            .with_render_node(DISCRETE)
            .with_openable(DISCRETE);
        let res = resolve_gpu_with(
            &[GpuRole::Integrated, GpuRole::Discrete, GpuRole::Software],
            &host,
            fake_classify,
        );
        assert_eq!(
            res,
            Resolution::Gpu {
                node: PathBuf::from(DISCRETE),
                role: GpuRole::Discrete,
                venus: true,
            }
        );
    }

    #[test]
    fn it_skips_an_unopenable_node_to_the_software_tail() {
        // The only node present is unopenable; with a `software` tail the list is
        // still satisfiable — the GPU rung is simply skipped.
        let mut host = FakeHost::new();
        host.with_render_node(INTEGRATED);
        let res = resolve_gpu_with(
            &[GpuRole::Integrated, GpuRole::Software],
            &host,
            fake_classify,
        );
        assert_eq!(res, Resolution::Software);
    }

    #[test]
    fn it_short_circuits_at_software_ignoring_later_roles() {
        // A `software`-first list resolves to software even with usable GPUs
        // present — the maximal-isolation pin (§7.3).
        let host = both_nodes_openable();
        let res = resolve_gpu_with(
            &[GpuRole::Software, GpuRole::Integrated],
            &host,
            fake_classify,
        );
        assert_eq!(res, Resolution::Software);
    }

    #[test]
    fn it_returns_unavailable_for_an_empty_preference_list() {
        // Degenerate `gpu = []`: nothing to satisfy ⇒ Unavailable.
        let host = both_nodes_openable();
        let res = resolve_gpu_with(&[], &host, fake_classify);
        assert_eq!(res, Resolution::Unavailable);
    }

    // --- classify() sysfs heuristic, over a fixture tree (never the real /sys) ---

    /// Lay down `/sys/class/drm/<name>/device/<attr> = <value>` under `root`.
    fn write_attr(root: &Path, name: &str, attr: &str, value: &str) {
        let device = root.join(name).join("device");
        std::fs::create_dir_all(&device).unwrap();
        std::fs::write(device.join(attr), value).unwrap();
    }

    fn temp_sys_root(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("katsuctl-gfx-test-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn classify_treats_boot_vga_as_integrated() {
        let root = temp_sys_root("bootvga");
        // boot_vga wins even with large VRAM present.
        write_attr(&root, "renderD128", "boot_vga", "1");
        write_attr(&root, "renderD128", "mem_info_vram_total", "17179869184");
        assert_eq!(
            classify_at(Path::new("/dev/dri/renderD128"), &root),
            GpuRole::Integrated
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn classify_treats_large_vram_as_discrete() {
        let root = temp_sys_root("vram-big");
        write_attr(&root, "renderD129", "boot_vga", "0");
        write_attr(&root, "renderD129", "mem_info_vram_total", "17179869184");
        assert_eq!(
            classify_at(Path::new("/dev/dri/renderD129"), &root),
            GpuRole::Discrete
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn classify_treats_small_vram_as_integrated() {
        let root = temp_sys_root("vram-small");
        // A 512 MiB UMA carve-out is an iGPU, not a discrete card.
        write_attr(&root, "renderD128", "mem_info_vram_total", "536870912");
        assert_eq!(
            classify_at(Path::new("/dev/dri/renderD128"), &root),
            GpuRole::Integrated
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn classify_treats_a_paravirtual_driver_as_integrated() {
        // A virtio-gpu node (no boot_vga, no VRAM file) is classified via the
        // `device/driver` symlink ⇒ integrated.
        let root = temp_sys_root("virtio");
        let device = root.join("renderD128").join("device");
        std::fs::create_dir_all(&device).unwrap();
        // Mimic the sysfs `driver` symlink → .../drivers/virtio_gpu.
        let driver_target = root.join("bus/virtio/drivers/virtio_gpu");
        std::fs::create_dir_all(&driver_target).unwrap();
        std::os::unix::fs::symlink(&driver_target, device.join("driver")).unwrap();
        assert_eq!(
            classify_at(Path::new("/dev/dri/renderD128"), &root),
            GpuRole::Integrated
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn classify_defaults_to_integrated_when_sysfs_is_bare() {
        // No attributes at all (e.g. a virtio-gpu node) ⇒ the safe rung.
        let root = temp_sys_root("bare");
        assert_eq!(
            classify_at(Path::new("/dev/dri/renderD128"), &root),
            GpuRole::Integrated
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
