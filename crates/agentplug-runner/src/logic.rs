use std::path::Path;

use anyhow::{Context, Result};
use mlua::{Lua, LuaSerdeExt};

pub struct LogicHost {
    lua: Lua,
}

fn lua_err(err: mlua::Error) -> anyhow::Error {
    anyhow::anyhow!("{err}")
}

impl LogicHost {
    pub fn load(dir: &Path) -> Result<Self> {
        let lua = Lua::new();
        let mut entries = std::fs::read_dir(dir)
            .with_context(|| format!("read {}", dir.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("lua") {
                continue;
            }
            let src = std::fs::read_to_string(&path)?;
            lua.load(src.as_str())
                .set_name(path.display().to_string())
                .exec()
                .map_err(lua_err)
                .with_context(|| format!("load {}", path.display()))?;
        }
        Ok(Self { lua })
    }

    pub fn call(&self, function: &str, input: &serde_json::Value) -> Result<serde_json::Value> {
        let f: mlua::Function = self
            .lua
            .globals()
            .get(function)
            .map_err(lua_err)
            .with_context(|| format!("lua function {function} not defined"))?;
        let arg = self.lua.to_value(input).map_err(lua_err)?;
        let out: mlua::Value = f.call(arg).map_err(lua_err)?;
        self.lua.from_value(out).map_err(lua_err)
    }
}
