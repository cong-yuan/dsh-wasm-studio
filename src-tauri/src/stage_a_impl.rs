use crate::agent_controls::{AgentSettings, MemoryFact, Record};
use crate::studio::Studio;
use anyhow::{bail, Result};
use serde_json::{json, Value};

impl Studio {
    pub(crate) fn register_control_agent(
        &self,
        id: &str,
        provider: &str,
        model: &str,
    ) -> Result<()> {
        let existing = self
            .shared
            .agent_controls
            .lock()
            .unwrap()
            .agents
            .get(id)
            .cloned();
        if let Some(record) = existing {
            let control = self
                .shared
                .agent_controls
                .lock()
                .unwrap()
                .runtime_for(&record.config);
            dsh_rs::runtime_controls::set(id, control);
            return Ok(());
        }
        let mut cfg = AgentSettings::default();
        cfg.provider = provider.to_owned();
        cfg.model = model.to_owned();
        self.store_control(id, cfg, 0)?;
        Ok(())
    }
    pub(crate) fn inherit_control_agent(&self, new_id: &str, source_id: &str) -> Result<()> {
        let source = self.control_record(source_id).config;
        self.store_control(new_id, source, 0)?;
        Ok(())
    }
    fn require_control_agent(&self, agent_id: &str) -> Result<()> {
        if agent_id.is_empty()
            || agent_id.len() > 200
            || agent_id.contains('/')
            || agent_id.contains('\\')
        {
            bail!("invalid Agent ID");
        }
        if self.agent(agent_id).is_err() && !self.session_on_disk(agent_id) {
            bail!("Agent not found");
        }
        Ok(())
    }
    fn control_agent_idle(&self, id: &str) -> Result<()> {
        self.require_control_agent(id)?;
        if self.agent(id).map(|a| a.driver_busy()).unwrap_or(false) {
            bail!("cannot update Agent controls while a turn is running");
        }
        Ok(())
    }
    fn default_agent_settings(&self, id: &str) -> AgentSettings {
        let mut cfg = AgentSettings::default();
        if let Ok(agent) = self.agent(id) {
            if let Some(header) = agent.session().request_header() {
                cfg.provider = header.config.provider;
                cfg.model = header.config.model;
                return cfg;
            }
        }
        if let Ok(events) = self.stored_events(id) {
            if !events.is_empty() {
                let options = crate::studio::last_request_options(&events);
                cfg.provider = options.provider;
                cfg.model = options.model;
                return cfg;
            }
        }
        let llm = self.llm_config();
        if let Some(provider) = llm.pointer("/current/provider").and_then(Value::as_str) {
            cfg.provider = provider.to_owned();
        }
        if let Some(model) = llm.pointer("/current/model").and_then(Value::as_str) {
            cfg.model = model.to_owned();
        }
        cfg
    }
    fn control_record(&self, id: &str) -> Record {
        self.shared
            .agent_controls
            .lock()
            .unwrap()
            .agents
            .get(id)
            .cloned()
            .unwrap_or_else(|| Record {
                revision: 0,
                config: self.default_agent_settings(id),
            })
    }
    fn store_control(&self, id: &str, config: AgentSettings, revision: u64) -> Result<Value> {
        config.validate()?;
        let record = Record { revision, config };
        let mut guard = self.shared.agent_controls.lock().unwrap();
        let mut next = guard.clone();
        next.agents.insert(id.to_owned(), record.clone());
        next.save(&self.shared.agent_controls_path)?;
        *guard = next;
        dsh_rs::runtime_controls::set(id, guard.runtime_for(&record.config));
        Ok(
            json!({ "ok": true, "agentId": id, "revision": format!("r{revision}"), "config": record.config }),
        )
    }
    pub fn get_agent_control_capabilities(&self) -> Value {
        // The requested settings are enforced at dsh-rs model/tool boundaries.
        json!({ "ok": true, "capabilities": {
            "thinkingLevel": true, "permissionMode": true, "memoryToggle": true,
            "primaryAgentSwitch": true, "agentConfigWrite": true,
        }})
    }
    // Shared memory is explicitly opt-in on *both* the source and recipient.
    // Only literal user-directed "Remember:" / "请记住：" content qualifies;
    // the model never gets to decide what silently enters the durable bank.
    pub(crate) fn capture_directed_memory(&self, agent_id: &str, text: &str) -> Result<()> {
        let trimmed = text.trim();
        let fact = ["请记住：", "记住：", "Remember:", "remember:"]
            .iter()
            .find_map(|prefix| trimmed.strip_prefix(prefix))
            .map(str::trim)
            .filter(|fact| !fact.is_empty());
        let Some(fact) = fact else {
            return Ok(());
        };
        anyhow::ensure!(fact.len() <= 900, "shared Memory fact exceeds 900 bytes");
        let mut guard = self.shared.agent_controls.lock().unwrap();
        let opt_in = guard
            .agents
            .get(agent_id)
            .map(|record| record.config.memory_enabled && record.config.shared_memory_enabled)
            .unwrap_or(false);
        if !opt_in {
            return Ok(());
        }
        if guard.shared_memories.iter().any(|entry| entry.text == fact) {
            return Ok(());
        }
        let mut next = guard.clone();
        static MEMORY_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let number = MEMORY_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        next.shared_memories.push(MemoryFact {
            id: format!("memory-{stamp}-{number}"),
            source_agent: agent_id.into(),
            text: fact.into(),
        });
        if next.shared_memories.len() > 128 {
            next.shared_memories.remove(0);
        }
        next.save(&self.shared.agent_controls_path)?;
        *guard = next;
        guard.hydrate();
        Ok(())
    }
    pub fn list_shared_memory(&self, agent_id: &str) -> Result<Value> {
        self.require_control_agent(agent_id)?;
        let guard = self.shared.agent_controls.lock().unwrap();
        let permitted = guard
            .agents
            .get(agent_id)
            .map(|record| record.config.memory_enabled && record.config.shared_memory_enabled)
            .unwrap_or(false);
        anyhow::ensure!(permitted, "shared memory is not enabled for this Agent");
        Ok(json!({ "ok": true, "agentId": agent_id, "facts": guard.shared_memories }))
    }
    pub fn delete_shared_memory(&self, agent_id: &str, fact_id: &str) -> Result<Value> {
        self.require_control_agent(agent_id)?;
        let mut guard = self.shared.agent_controls.lock().unwrap();
        let permitted = guard
            .agents
            .get(agent_id)
            .map(|record| record.config.memory_enabled && record.config.shared_memory_enabled)
            .unwrap_or(false);
        anyhow::ensure!(permitted, "shared memory is not enabled for this Agent");
        let mut next = guard.clone();
        let original = next.shared_memories.len();
        next.shared_memories.retain(|row| row.id != fact_id);
        anyhow::ensure!(
            next.shared_memories.len() + 1 == original,
            "shared memory fact not found"
        );
        next.save(&self.shared.agent_controls_path)?;
        *guard = next;
        guard.hydrate();
        Ok(json!({"ok": true, "agentId": agent_id, "id": fact_id, "deleted": true}))
    }
    pub fn pending_tool_approvals(&self, id: &str) -> Result<Value> {
        self.require_control_agent(id)?;
        let approvals = dsh_rs::runtime_controls::pending_approvals()
            .into_iter()
            .filter(|row| row.agent_id == id)
            .collect::<Vec<_>>();
        Ok(json!({"ok": true, "agentId": id, "approvals": approvals}))
    }
    pub fn decide_tool_approval(
        &self,
        id: &str,
        approval_id: &str,
        approve: bool,
    ) -> Result<Value> {
        self.require_control_agent(id)?;
        dsh_rs::runtime_controls::decide_approval(id, approval_id, approve)
            .map_err(|reason| anyhow::anyhow!(reason))?;
        Ok(json!({"ok": true, "agentId": id, "id": approval_id,
            "approved": approve}))
    }
    pub fn get_session_runtime_controls(&self, id: &str) -> Result<Value> {
        self.require_control_agent(id)?;
        let cfg = self.control_record(id).config;
        let supported = dsh_rs::runtime_controls::supports_reasoning(&cfg.model);
        Ok(
            json!({ "ok": true, "agentId": id, "level": cfg.thinking_level,
            "mode": cfg.permission_mode, "enabled": cfg.memory_enabled,
            "locked": !supported,
            "supportedLevels": if supported { vec!["off", "low", "medium", "high", "extra-high"] }
                else { vec!["off"] } }),
        )
    }
    fn mutate_control<F>(&self, id: &str, update: F) -> Result<AgentSettings>
    where
        F: FnOnce(&mut AgentSettings),
    {
        self.control_agent_idle(id)?;
        let old = self.control_record(id);
        let mut config = old.config;
        update(&mut config);
        self.store_control(id, config.clone(), old.revision + 1)?;
        Ok(config)
    }
    pub fn set_session_thinking_level(&self, id: &str, level: &str) -> Result<Value> {
        let cfg = self.mutate_control(id, |cfg| cfg.thinking_level = level.into())?;
        Ok(json!({"ok": true, "agentId": id, "level": cfg.thinking_level}))
    }
    pub fn set_session_permission_mode(&self, id: &str, mode: &str) -> Result<Value> {
        let cfg = self.mutate_control(id, |cfg| cfg.permission_mode = mode.into())?;
        Ok(json!({"ok": true, "agentId": id, "mode": cfg.permission_mode}))
    }
    pub fn set_session_memory_enabled(&self, id: &str, enabled: bool) -> Result<Value> {
        let cfg = self.mutate_control(id, |cfg| cfg.memory_enabled = enabled)?;
        Ok(json!({"ok": true, "agentId": id, "enabled": cfg.memory_enabled}))
    }
    pub fn get_primary_agent(&self) -> Result<Value> {
        let stored = self.shared.agent_controls.lock().unwrap().primary.clone();
        let mut live_ids = self
            .list_agents()
            .into_iter()
            .map(|row| row.id)
            .collect::<Vec<_>>();
        live_ids.sort();
        let selected = stored
            .filter(|id| live_ids.contains(id))
            .or_else(|| live_ids.first().cloned())
            .ok_or_else(|| anyhow::anyhow!("no primary Agent available"))?;
        Ok(json!({ "ok": true, "agentId": selected }))
    }
    pub fn switch_primary_agent(&self, id: &str) -> Result<Value> {
        self.require_control_agent(id)?;
        if self.agent(id).is_err() {
            bail!("primary Agent must be live");
        }
        let mut guard = self.shared.agent_controls.lock().unwrap();
        let mut next = guard.clone();
        next.primary = Some(id.to_string());
        next.save(&self.shared.agent_controls_path)?;
        *guard = next;
        Ok(json!({ "ok": true, "agentId": id }))
    }
    pub fn get_agent_config(&self, id: &str) -> Result<Value> {
        self.require_control_agent(id)?;
        let r = self.control_record(id);
        Ok(json!({"ok": true, "agentId": id,
            "revision": format!("r{}", r.revision), "config": r.config,
            "chat": { "provider": r.config.provider, "model": r.config.model },
            "models": { "chat": {"provider": r.config.provider, "id": r.config.model} },
            "agent": {"id": id, "name": id},
            "memory": { "enabled": r.config.memory_enabled, "shared": r.config.shared_memory_enabled },
            "capabilities": {"modelSwitch": true, "thinkingLevel": dsh_rs::runtime_controls::supports_reasoning(&r.config.model),
                "permissionMode": true, "primaryAgentSwitch": true, "memoryToggle": true, "agentConfigWrite": true},
            "source": "studio-native" }))
    }
    pub async fn patch_agent_config(
        &self,
        id: &str,
        patch: Value,
        revision: &str,
    ) -> Result<Value> {
        self.control_agent_idle(id)?;
        let old = self.control_record(id);
        if revision != format!("r{}", old.revision) {
            bail!("agent config revision conflict");
        }
        let obj = patch
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("patch must be an object"))?;
        if obj.is_empty() {
            bail!("empty config patch");
        }
        let mut cfg = old.config.clone();
        for (key, value) in obj {
            match key.as_str() {
                "provider" => {
                    cfg.provider = value
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("provider must be string"))?
                        .into()
                }
                "model" => {
                    cfg.model = value
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("model must be string"))?
                        .into()
                }
                "thinkingLevel" => {
                    cfg.thinking_level = value
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("thinkingLevel must be string"))?
                        .into()
                }
                "permissionMode" => {
                    cfg.permission_mode = value
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("permissionMode must be string"))?
                        .into()
                }
                "memoryEnabled" => {
                    cfg.memory_enabled = value
                        .as_bool()
                        .ok_or_else(|| anyhow::anyhow!("memoryEnabled must be boolean"))?
                }
                "memoryNotes" => {
                    cfg.memory_notes = value
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("memoryNotes must be string"))?
                        .into()
                }
                "sharedMemoryEnabled" => {
                    cfg.shared_memory_enabled = value
                        .as_bool()
                        .ok_or_else(|| anyhow::anyhow!("sharedMemoryEnabled must be boolean"))?
                }
                _ => bail!("unsupported per-Agent config key: {key}"),
            }
        }
        cfg.validate()?;
        if old.config.provider != cfg.provider || old.config.model != cfg.model {
            self.rebind_agent_model(id, cfg.provider.clone(), cfg.model.clone())
                .await?;
        }
        // Recheck after a long model rebind; another writer may have updated it.
        if self.control_record(id).revision != old.revision {
            bail!("agent config revision changed while rebinding");
        }
        self.store_control(id, cfg, old.revision + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    #[tokio::test]
    async fn explicit_shared_memory_opt_in_capture_and_deletion_are_scoped() {
        let path = std::env::temp_dir().join(format!("studio-memory-optin-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&path);
        let studio = Studio::with_hook(None, None, path.clone()).await.unwrap();
        for id in ["memory-source-test", "memory-recipient-test"] {
            studio
                .create_agent(Some(id.into()), "mock".into(), "mock-1".into(), None)
                .unwrap();
        }
        let source = "memory-source-test";
        let recipient = "memory-recipient-test";
        let r1 = studio.get_agent_config(source).unwrap()["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        studio
            .patch_agent_config(
                source,
                json!({"memoryEnabled":true, "sharedMemoryEnabled":true}),
                &r1,
            )
            .await
            .unwrap();
        studio
            .capture_directed_memory(source, "ordinary user conversation")
            .unwrap();
        assert_eq!(
            studio.list_shared_memory(source).unwrap()["facts"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        studio
            .capture_directed_memory(source, "请记住：使用简体中文回复")
            .unwrap();
        studio
            .capture_directed_memory(source, "请记住：使用简体中文回复")
            .unwrap();
        let facts = studio.list_shared_memory(source).unwrap();
        assert_eq!(facts["facts"].as_array().unwrap().len(), 1);
        let fact_id = facts["facts"][0]["id"].as_str().unwrap();
        assert!(
            studio.list_shared_memory(recipient).is_err(),
            "new Agent is private by default"
        );
        assert!(dsh_rs::runtime_controls::get(recipient)
            .shared_notes
            .is_empty());
        let r2 = studio.get_agent_config(recipient).unwrap()["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        studio
            .patch_agent_config(
                recipient,
                json!({"memoryEnabled":true,"sharedMemoryEnabled":true}),
                &r2,
            )
            .await
            .unwrap();
        assert!(dsh_rs::runtime_controls::get(recipient)
            .shared_notes
            .contains("简体中文"));
        assert_eq!(
            studio.list_shared_memory(recipient).unwrap()["facts"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(studio
            .delete_shared_memory(recipient, "memory-invalid")
            .is_err());
        studio.delete_shared_memory(recipient, fact_id).unwrap();
        assert!(studio.list_shared_memory(recipient).unwrap()["facts"]
            .as_array()
            .unwrap()
            .is_empty());
        assert!(dsh_rs::runtime_controls::get(recipient)
            .shared_notes
            .is_empty());
        let persisted =
            crate::agent_controls::ControlStore::load(&path.join("agent-controls.json")).unwrap();
        assert!(persisted.shared_memories.is_empty());
        for id in [source, recipient] {
            let _ = studio.dispose_agent(id);
        }
        let _ = std::fs::remove_dir_all(path);
    }

    #[tokio::test]
    async fn stage_a_controls_persist_and_enforce_execution() {
        let path: PathBuf =
            std::env::temp_dir().join(format!("studio-phase-a-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&path);
        let studio = Studio::with_hook(None, None, path.clone()).await.unwrap();
        let id = "phase-a-control-test";
        studio
            .create_agent(Some(id.to_string()), "mock".into(), "mock-1".into(), None)
            .unwrap();
        let initial = studio.get_session_runtime_controls(id).unwrap();
        assert_eq!(initial["mode"], "ask");
        assert_eq!(
            dsh_rs::runtime_controls::tool_denial(id, "bash").unwrap().0,
            "APPROVAL"
        );
        studio.set_session_permission_mode(id, "read_only").unwrap();
        assert_eq!(
            dsh_rs::runtime_controls::tool_denial(id, "write_file")
                .unwrap()
                .0,
            "DENIED"
        );
        assert_eq!(dsh_rs::runtime_controls::tool_denial(id, "read_file"), None);
        studio.set_session_memory_enabled(id, true).unwrap();
        assert!(dsh_rs::runtime_controls::get(id).memory_enabled);
        assert!(
            studio.set_session_thinking_level(id, "high").is_err(),
            "mock models cannot advertise model reasoning"
        );
        assert!(studio
            .set_session_permission_mode("../../other", "operate")
            .is_err());
        let original = studio.get_agent_config(id).unwrap();
        assert_eq!(original["config"]["memoryEnabled"], true);
        let expected = original["revision"].as_str().unwrap();
        let update = studio
            .patch_agent_config(
                id,
                json!({"memoryNotes":"Remember preferred language: Chinese"}),
                expected,
            )
            .await
            .unwrap();
        assert_ne!(update["revision"], original["revision"]);
        assert!(studio
            .patch_agent_config(id, json!({"memoryNotes":"stale"}), expected)
            .await
            .is_err());
        assert!(dsh_rs::runtime_controls::get(id)
            .memory_notes
            .contains("Chinese"));
        assert_eq!(studio.switch_primary_agent(id).unwrap()["agentId"], id);
        assert_eq!(studio.get_primary_agent().unwrap()["agentId"], id);
        let persisted =
            crate::agent_controls::ControlStore::load(&path.join("agent-controls.json")).unwrap();
        assert_eq!(persisted.primary.as_deref(), Some(id));
        assert_eq!(
            persisted.agents.get(id).unwrap().config.memory_notes,
            "Remember preferred language: Chinese"
        );
        let _ = studio.dispose_agent(id);
        // Preserve disk state for verification above, then clean the test directory.
        let _ = std::fs::remove_dir_all(&path);
    }
}
