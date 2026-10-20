use std::path::PathBuf;

use agentplug_host::component::build_component_engine;
use agentplug_host::loader::{artifact_kind, load_selected_artifact};

const CORE_MODULE: [u8; 8] = [0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
const EMPTY_COMPONENT: [u8; 8] = [0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00];
const UNKNOWN_VERSION: [u8; 8] = [0x00, 0x61, 0x73, 0x6d, 0x02, 0x00, 0x00, 0x00];

fn main() -> anyhow::Result<()> {
    let out_dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or_else(|| anyhow::anyhow!("usage: loader_witness <out-dir>"))?,
    );
    let artifacts = out_dir.join("artifacts");
    std::fs::create_dir_all(&artifacts)?;
    let core_engine = agentplug_host::build_engine()?;
    let component_engine = build_component_engine()?;
    let cases: [(&str, &[u8]); 3] = [
        ("core.wasm", &CORE_MODULE),
        ("component.wasm", &EMPTY_COMPONENT),
        ("unknown.wasm", &UNKNOWN_VERSION),
    ];
    let mut rows = Vec::new();
    for (name, bytes) in cases {
        let path = artifacts.join(name);
        std::fs::write(&path, bytes)?;
        let header_kind = artifact_kind(bytes).map(|k| format!("{k:?}")).unwrap_or_else(|e| format!("{e:#}"));
        let row = match load_selected_artifact(&core_engine, &component_engine, &path, "loader-witness", name) {
            Ok(loaded) => serde_json::json!({
                "artifact": name,
                "kind": format!("{:?}", loaded.kind()),
                "linker": loaded.linker_name(),
                "result": "loaded",
            }),
            Err(e) => serde_json::json!({
                "artifact": name,
                "kind": header_kind,
                "linker": null,
                "result": "refused",
                "error": format!("{e:#}"),
            }),
        };
        rows.push(row);
    }
    let report = serde_json::json!({ "witness": "loader-selection", "rows": rows });
    let report_path = out_dir.join("loader-witness.json");
    std::fs::write(&report_path, serde_json::to_string_pretty(&report)?)?;
    println!("{}", report_path.display());
    Ok(())
}
