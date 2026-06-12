// Verify that packaging files stay in sync with Cargo.toml.
// A version bump to Cargo.toml that forgets to update snapcraft.yaml or
// the Arch PKGBUILD will fail CI here rather than shipping a mislabelled package.
#[test]
fn packaging_versions_match_cargo() {
    let v = env!("CARGO_PKG_VERSION");
    assert!(
        include_str!("../snap/snapcraft.yaml").contains(&format!("version: '{v}'")),
        "snap/snapcraft.yaml version != Cargo.toml version {v}",
    );
    assert!(
        include_str!("../packaging/arch/PKGBUILD").contains(&format!("pkgver={v}")),
        "packaging/arch/PKGBUILD pkgver != Cargo.toml version {v}",
    );
    assert!(
        include_str!("../packaging/arch/.SRCINFO").contains(&format!("pkgver = {v}")),
        "packaging/arch/.SRCINFO pkgver != Cargo.toml version {v}",
    );
}
