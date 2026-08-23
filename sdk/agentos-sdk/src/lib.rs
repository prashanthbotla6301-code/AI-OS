//! AgentOS Agent SDK — the library third-party developers use to produce
//! `.agent` packages without hand-writing JSON.
//!
//! Phase 1 scope: manifest/packaging construction + validation (the same
//! validator the OS package manager uses). Phase 2 adds the runtime SDK
//! (agent traits, bus client, tool definitions) that runs *inside* agents.

use agentos_schema::{Manifest, PermissionRequest, ValidationReport};
use std::fs;
use std::path::Path;

pub struct AgentBuilder {
    manifest: Manifest,
    permissions_file: Vec<PermissionRequest>,
    agent_json: serde_json::Value,
}

impl AgentBuilder {
    pub fn new(id: &str, name: &str, version: &str) -> Result<AgentBuilder, String> {
        if !agentos_schema::permission::is_known("network") {
            unreachable!()
        }
        let _ = id;
        Ok(AgentBuilder {
            manifest: serde_json::from_value(serde_json::json!({
                "format": "agentos",
                "format_version": "1.0.0",
                "id": id,
                "name": name,
                "version": version,
                "developer": {},
                "runtime": {"engine": "mock"},
                "permissions": [],
                "capabilities": [],
            }))
            .map_err(|e: serde_json::Error| e.to_string())?,
            permissions_file: Vec::new(),
            agent_json: serde_json::json!({
                "entry": {"kind": "builtin:echo"},
                "interface": [],
                "sandbox": {"fs": [], "network": false, "exec": false},
            }),
        })
    }

    pub fn description(mut self, d: &str) -> Self {
        self.manifest.description = Some(d.into());
        self
    }
    pub fn developer(mut self, name: &str, url: Option<&str>) -> Self {
        self.manifest.developer = agentos_schema::Developer {
            name: Some(name.into()),
            url: url.map(str::to_string),
            email: None,
        };
        self
    }
    pub fn engine(mut self, engine: &str) -> Self {
        self.manifest.runtime.engine = Some(engine.into());
        self
    }
    pub fn capability(mut self, c: &str) -> Self {
        self.manifest.capabilities.push(c.into());
        self
    }
    /// Request a permission (adds it to both manifest and permissions.json).
    pub fn request_permission(mut self, permission: &str, reason: &str) -> Result<Self, String> {
        if !agentos_schema::permission::is_known(permission) {
            return Err(format!("unknown permission {permission:?}"));
        }
        let req = PermissionRequest {
            permission: permission.into(),
            access: None,
            resource: None,
            reason: Some(reason.into()),
        };
        self.manifest.permissions.push(req.clone());
        self.permissions_file.push(req);
        Ok(self)
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn validate(&self) -> ValidationReport {
        agentos_schema::validate_manifest(&self.manifest)
    }

    /// Write the package directory (manifest.json, agent.json, permissions.json
    /// + contract subdirectories with READMEs).
    pub fn build_dir(&self, out: &Path) -> Result<ValidationReport, String> {
        fs::create_dir_all(out).map_err(|e| e.to_string())?;
        let report = self.validate();
        fs::write(
            out.join("manifest.json"),
            serde_json::to_string_pretty(&self.manifest).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        fs::write(
            out.join("agent.json"),
            serde_json::to_string_pretty(&self.agent_json).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        fs::write(
            out.join("permissions.json"),
            serde_json::to_string_pretty(&agentos_schema::PermissionsFile {
                permissions: self.permissions_file.clone(),
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        for d in [
            "runtime",
            "tools",
            "skills",
            "workflows",
            "models",
            "ui",
            "assets",
        ] {
            let p = out.join(d);
            fs::create_dir_all(&p).map_err(|e| e.to_string())?;
            fs::write(p.join("README.md"), format!("# {d}/\nPart of the .agent package contract. See docs/agent-format.md in the AgentOS repository.\n")).map_err(|e| e.to_string())?;
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_produces_valid_package() {
        let dir = std::env::temp_dir().join(format!("agentos-sdk-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let b = AgentBuilder::new("com.example.video", "Video Agent", "0.1.0")
            .unwrap()
            .description("edits videos")
            .developer("Example", Some("https://example.com"))
            .capability("video.edit")
            .request_permission("filesystem.read", "read source clips")
            .unwrap();
        let report = b.build_dir(&dir).unwrap();
        assert!(report.valid(), "{:?}", report.errors);
        let dir_report = agentos_schema::validate_package_dir(&dir);
        assert!(dir_report.valid(), "{:?}", dir_report.errors);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_permission_rejected_at_build_time() {
        let b = AgentBuilder::new("com.example.bad", "Bad", "1.0.0").unwrap();
        assert!(b.request_permission("root.everything", "nope").is_err());
    }
}
