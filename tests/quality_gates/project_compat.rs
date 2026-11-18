//! `.smc.json`（`schema_version = 1`）後方互換性のゴールデンフィクスチャ検証テスト。

use sea_movie_creator::timeline::serialization::{
    deserialize_project_from_str, serialize_project_to_string,
};

const LEGACY_V1_FIXTURE: &str = include_str!("fixtures/v1_legacy_project.smc.json");

#[test]
fn legacy_v1_project_fixture_deserializes_validates_and_roundtrips() -> Result<(), String> {
    let doc = deserialize_project_from_str(LEGACY_V1_FIXTURE)
        .map_err(|err| format!("failed to load legacy v1 fixture: {err}"))?;

    assert_eq!(doc.schema_version, 1);
    assert_eq!(doc.assets.len(), 3);
    assert_eq!(doc.layers.len(), 3);
    assert_eq!(doc.total_duration_frames(), 300);
    assert!(doc.selection_range.is_some());

    let reserialized = serialize_project_to_string(&doc)
        .map_err(|err| format!("failed to reserialize legacy v1 fixture: {err}"))?;
    let reloaded = deserialize_project_from_str(&reserialized)
        .map_err(|err| format!("failed to reload reserialized v1 fixture: {err}"))?;
    assert_eq!(doc, reloaded);
    Ok(())
}

#[test]
fn minimal_v1_project_json_uses_backward_compatible_defaults() -> Result<(), String> {
    let minimal_json = r#"{"name":"Minimal Legacy Project"}"#;
    let doc = deserialize_project_from_str(minimal_json)
        .map_err(|err| format!("minimal JSON must deserialize with serde defaults: {err}"))?;

    assert_eq!(doc.schema_version, 1);
    assert_eq!(doc.name, "Minimal Legacy Project");
    assert_eq!(doc.assets, Vec::new());
    assert_eq!(doc.layers, Vec::new());
    Ok(())
}
