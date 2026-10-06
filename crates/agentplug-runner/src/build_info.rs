include!(concat!(env!("OUT_DIR"), "/build_info.rs"));

pub const RELEASE_RUNNER_ROOT_KEY_ID: &str = "release-20261006-2";
pub const RELEASE_RUNNER_ROOT_PUBLIC_KEY: &str =
    "7e1e7cb128556b9e56810dd485f41d3015c65e17fbea6a8c4ad4e1d0275d64eb";
pub const RELEASE_PLUGIN_ROOT_KEY_ID: &str = "plugin-release-20261006-2";
pub const RELEASE_PLUGIN_ROOT_PUBLIC_KEY: &str =
    "022fa8098b8fcbc63a63b5ff5d49d4246a5c89d8cabc14925087fcab728bf6f0";

#[derive(Clone)]
pub struct Reported {
    pub version: String,
    pub commit: String,
    pub build_ts: u64,
    pub release_build: bool,
}

pub fn is_release_build() -> bool {
    RELEASE_BUILD
}

pub fn embedded_runner_root() -> (&'static str, &'static str) {
    (RELEASE_RUNNER_ROOT_KEY_ID, RELEASE_RUNNER_ROOT_PUBLIC_KEY)
}

pub fn embedded_plugin_root() -> (&'static str, &'static str) {
    (RELEASE_PLUGIN_ROOT_KEY_ID, RELEASE_PLUGIN_ROOT_PUBLIC_KEY)
}

pub fn document() -> serde_json::Value {
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "commit": COMMIT,
        "build_ts": BUILD_TS,
        "release_build": RELEASE_BUILD,
            "embedded_runner_root": {
                "id": RELEASE_RUNNER_ROOT_KEY_ID,
                "public_key": RELEASE_RUNNER_ROOT_PUBLIC_KEY,
            },
            "embedded_plugin_root": {
                "id": RELEASE_PLUGIN_ROOT_KEY_ID,
                "public_key": RELEASE_PLUGIN_ROOT_PUBLIC_KEY,
            },
    })
}

pub fn parse(text: &str) -> Option<Reported> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    Some(Reported {
        version: value.get("version")?.as_str()?.to_string(),
        commit: value
            .get("commit")?
            .as_str()
            .unwrap_or("unknown")
            .to_string(),
        build_ts: value.get("build_ts")?.as_u64().unwrap_or(0),
        release_build: value.get("release_build")?.as_bool()?,
    })
}

pub fn probe(exe: &std::path::Path) -> Option<Reported> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--build-info");
    agentplug_host::apply_windowless(&mut cmd);
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse(&String::from_utf8_lossy(&output.stdout))
}
