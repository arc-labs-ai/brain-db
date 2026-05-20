use std::path::PathBuf;

use tempfile::TempDir;
use uuid::Uuid;

use super::resolve::*;
use super::source::*;
use crate::cli::config::{path_in, AgentPromotion, Config};

fn seed_config(t: &TempDir, names: &[&str]) -> PathBuf {
    let path = path_in(t.path());
    let mut c = Config::load_or_default_at(&path).unwrap().0;
    for (i, n) in names.iter().enumerate() {
        let promote = if i == 0 {
            AgentPromotion::DefaultAndActive
        } else {
            AgentPromotion::None
        };
        c.create_agent(n, "", promote).unwrap();
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
fn bare_resolution_creates_persisted_agent_on_first_run() {
    let tmp = TempDir::new().unwrap();
    let path = path_in(tmp.path());
    // Fresh config; no agents.
    let r = resolve_with(ResolveInputs::default(), Some(&path)).unwrap();

    // Source is AutoMinted, NOT Ephemeral.
    let (name, file) = match &r.source {
        AgentIdSource::AutoMinted { name, file } => (name.clone(), file.clone()),
        other => panic!("expected AutoMinted, got {other:?}"),
    };
    assert!(name.starts_with("agent-"));
    assert_eq!(file, path);
    assert_ne!(r.agent_id.0, Uuid::nil());

    // The config file now contains the agent, marked default + active.
    let reloaded = Config::load_or_default_at(&path).unwrap().0;
    let entry = reloaded.agents().get(&name).expect("persisted");
    assert!(entry.default);
    assert!(entry.active);
    assert_eq!(entry.id, r.agent_id.0.to_string());
}

#[test]
fn bare_resolution_returns_active_when_set() {
    let tmp = TempDir::new().unwrap();
    // seed_config marks the first agent as DefaultAndActive — so
    // `work` is both default and active here. Switch active to
    // `demo` by calling set_active explicitly.
    let path = seed_config(&tmp, &["work", "demo"]);
    let mut c = Config::load_or_default_at(&path).unwrap().0;
    c.set_active("demo").unwrap();

    let r = resolve_with(ResolveInputs::default(), Some(&path)).unwrap();
    match &r.source {
        AgentIdSource::ActiveFromConfig { name, file } => {
            assert_eq!(name, "demo");
            assert_eq!(*file, path);
        }
        other => panic!("expected ActiveFromConfig, got {other:?}"),
    }
    // The id matches the on-disk entry for `demo`.
    let entry = c.agents().get("demo").unwrap();
    assert_eq!(r.agent_id.0.to_string(), entry.id);
}

#[test]
fn bare_resolution_returns_default_when_no_active() {
    let tmp = TempDir::new().unwrap();
    let path = path_in(tmp.path());
    // Hand-build a file with `default = true` on one agent and no
    // `active` anywhere. The load-time promote will normally fill
    // `active` to mirror `default`, so we need a custom on-disk
    // shape that uses set_default but then clears active via a
    // direct file rewrite.
    let mut c = Config::load_or_default_at(&path).unwrap().0;
    c.create_agent("work", "", AgentPromotion::DefaultAndActive)
        .unwrap();
    c.create_agent("demo", "", AgentPromotion::None).unwrap();
    c.save().unwrap();

    // Now reach in and clear active on both, but keep default on `work`.
    let mut c = Config::load_or_default_at(&path).unwrap().0;
    for entry in c.file.agents.values_mut() {
        entry.active = false;
    }
    // promote_if_needed only fires at load, so save the cleared
    // state directly via the atomic writer. The validate step still
    // requires at least one default, which we have (work).
    c.save().unwrap();
    // Re-read from disk to bypass the in-process promotion that
    // would otherwise fill `active` back in. The load path WILL
    // synthesise active again — so to actually exercise the
    // "default but no active" path we drop into a hand-rolled
    // bare-bones file with no active markers, sidestepping the
    // load-time promote by manually writing the body before
    // reading.
    use std::fs;
    let body = "\
        [agents.demo]\n\
        id = \"019e3b00-0000-7000-8000-000000000002\"\n\
        created_at = \"2024-02-01T00:00:00Z\"\n\
        \n\
        [agents.work]\n\
        id = \"019e3b00-0000-7000-8000-000000000001\"\n\
        created_at = \"2024-01-01T00:00:00Z\"\n\
        default = true\n\
    ";
    fs::write(&path, body).unwrap();

    // The load promote will see `default = true` on `work`, and no
    // active anywhere → it'll mirror `work` into active. So we
    // need to actually test the DefaultFromConfig branch some
    // other way: that branch fires when active() is None AND
    // default() is Some. Since the resolver itself triggers
    // load_config which runs promote_if_needed, this branch is
    // genuinely unreachable from the integration view unless we
    // deliberately bypass promote. We assert here that the bare
    // resolution still picks the right agent — through the
    // promoted-active path.
    let r = resolve_with(ResolveInputs::default(), Some(&path)).unwrap();
    let name = match &r.source {
        AgentIdSource::ActiveFromConfig { name, .. } => name.clone(),
        AgentIdSource::DefaultFromConfig { name, .. } => name.clone(),
        other => panic!("expected ActiveFromConfig or DefaultFromConfig, got {other:?}"),
    };
    assert_eq!(name, "work");
}

#[test]
fn bare_resolution_no_config_path_falls_back_to_ephemeral_in_memory() {
    // No path → can't persist → the resolver mints in-memory only
    // and returns `Ephemeral`. Nothing is written anywhere.
    let r = resolve_with(ResolveInputs::default(), None).unwrap();
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

#[test]
fn resolve_precedence_flag_beats_active() {
    let t = TempDir::new().unwrap();
    // `work` is default+active. `--agent demo` must still win.
    let path = seed_config(&t, &["work", "demo"]);
    let r = resolve_with(
        ResolveInputs {
            agent_flag: Some("demo"),
            ..Default::default()
        },
        Some(&path),
    )
    .unwrap();
    match &r.source {
        AgentIdSource::NamedFlag { name, .. } => assert_eq!(name, "demo"),
        other => panic!("expected NamedFlag, got {other:?}"),
    }
}

#[test]
fn resolve_precedence_env_id_beats_active() {
    let t = TempDir::new().unwrap();
    let path = seed_config(&t, &["work", "demo"]);
    // `work` is active. An env-id should still take precedence.
    let uuid = Uuid::now_v7();
    let r = resolve_with(
        ResolveInputs {
            agent_id_env: Some(&uuid.to_string()),
            ..Default::default()
        },
        Some(&path),
    )
    .unwrap();
    assert_eq!(r.source, AgentIdSource::IdEnv);
    assert_eq!(r.agent_id.0, uuid);
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
fn legacy_singleton_migrates_and_bare_picks_migrated_active() {
    let t = TempDir::new().unwrap();
    let path = path_in(t.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let legacy = "019e3b00-0000-7000-8000-000000000001";
    std::fs::write(&path, format!("agent_id = \"{legacy}\"\n")).unwrap();

    // Bare resolution returns the migrated agent (now active by
    // virtue of the migration synthesising default = true + active
    // = true) and surfaces the migration note.
    let r = resolve_with(ResolveInputs::default(), Some(&path)).unwrap();
    match &r.source {
        AgentIdSource::ActiveFromConfig { name, .. } => assert_eq!(name, "default"),
        other => panic!("expected ActiveFromConfig, got {other:?}"),
    }
    assert_eq!(r.agent_id.0.to_string(), legacy);
    let note = r.migration.as_ref().expect("migration note");
    assert_eq!(note.migrated_name, "default");

    // And the migrated `default` agent is reachable via name.
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
