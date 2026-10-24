use std::io::Read;
use std::path::Path;

use anyhow::{anyhow, bail};
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Module};

use crate::component::{component_linker, ComponentState};
use crate::registry::SiblingPools;
use crate::precompiled::load_module_file_backed;

const WASM_MAGIC: [u8; 4] = *b"\0asm";
const CORE_VERSION_WORD: [u8; 4] = [0x01, 0x00, 0x00, 0x00];
const COMPONENT_VERSION_WORD: [u8; 4] = [0x0d, 0x00, 0x01, 0x00];
const HEADER_LEN: usize = 8;

pub const CORE_LINKER: &str = "core";
pub const COMPONENT_LINKER: &str = "component_linker";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    Core,
    Component,
}

pub fn artifact_kind(header: &[u8]) -> anyhow::Result<ArtifactKind> {
    let header = header
        .get(..HEADER_LEN)
        .ok_or_else(|| anyhow!("unknown artifact: {} bytes is shorter than a wasm header", header.len()))?;
    if header[..4] != WASM_MAGIC {
        bail!("unknown artifact: missing wasm magic");
    }
    let version: [u8; 4] = [header[4], header[5], header[6], header[7]];
    if version == CORE_VERSION_WORD {
        Ok(ArtifactKind::Core)
    } else if version == COMPONENT_VERSION_WORD {
        Ok(ArtifactKind::Component)
    } else {
        bail!("unknown artifact: wasm version word {version:02x?} is neither a core module nor a component")
    }
}

fn read_header(path: &Path) -> anyhow::Result<[u8; HEADER_LEN]> {
    let mut header = [0u8; HEADER_LEN];
    std::fs::File::open(path)?
        .read_exact(&mut header)
        .map_err(|e| anyhow!("unknown artifact {}: {e}", path.display()))?;
    Ok(header)
}

pub enum LoadedArtifact {
    Core(Module),
    Component {
        component: Component,
        linker: Linker<ComponentState>,
    },
}

impl LoadedArtifact {
    pub fn kind(&self) -> ArtifactKind {
        match self {
            LoadedArtifact::Core(_) => ArtifactKind::Core,
            LoadedArtifact::Component { .. } => ArtifactKind::Component,
        }
    }

    pub fn linker_name(&self) -> &'static str {
        match self {
            LoadedArtifact::Core(_) => CORE_LINKER,
            LoadedArtifact::Component { .. } => COMPONENT_LINKER,
        }
    }
}

impl LoadedArtifact {
    pub async fn dispatch_verb(
        &self,
        component_engine: &Engine,
        plugin_name: &str,
        siblings: &SiblingPools,
        cwd: &Path,
        verb: &str,
        body: &str,
    ) -> anyhow::Result<String> {
        match self {
            LoadedArtifact::Component { component, linker } => {
                crate::component::dispatch_component_verb(component_engine, component, linker, plugin_name, siblings, cwd, verb, body).await
            }
            LoadedArtifact::Core(_) => bail!("{plugin_name} is a core module, served through registry dispatch_on"),
        }
    }
}

pub fn load_selected_artifact(
    core_engine: &Engine,
    component_engine: &Engine,
    wasm_path: &Path,
    plugin_name: &str,
    content_hash: &str,
) -> anyhow::Result<LoadedArtifact> {
    match artifact_kind(&read_header(wasm_path)?)? {
        ArtifactKind::Core => Ok(LoadedArtifact::Core(load_module_file_backed(
            core_engine,
            wasm_path,
            plugin_name,
            content_hash,
        )?)),
        ArtifactKind::Component => {
            let bytes = std::fs::read(wasm_path)?;
            let component = Component::new(component_engine, &bytes).map_err(|e| anyhow!("{e:#}"))?;
            let linker = component_linker(component_engine)?;
            Ok(LoadedArtifact::Component { component, linker })
        }
    }
}
