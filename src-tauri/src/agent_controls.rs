//! Persistent Studio-owned settings; dsh-rs consumes execution snapshots from
//! this module at the actual LLM / tool boundaries. Never trust an iframe's
//! claim that a mutation succeeded without checking the backend result.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentSettings {
    pub provider: String,
    pub model: String,
    pub thinking_level: String,
    pub permission_mode: String,
    pub memory_enabled: bool,
    pub memory_notes: String,
}
impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            provider: "mock".into(),
            model: "mock-1".into(),
            thinking_level: "off".into(),
            permission_mode: "ask".into(),
            memory_enabled: false,
            memory_notes: String::new(),
        }
    }
}
impl AgentSettings {
    pub fn live(&self) -> dsh_rs::runtime_controls::RuntimeControls {
        dsh_rs::runtime_controls::RuntimeControls {
            thinking_level: self.thinking_level.clone(),
            permission_mode: self.permission_mode.clone(),
            memory_enabled: self.memory_enabled,
            memory_notes: self.memory_notes.clone(),
        }
    }
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.provider.trim().is_empty() && self.provider.len() <= 120,
            "invalid provider"
        );
        anyhow::ensure!(
            !self.model.trim().is_empty() && self.model.len() <= 200,
            "invalid model"
        );
        anyhow::ensure!(
            matches!(
                self.thinking_level.as_str(),
                "off" | "low" | "medium" | "high" | "extra-high"
            ),
            "invalid thinking level"
        );
        anyhow::ensure!(
            matches!(
                self.permission_mode.as_str(),
                "ask" | "read_only" | "operate" | "auto"
            ),
            "invalid permission mode"
        );
        anyhow::ensure!(self.memory_notes.len() <= 65536, "memory notes too long");
        if self.thinking_level != "off" {
            anyhow::ensure!(
                dsh_rs::runtime_controls::supports_reasoning(&self.model),
                "reasoning effort unsupported for this model"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub revision: u64,
    pub config: AgentSettings,
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlStore {
    pub primary: Option<String>,
    pub agents: HashMap<String, Record>,
}
impl ControlStore {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let data = std::fs::read(path).context("read agent controls")?;
        let settings: Self = serde_json::from_slice(&data).context("parse agent controls")?;
        for record in settings.agents.values() {
            record.config.validate()?;
        }
        Ok(settings)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        let temp = path.with_extension("json.new");
        std::fs::write(&temp, serde_json::to_vec_pretty(self)?).context("write agent controls")?;
        std::fs::rename(&temp, path).context("commit agent controls")?;
        Ok(())
    }
    pub fn hydrate(&self) {
        for (id, record) in &self.agents {
            dsh_rs::runtime_controls::set(id, record.config.live());
        }
    }
}
