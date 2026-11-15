//! AI エージェントおよびツール連携向けの JSON Schema エクスポート。

use schemars::schema_for;

use crate::timeline::command::EditCommand;
use crate::timeline::document::ProjectDocument;

/// [`ProjectDocument`] の JSON Schema 文字列（整形済み JSON）を生成する。
///
/// # Errors
///
/// スキーマの JSON シリアライズに失敗した場合にエラーを返す。
pub fn project_document_schema_json() -> Result<String, serde_json::Error> {
    let schema = schema_for!(ProjectDocument);
    serde_json::to_string_pretty(&schema)
}

/// [`EditCommand`] の JSON Schema 文字列（整形済み JSON）を生成する。
///
/// # Errors
///
/// スキーマの JSON シリアライズに失敗した場合にエラーを返す。
pub fn edit_command_schema_json() -> Result<String, serde_json::Error> {
    let schema = schema_for!(EditCommand);
    serde_json::to_string_pretty(&schema)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_generate_valid_json_objects() -> Result<(), serde_json::Error> {
        let doc_json = project_document_schema_json()?;
        let doc_val: serde_json::Value = serde_json::from_str(&doc_json)?;
        assert!(doc_val.is_object());
        assert!(doc_json.contains("ProjectDocument"));
        assert!(doc_json.contains("PlaybackRate"));

        let cmd_json = edit_command_schema_json()?;
        let cmd_val: serde_json::Value = serde_json::from_str(&cmd_json)?;
        assert!(cmd_val.is_object());
        assert!(cmd_json.contains("EditCommand"));
        assert!(cmd_json.contains("ParameterValue"));
        assert!(cmd_json.contains("split_clip") || cmd_json.contains("SplitClip"));
        Ok(())
    }
}
