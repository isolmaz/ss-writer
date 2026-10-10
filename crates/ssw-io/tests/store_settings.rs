//! `settings.json`: loading, unknown-key preservation, atomic saving, corrupt-file backup.

use std::io;
use std::path::PathBuf;

use serde_json::{Value, json};
use ssw_core::settings::Settings;
use ssw_io::settings_file::{SettingsFileError, load_settings, save_settings};
use tempfile::tempdir;

fn backup_of(path: &std::path::Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".bak");
    PathBuf::from(name)
}

#[test]
fn a_missing_file_yields_the_defaults_and_creates_nothing() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("settings.json");
    let loaded = load_settings(&path).unwrap();
    assert_eq!(loaded.settings, Settings::default());
    assert_eq!(loaded.backup, None);
    assert!(!path.exists());
}

#[test]
fn empty_and_bom_prefixed_files_load_without_a_backup() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("settings.json");

    std::fs::write(&path, b"  \r\n").unwrap();
    let empty = load_settings(&path).unwrap();
    assert_eq!(empty.settings, Settings::default());
    assert_eq!(empty.backup, None);

    std::fs::write(&path, b"\xEF\xBB\xBF{\"general\": {\"language\": \"tr\"}}").unwrap();
    let bom = load_settings(&path).unwrap();
    assert_eq!(bom.settings.general.language.as_deref(), Some("tr"));
    assert_eq!(bom.backup, None);
    assert!(path.exists());
}

#[test]
fn settings_round_trip_through_the_file() {
    let tmp = tempdir().unwrap();
    // The folder is created on demand.
    let path = tmp
        .path()
        .join("roaming")
        .join("SS Writer")
        .join("settings.json");
    let mut settings = Settings::default();
    settings.general.language = Some("tr".to_owned());
    settings.general.check_for_updates = false;
    settings.advanced.history_retention_days = 7;
    save_settings(&path, &settings).unwrap();

    let loaded = load_settings(&path).unwrap();
    assert_eq!(loaded.settings, settings);
    assert_eq!(loaded.backup, None);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.ends_with('\n'));
    // Human-editable: pretty-printed over several lines.
    assert!(text.lines().count() > 10);
    // No temporary file is left behind.
    assert!(
        !tmp.path()
            .join("roaming")
            .join("SS Writer")
            .join("settings.json.tmp")
            .exists()
    );
}

#[test]
fn unknown_keys_survive_a_load_modify_save_cycle() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("settings.json");
    let original = json!({
        "general": { "language": "tr", "future_general_key": [1, 2, 3] },
        "advanced": { "history_retention_days": 7, "experimental": { "flag": true } },
        "from_the_future": { "a": 1, "b": ["x", null] }
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&original).unwrap()).unwrap();

    let mut loaded = load_settings(&path).unwrap();
    assert_eq!(loaded.settings.general.language.as_deref(), Some("tr"));
    assert_eq!(loaded.settings.advanced.history_retention_days, 7);
    loaded.settings.general.check_for_updates = false;
    save_settings(&path, &loaded.settings).unwrap();

    let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["general"]["future_general_key"], json!([1, 2, 3]));
    assert_eq!(saved["advanced"]["experimental"], json!({ "flag": true }));
    assert_eq!(saved["from_the_future"], original["from_the_future"]);
    assert_eq!(saved["general"]["language"], json!("tr"));
    assert_eq!(saved["general"]["check_for_updates"], json!(false));
    assert_eq!(saved["advanced"]["history_retention_days"], json!(7));
}

#[test]
fn corrupt_files_are_backed_up_and_replaced_by_the_defaults() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("settings.json");
    let backup = backup_of(&path);

    // Hand-editing mistake: the text is kept in the backup.
    let broken = "{ \"general\": { \"language\": \"tr\", }";
    std::fs::write(&path, broken).unwrap();
    let loaded = load_settings(&path).unwrap();
    assert_eq!(loaded.settings, Settings::default());
    assert_eq!(loaded.backup.as_deref(), Some(backup.as_path()));
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), broken);
    assert!(!path.exists());

    // A known key with a value of the wrong type is corrupt too; the older backup is replaced.
    let wrong_type = "{\"general\": {\"check_for_updates\": \"yes\"}}";
    std::fs::write(&path, wrong_type).unwrap();
    let loaded = load_settings(&path).unwrap();
    assert_eq!(loaded.settings, Settings::default());
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), wrong_type);

    // Bytes that are not UTF-8 at all.
    std::fs::write(&path, [0xFF, 0xFE, 0x00, 0x80]).unwrap();
    let loaded = load_settings(&path).unwrap();
    assert_eq!(loaded.backup.as_deref(), Some(backup.as_path()));
    assert_eq!(std::fs::read(&backup).unwrap(), [0xFF, 0xFE, 0x00, 0x80]);

    // After that, saving writes a fresh file next to the backup.
    save_settings(&path, &loaded.settings).unwrap();
    assert_eq!(load_settings(&path).unwrap().settings, Settings::default());
}

#[test]
fn an_unreadable_settings_path_is_an_io_error() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("settings.json");
    // A folder where the file should be: reading fails with something other than "not found".
    std::fs::create_dir(&path).unwrap();
    let error = load_settings(&path).unwrap_err();
    assert!(
        matches!(&error, SettingsFileError::Io { path: failed, .. } if failed == &path),
        "{error:?}"
    );
    assert!(error.to_string().contains("settings.json"));
}

#[test]
fn a_corrupt_file_that_cannot_be_moved_is_an_io_error() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("settings.json");
    std::fs::write(&path, "not json").unwrap();
    // The backup name is taken by a folder, so the rename cannot succeed.
    std::fs::create_dir(backup_of(&path)).unwrap();
    let error = load_settings(&path).unwrap_err();
    assert!(matches!(error, SettingsFileError::Io { .. }), "{error:?}");
    // The user's text is still where it was.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
}

#[test]
fn saving_reports_the_step_that_failed_and_cleans_up() {
    let tmp = tempdir().unwrap();
    let settings = Settings::default();

    // The parent "folder" is a file.
    let blocker = tmp.path().join("blocker");
    std::fs::write(&blocker, "x").unwrap();
    let error = save_settings(&blocker.join("settings.json"), &settings).unwrap_err();
    assert!(
        matches!(&error, SettingsFileError::Io { path, .. } if path == &blocker),
        "{error:?}"
    );

    // The temporary file name is taken by a folder.
    let path = tmp.path().join("settings.json");
    std::fs::create_dir(tmp.path().join("settings.json.tmp")).unwrap();
    let error = save_settings(&path, &settings).unwrap_err();
    assert!(matches!(error, SettingsFileError::Io { .. }), "{error:?}");
    std::fs::remove_dir(tmp.path().join("settings.json.tmp")).unwrap();

    // The target is a folder: the final rename fails and the temporary file is removed.
    let target = tmp.path().join("as_folder");
    std::fs::create_dir(&target).unwrap();
    let error = save_settings(&target, &settings).unwrap_err();
    assert!(
        matches!(&error, SettingsFileError::Io { path, .. } if path == &target),
        "{error:?}"
    );
    assert!(!tmp.path().join("as_folder.tmp").exists());
}

#[test]
fn settings_errors_describe_themselves() {
    let io_error = SettingsFileError::Io {
        path: PathBuf::from("C:\\x\\settings.json"),
        source: io::Error::other("denied"),
    };
    assert!(io_error.to_string().contains("denied"));
    let parse = Settings::from_json_str("{").unwrap_err();
    let wrapped = SettingsFileError::from(parse);
    assert!(wrapped.to_string().contains("not valid"));
}
