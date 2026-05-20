use std::path::PathBuf;

use tempfile::TempDir;
use uuid::Uuid;

use super::resolve::*;
use super::source::*;
use crate::cli::config::{path_in, Config};

fn seed_config(t: &TempDir, names: &[&str]) -> PathBuf {
    let path = path_in(t.path());
    let mut c = Config::load_or_default_at(&path).unwrap().0;
    for n in names {
        c.create_agent(n, "").unwrap();
    }
    c.save().unwrap();
    path
}

// ----- precedence + happy paths --------------------------------

#[test]
fn flag_name_resolves_to_stored_agent() {
    let t = TempDir::new().unwrap();
    let path = seed_config(&t, &["work"]);
    let r = resolve_with(
        ResolveInputs {
            agent_flag: Some("work"),
            ..Default::default()
        },
        Some(&path),
    )
    .unwrap();
    match r.source {
        AgentIdSource::NamedFlag { name, file } => {
            assert_eq!(name, "work");
            assert_eq!(file, path);
        }
        other => panic!("expected NamedFlag, got {other:?}"),
    }
}

#[test]
fn flag_id_bypasses_named_lookup() {
    let t = TempDir::new().unwrap();
    let path = path_in(t.path()); // file may not exist; OK
    let uuid = Uuid::now_v7();
    let r = resolve_with(
        ResolveInputs {
            agent_id_flag: Some(&uuid.to_string()),
            ..Default::default()
        },
        Some(&path),
    )
    .unwrap();
    assert_eq!(r.agent_id.0, uuid);
    assert_eq!(r.source, AgentIdSource::IdFlag);
}

#[test]
fn env_name_resolves_to_stored_agent() {
    let t = TempDir::new().unwrap();
    let path = seed_config(&t, &["work"]);
    let r = resolve_with(
        ResolveInputs {
            agent_env: Some("work"),
            ..Default::default()
        },
        Some(&path),
    )
    .unwrap();
    assert!(matches!(r.source, AgentIdSource::NamedEnv { .. }));
}

#[test]
fn env_id_resolves_directly() {
    let t = TempDir::new().unwrap();
    let uuid = Uuid::now_v7();
    let r = resolve_with(
        ResolveInputs {
            agent_id_env: Some(&uuid.to_string()),
            ..Default::default()
        },
        Some(&path_in(t.path())),
    )
    .unwrap();
    assert_eq!(r.agent_id.0, uuid);
    assert_eq!(r.source, AgentIdSource::IdEnv);
}

#[test]
fn bare_resolution_returns_ephemeral() {
    let t = TempDir::new().unwrap();
    // Even with a config that HAS agents, bare invocation goes
    // ephemeral — that's the locked design decision.
    let path = seed_config(&t, &["work", "demo"]);
    let r = resolve_with(ResolveInputs::default(), Some(&path)).unwrap();
    assert_eq!(r.source, AgentIdSource::Ephemeral);
    assert_ne!(r.agent_id.0, Uuid::nil());
}

#[test]
fn flag_name_overrides_env_name() {
    let t = TempDir::new().unwrap();
    let path = seed_config(&t, &["work", "demo"]);
    let r = resolve_with(
        ResolveInputs {
            agent_flag: Some("demo"),
            agent_env: Some("work"),
            ..Default::default()
        },
        Some(&path),
    )
    .unwrap();
    match r.source {
        AgentIdSource::NamedFlag { name, .. } => assert_eq!(name, "demo"),
        other => panic!("expected NamedFlag, got {other:?}"),
    }
}

// ----- error paths ---------------------------------------------

#[test]
fn flag_name_missing_errors_with_hint() {
    let t = TempDir::new().unwrap();
    let path = seed_config(&t, &["work"]);
    let err = resolve_with(
        ResolveInputs {
            agent_flag: Some("wokr"),
            ..Default::default()
        },
        Some(&path),
    )
    .unwrap_err();
    match err {
        ResolveError::UnknownNamed { name, suggestion } => {
            assert_eq!(name, "wokr");
            assert_eq!(suggestion.as_deref(), Some("work"));
        }
        other => panic!("expected UnknownNamed, got {other:?}"),
    }
}

#[test]
fn env_name_missing_errors() {
    let t = TempDir::new().unwrap();
    let path = seed_config(&t, &["work"]);
    let err = resolve_with(
        ResolveInputs {
            agent_env: Some("nope"),
            ..Default::default()
        },
        Some(&path),
    )
    .unwrap_err();
    assert!(
        matches!(err, ResolveError::UnknownNamed { .. }),
        "got {err:?}"
    );
}

#[test]
fn flag_id_invalid_uuid_errors() {
    let err = resolve_with(
        ResolveInputs {
            agent_id_flag: Some("definitely-not-a-uuid"),
            ..Default::default()
        },
        None,
    )
    .unwrap_err();
    assert!(matches!(err, ResolveError::BadFlagId(_)), "got {err:?}");
}

#[test]
fn env_id_invalid_uuid_errors() {
    let err = resolve_with(
        ResolveInputs {
            agent_id_env: Some("garbage"),
            ..Default::default()
        },
        None,
    )
    .unwrap_err();
    assert!(matches!(err, ResolveError::BadEnvId(_)), "got {err:?}");
}

#[test]
fn flag_name_and_flag_id_both_set_errors() {
    let err = resolve_with(
        ResolveInputs {
            agent_flag: Some("work"),
            agent_id_flag: Some(&Uuid::now_v7().to_string()),
            ..Default::default()
        },
        None,
    )
    .unwrap_err();
    assert!(matches!(err, ResolveError::FlagsConflict), "got {err:?}");
}

#[test]
fn env_name_and_env_id_both_set_errors() {
    let err = resolve_with(
        ResolveInputs {
            agent_env: Some("work"),
            agent_id_env: Some(&Uuid::now_v7().to_string()),
            ..Default::default()
        },
        None,
    )
    .unwrap_err();
    assert!(matches!(err, ResolveError::EnvConflict), "got {err:?}");
}

// ----- migration ----------------------------------------------

#[test]
fn legacy_singleton_migrates_and_bare_still_ephemeral() {
    let t = TempDir::new().unwrap();
    let path = path_in(t.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let legacy = "019e3b00-0000-7000-8000-000000000001";
    std::fs::write(&path, format!("agent_id = \"{legacy}\"\n")).unwrap();

    // Bare resolution returns ephemeral AND surfaces migration.
    let r = resolve_with(ResolveInputs::default(), Some(&path)).unwrap();
    assert_eq!(r.source, AgentIdSource::Ephemeral);
    let note = r.migration.as_ref().expect("migration note");
    assert_eq!(note.migrated_name, "default");

    // But the migrated `default` agent is now reachable via name.
    let r2 = resolve_with(
        ResolveInputs {
            agent_flag: Some("default"),
            ..Default::default()
        },
        Some(&path),
    )
    .unwrap();
    assert_eq!(r2.agent_id.0.to_string(), legacy);
}
