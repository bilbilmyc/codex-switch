use serde_json::Value;

fn v1_manifest() -> toml::Value {
    toml::from_str(include_str!("../Cargo.toml")).unwrap()
}

fn v2_config() -> Value {
    serde_json::from_str(include_str!("../v2/src-tauri/tauri.conf.json")).unwrap()
}

#[test]
fn v2_preserves_the_v1_installer_identity() {
    let v1 = v1_manifest();
    let packager = &v1["package"]["metadata"]["packager"];
    let v2 = v2_config();
    let identifier = packager["identifier"].as_str().unwrap();
    // Both NSIS templates use productName for the uninstall registry key and
    // publisher/productName for the saved install directory.
    assert_eq!(
        v2["productName"],
        packager["product-name"].as_str().unwrap()
    );
    assert_eq!(v2["identifier"], identifier);
    assert_eq!(
        v2["bundle"]["publisher"],
        identifier.split('.').nth(1).unwrap()
    );
    assert_eq!(
        v2["mainBinaryName"],
        v1["package"]["name"].as_str().unwrap()
    );
    assert_eq!(
        v2["bundle"]["windows"]["nsis"]["installMode"],
        packager["nsis"]["installMode"].as_str().unwrap()
    );
}

#[test]
fn v2_release_versions_agree() {
    let manifest: toml::Value = toml::from_str(include_str!("../v2/src-tauri/Cargo.toml")).unwrap();
    let frontend: Value =
        serde_json::from_str(include_str!("../v2/frontend/package.json")).unwrap();
    let config = v2_config();
    assert_eq!(
        config["version"],
        manifest["package"]["version"].as_str().unwrap()
    );
    assert_eq!(config["version"], frontend["version"]);
}
