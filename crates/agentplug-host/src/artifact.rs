use std::path::Path;

const WASM_MAGIC: &[u8; 4] = b"\0asm";
const CORE_MODULE_HEADER: [u8; 8] = [0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
const COMPONENT_HEADER: [u8; 8] = [0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    CoreModule,
    Component,
}

pub const COMPONENT_ON_CORE_PATH: &str = "artifact.component_on_core_path";
pub const CORE_MODULE_ON_COMPONENT_PATH: &str = "artifact.core_module_on_component_path";

pub fn artifact_kind(bytes: &[u8]) -> anyhow::Result<ArtifactKind> {
    if bytes.len() < 8 || &bytes[..4] != WASM_MAGIC {
        anyhow::bail!("artifact.not_wasm: the artifact does not start with the wasm magic");
    }
    match &bytes[..8] {
        h if h == CORE_MODULE_HEADER => Ok(ArtifactKind::CoreModule),
        h if h == COMPONENT_HEADER => Ok(ArtifactKind::Component),
        other => anyhow::bail!("artifact.unknown_header: {other:02x?} is neither a core module nor a component"),
    }
}

pub fn artifact_kind_of_file(path: &Path) -> anyhow::Result<ArtifactKind> {
    let mut header = [0u8; 8];
    let read = {
        use std::io::Read;
        std::fs::File::open(path)?.read(&mut header)?
    };
    artifact_kind(&header[..read])
}
