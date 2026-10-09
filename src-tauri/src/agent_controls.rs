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
    pub shared_memory_enabled: bool,
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
            shared_memory_enabled: false,
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
            shared_notes: String::new(),
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryFact {
    pub id: String,
    pub source_agent: String,
    pub text: String,
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlStore {
    pub primary: Option<String>,
    pub agents: HashMap<String, Record>,
    pub shared_memories: Vec<MemoryFact>,
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
        anyhow::ensure!(
            settings.shared_memories.len() <= 128,
            "too many shared memories"
        );
        anyhow::ensure!(
            settings
                .shared_memories
                .iter()
                .all(|row| row.id.starts_with("memory-")
                    && !row.text.is_empty()
                    && row.text.len() <= 900),
            "invalid shared memory record"
        );
        Ok(settings)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        let temp = path.with_extension("json.new");
        std::fs::write(&temp, serde_json::to_vec_pretty(self)?).context("write agent controls")?;
        std::fs::rename(&temp, path).context("commit agent controls")?;
        Ok(())
    }
    pub fn runtime_for(
        &self,
        settings: &AgentSettings,
    ) -> dsh_rs::runtime_controls::RuntimeControls {
        let mut controls = settings.live();
        if settings.memory_enabled && settings.shared_memory_enabled {
            controls.shared_notes = self
                .shared_memories
                .iter()
                .rev()
                .take(30)
                .map(|entry| format!("- {}", entry.text))
                .collect::<Vec<_>>()
                .join("\n");
            if controls.shared_notes.len() > 12000 {
                controls.shared_notes = controls.shared_notes.chars().take(3500).collect();
            }
        }
        controls
    }
    pub fn hydrate(&self) {
        for (id, record) in &self.agents {
            dsh_rs::runtime_controls::set(id, self.runtime_for(&record.config));
        }
    }
}
