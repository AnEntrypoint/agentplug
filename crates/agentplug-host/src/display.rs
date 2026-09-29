use serde_json::{json, Value};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ActiveTarget {
    pub(crate) target: (u32, i32, u32),
    pub(crate) monitor: String,
    pub(crate) source: (u32, i32, u32),
    pub(crate) refresh_hz: f64,
    pub(crate) preferred_hz: f64,
    pub(crate) virtual_display: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Topology {
    pub(crate) targets: Vec<ActiveTarget>,
}

impl Topology {
    fn clone_partners(&self, t: &ActiveTarget) -> impl Iterator<Item = &ActiveTarget> {
        let (source, target) = (t.source, t.target);
        self.targets.iter().filter(move |o| o.source == source && o.target != target)
    }

    pub(crate) fn refresh_cap_hz(&self) -> Option<u32> {
        self.targets.iter().map(|t| t.refresh_hz).fold(None, |m: Option<f64>, r| Some(m.map_or(r, |m| m.max(r)))).map(|r| r.round() as u32)
    }

    pub(crate) fn clone_with_virtual(&self) -> bool {
        self.targets.iter().any(|t| t.virtual_display && self.clone_partners(t).any(|p| !p.virtual_display))
    }

    fn capped_panel(&self) -> Option<&ActiveTarget> {
        self.targets.iter().find(|t| !t.virtual_display && t.preferred_hz > t.refresh_hz + 1.0 && self.clone_partners(t).next().is_some())
    }

    pub(crate) fn report(&self, raf_fps: Option<u64>, uncapped: bool) -> Value {
        let mut out = json!({
            "refresh_cap_hz": self.refresh_cap_hz(),
            "display_clone_with_virtual": self.clone_with_virtual(),
            "uncapped": uncapped,
            "displays": self.targets.iter().map(|t| format!(
                "{}@{}Hz{}{}{}",
                t.monitor,
                t.refresh_hz.round() as u32,
                if t.preferred_hz > t.refresh_hz + 1.0 { format!("(panel {}Hz)", t.preferred_hz.round() as u32) } else { String::new() },
                if t.virtual_display { " virtual" } else { "" },
                if self.clone_partners(t).next().is_some() { " clone" } else { "" },
            )).collect::<Vec<_>>(),
        });
        if let Some(panel) = self.capped_panel() {
            let partners: Vec<String> = self.clone_partners(panel).map(|p| format!("{}{}", p.monitor, if p.virtual_display { "(virtual)" } else { "" })).collect();
            out["display_warn"] = json!(format!(
                "{} can run {}Hz but is cloned with {} so the desktop runs {}Hz{}; for real frame rates extend or disconnect the clone partner (Win+P -> Extend, or disable the virtual display) -- not changed automatically",
                panel.monitor,
                panel.preferred_hz.round() as u32,
                partners.join(","),
                panel.refresh_hz.round() as u32,
                match (uncapped, raf_fps) {
                    (true, Some(fps)) => format!(", uncapped session measured rAF {fps}fps"),
                    (false, Some(fps)) => format!(", rAF measured {fps}fps; `session new uncapped` measures past the cap"),
                    (false, None) => ", headed rAF is capped there; `session new uncapped` measures past it".to_string(),
                    (true, None) => String::new(),
                }
            ));
        }
        out
    }
}

#[cfg(windows)]
mod win {
    use super::{ActiveTarget, Topology};

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Luid {
        low: u32,
        high: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct PathSource {
        adapter: Luid,
        id: u32,
        mode_idx: u32,
        flags: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct PathTarget {
        adapter: Luid,
        id: u32,
        mode_idx: u32,
        output_technology: u32,
        rotation: u32,
        scaling: u32,
        refresh_num: u32,
        refresh_den: u32,
        scanline: u32,
        available: i32,
        flags: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct PathInfo {
        source: PathSource,
        target: PathTarget,
        flags: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ModeInfo([u64; 8]);

    #[repr(C)]
    struct Header {
        kind: u32,
        size: u32,
        adapter: Luid,
        id: u32,
    }

    #[repr(C)]
    struct TargetName {
        header: Header,
        flags: u32,
        output_technology: u32,
        edid_manufacture: u16,
        edid_product: u16,
        connector: u32,
        friendly: [u16; 64],
        device_path: [u16; 128],
    }

    #[repr(C)]
    struct PreferredMode {
        header: Header,
        width: u32,
        height: u32,
        pixel_rate: u64,
        hsync: [u32; 2],
        vsync: [u32; 2],
        rest: [u32; 6],
    }

    const QDC_ONLY_ACTIVE_PATHS: u32 = 2;
    const GET_TARGET_NAME: u32 = 2;
    const GET_TARGET_PREFERRED_MODE: u32 = 3;
    const OUTPUT_INDIRECT_VIRTUAL: u32 = 17;
    const ERROR_INSUFFICIENT_BUFFER: i32 = 122;
    const QUERY_ATTEMPTS: usize = 4;

    #[link(name = "user32")]
    extern "system" {
        fn GetDisplayConfigBufferSizes(flags: u32, paths: *mut u32, modes: *mut u32) -> i32;
        fn QueryDisplayConfig(flags: u32, paths: *mut u32, path_array: *mut PathInfo, modes: *mut u32, mode_array: *mut ModeInfo, topology: *mut u32) -> i32;
        fn DisplayConfigGetDeviceInfo(packet: *mut Header) -> i32;
    }

    #[link(name = "cfgmgr32")]
    extern "system" {
        fn CM_Locate_DevNodeW(devinst: *mut u32, device_id: *const u16, flags: u32) -> u32;
        fn CM_Get_Parent(parent: *mut u32, devinst: u32, flags: u32) -> u32;
        fn CM_Get_Device_IDW(devinst: u32, buffer: *mut u16, len: u32, flags: u32) -> u32;
    }

    fn parent_device_id(monitor_device_path: &str) -> Option<String> {
        let inner = monitor_device_path.strip_prefix("\\\\?\\")?;
        let instance = inner.split("#{").next()?.replace('#', "\\");
        let wide_id: Vec<u16> = instance.encode_utf16().chain([0]).collect();
        let (mut node, mut parent) = (0u32, 0u32);
        let mut buf = [0u16; 256];
        let ok = unsafe {
            CM_Locate_DevNodeW(&mut node, wide_id.as_ptr(), 0) == 0
                && CM_Get_Parent(&mut parent, node, 0) == 0
                && CM_Get_Device_IDW(parent, buf.as_mut_ptr(), buf.len() as u32, 0) == 0
        };
        ok.then(|| wide(&buf))
    }

    fn header<T>(kind: u32, adapter: Luid, id: u32) -> Header {
        Header { kind, size: std::mem::size_of::<T>() as u32, adapter, id }
    }

    fn wide(s: &[u16]) -> String {
        String::from_utf16_lossy(&s[..s.iter().position(|c| *c == 0).unwrap_or(s.len())])
    }

    fn hz(num: u32, den: u32) -> f64 {
        if den == 0 { 0.0 } else { num as f64 / den as f64 }
    }

    fn monitor_id(device_path: &str, fallback: String) -> String {
        device_path.split('#').nth(1).filter(|s| !s.is_empty()).map(str::to_string).unwrap_or(fallback)
    }

    fn device_info<T>(packet: &mut T) -> bool {
        unsafe { DisplayConfigGetDeviceInfo(std::ptr::from_mut(packet).cast::<Header>()) == 0 }
    }

    fn active_paths() -> Result<Vec<PathInfo>, String> {
        for _ in 0..QUERY_ATTEMPTS {
            let (mut path_count, mut mode_count) = (0u32, 0u32);
            let rc = unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count) };
            if rc != 0 {
                return Err(format!("GetDisplayConfigBufferSizes failed ({rc})"));
            }
            if path_count == 0 {
                return Ok(Vec::new());
            }
            let mut paths = vec![PathInfo::default(); path_count as usize];
            let mut modes = vec![ModeInfo([0; 8]); mode_count.max(1) as usize];
            let rc = unsafe { QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut path_count, paths.as_mut_ptr(), &mut mode_count, modes.as_mut_ptr(), std::ptr::null_mut()) };
            match rc {
                0 => {
                    paths.truncate(path_count as usize);
                    return Ok(paths);
                }
                ERROR_INSUFFICIENT_BUFFER => continue,
                _ => return Err(format!("QueryDisplayConfig failed ({rc})")),
            }
        }
        Err(format!("QueryDisplayConfig kept reporting a changed topology across {QUERY_ATTEMPTS} attempts"))
    }

    pub(super) fn query() -> Result<Topology, String> {
        let paths = active_paths()?;
        let targets = paths
            .iter()
            .map(|p| {
                let t = p.target;
                let mut name = TargetName { header: header::<TargetName>(GET_TARGET_NAME, t.adapter, t.id), flags: 0, output_technology: 0, edid_manufacture: 0, edid_product: 0, connector: 0, friendly: [0; 64], device_path: [0; 128] };
                let named = device_info(&mut name);
                let mut preferred = PreferredMode { header: header::<PreferredMode>(GET_TARGET_PREFERRED_MODE, t.adapter, t.id), width: 0, height: 0, pixel_rate: 0, hsync: [0; 2], vsync: [0; 2], rest: [0; 6] };
                let preferred_hz = if device_info(&mut preferred) { hz(preferred.vsync[0], preferred.vsync[1]) } else { 0.0 };
                let device_path = if named { wide(&name.device_path) } else { String::new() };
                let refresh_hz = hz(t.refresh_num, t.refresh_den);
                ActiveTarget {
                    target: (t.adapter.low, t.adapter.high, t.id),
                    monitor: monitor_id(&device_path, format!("target{}", t.id)),
                    source: (p.source.adapter.low, p.source.adapter.high, p.source.id),
                    refresh_hz,
                    preferred_hz: preferred_hz.max(refresh_hz),
                    virtual_display: t.output_technology == OUTPUT_INDIRECT_VIRTUAL
                        || parent_device_id(&device_path).is_some_and(|id| id.to_ascii_uppercase().starts_with("ROOT\\")),
                }
            })
            .collect();
        Ok(Topology { targets })
    }
}

#[cfg(windows)]
pub(crate) fn query() -> Result<Topology, String> {
    win::query()
}

#[cfg(not(windows))]
pub(crate) fn query() -> Result<Topology, String> {
    Err("display topology probe is implemented on Windows only".to_string())
}
