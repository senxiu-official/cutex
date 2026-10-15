use super::*;

#[test]
fn preferences_persist_without_crossing_sessions_and_reload_labels() {
    let root =
        std::env::temp_dir().join(format!("cutex-notification-test-{}", uuid::Uuid::new_v4()));
    let id = uuid::Uuid::new_v4().to_string();
    let other = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        session_at(&root, &id, Change::Read).unwrap().level,
        Level::Off
    );
    assert!(!root.exists());
    assert_eq!(
        session_at(&root, &id, Change::Cycle).unwrap().level,
        Level::Important
    );
    assert_eq!(session_at(&root, &id, Change::Read).unwrap().label, "CIAO!");
    assert_eq!(
        session_at(&root, &other, Change::Read).unwrap().level,
        Level::Off
    );
    fs::write(
        root.join("config.json"),
        r#"{"important":"关注","normal":"普通","off":"关闭"}"#,
    )
    .unwrap();
    assert_eq!(session_at(&root, &id, Change::Read).unwrap().label, "关注");
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let root = root.clone();
            let id = id.clone();
            std::thread::spawn(move || session_at(&root, &id, Change::Cycle).unwrap())
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(
        session_at(&root, &id, Change::Read).unwrap().level,
        Level::Off
    );
    fs::write(root.join("config.json"), r#"{"important":"\u001b[31m"}"#).unwrap();
    assert!(session_at(&root, &id, Change::Set(Level::Normal)).is_err());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn style_overrides_reload_and_invalid_style_does_not_change_priority() {
    let root = std::env::temp_dir().join(format!("cutex-style-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    fs::write(root.join("config.json"), r##"{"important":"same","normal":"same","styles":{"normal":{"fg":"#123456","bold":true}}}"##).unwrap();
    let important = session_at(&root, &id, Change::Set(Level::Important)).unwrap();
    let normal = session_at(&root, &id, Change::Set(Level::Normal)).unwrap();
    assert_eq!(important.label, normal.label);
    assert_eq!(
        normal.style,
        ItemStyle {
            fg: "#123456".into(),
            bold: true
        }
    );
    assert_ne!(important.style, normal.style);
    fs::write(
        root.join("config.json"),
        r#"{"styles":{"normal":{"fg":"red"}}}"#,
    )
    .unwrap();
    assert!(session_at(&root, &id, Change::Cycle).is_err());
    assert_eq!(
        read_json::<Level>(&root.join("sessions").join(format!("{id}.json"))).unwrap(),
        Level::Normal
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn creation_default_is_persisted_once_and_never_overrides_a_session() {
    let dir = std::env::temp_dir().join(format!("cutex-default-notify-{}", uuid::Uuid::new_v4()));
    let id = uuid::Uuid::new_v4().to_string();
    let first = super::session_at_with_initial(&dir, &id, Change::Read, Some(Level::Normal)).unwrap();
    assert_eq!(first.level, Level::Normal);
    let later = super::session_at_with_initial(&dir, &id, Change::Read, Some(Level::Important)).unwrap();
    assert_eq!(later.level, Level::Normal);
    assert_eq!(super::session_at(&dir, &id, Change::Read).unwrap().level, Level::Normal);
    let id = uuid::Uuid::new_v4().to_string();
    assert_eq!(super::session_at_with_initial(&dir, &id, Change::Cycle, Some(Level::Normal)).unwrap().level, Level::Off);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn old_or_non_timestamped_sessions_never_receive_creation_defaults() {
    let id = "01800000-0000-7000-8000-000000000000";
    let stamp = u128::from_str_radix("018000000000", 16).unwrap();
    assert_eq!(creation_level(id, Some(&(stamp + 1).to_string()), Some("\"normal\"")), None);
    assert_eq!(creation_level(id, Some(&stamp.to_string()), Some("\"normal\"")), Some(Level::Normal));
    assert_eq!(creation_level(&uuid::Uuid::new_v4().to_string(), Some("0"), Some("\"normal\"")), None);
}
