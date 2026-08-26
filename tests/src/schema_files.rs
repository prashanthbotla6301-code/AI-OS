//! Keeps the shipped JSON Schema files in sync with the crate's canonical
//! permission list and required manifest fields.

use serde_json::Value;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests is below repository root")
        .to_path_buf()
}

#[test]
fn manifest_schema_matches_crate_contract() {
    let path = repo_root().join("packages/schema/manifest-v1.schema.json");
    let schema: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(schema["properties"]["format"]["const"], "agentos");
    let required = schema["required"].as_array().unwrap();
    for field in ["format", "format_version", "id", "name", "version"] {
        assert!(
            required.iter().any(|v| v == field),
            "manifest schema must require {field}"
        );
    }
    // permission enum matches the crate's 11 permissions
    let perm_enum = schema["definitions"]["permissionRequest"]["properties"]["permission"]["enum"]
        .as_array()
        .unwrap();
    let crate_perms = agentos_schema::permission::ALL;
    assert_eq!(
        perm_enum.len(),
        crate_perms.len(),
        "schema and crate permission lists drifted"
    );
    for p in crate_perms {
        assert!(
            perm_enum.iter().any(|v| v == p),
            "schema missing permission {p}"
        );
    }
    let engine_enum = schema["properties"]["runtime"]["properties"]["engine"]["enum"]
        .as_array()
        .unwrap();
    for e in agentos_schema::engine::ALL {
        assert!(
            engine_enum.iter().any(|v| v == e),
            "schema missing engine {e}"
        );
    }
}

#[test]
fn permissions_schema_matches_crate_contract() {
    let path = repo_root().join("packages/schema/permissions-v1.schema.json");
    let schema: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(schema["required"][0], "permissions");
    let perm_enum = schema["properties"]["permissions"]["items"]["properties"]["permission"]
        ["enum"]
        .as_array()
        .unwrap();
    assert_eq!(perm_enum.len(), agentos_schema::permission::ALL.len());
}

#[test]
fn example_packages_validate() {
    let root = repo_root();
    for pkg in ["hello.agent", "net-demo.agent"] {
        let dir = root.join("packages/examples").join(pkg);
        let report = agentos_schema::validate_package_dir(&dir);
        assert!(report.valid(), "{pkg} must validate: {:?}", report.errors);
    }
}

#[test]
fn shipped_policy_is_deny_by_default() {
    let root = repo_root();
    let policy: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("rootfs/etc/agentos/permissions-default.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(policy["default"], "deny");
    for p in agentos_schema::permission::ALL {
        assert_eq!(
            policy["permissions"][p]["default"], "deny",
            "{p} must ship denied"
        );
    }
}
