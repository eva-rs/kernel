use eva_protocol::types::text::{Catalog, Catalogs, Message};

#[test]
fn progress_keeps_plain_wire_compatibility() {
    use eva_protocol::{types::turn::ItemDelta, wire::tools::ToolItem};
    let old = serde_json::json!({"progress": {"text": "working"}});
    assert!(matches!(
        serde_json::from_value::<ToolItem>(old.clone()).unwrap(),
        ToolItem::Progress { message: None, .. }
    ));
    assert!(matches!(
        serde_json::from_value::<ItemDelta>(old.clone()).unwrap(),
        ItemDelta::Progress { message: None, .. }
    ));
    let plain = ToolItem::Progress {
        text: "working".into(),
        message: None,
    };
    assert_eq!(serde_json::to_value(plain).unwrap(), old);
    let message = Catalogs::default().bind("echo", Message::new("Hi {name}").arg("name", "Eva"));
    let localized = ToolItem::Progress {
        text: message.message.fallback(),
        message: Some(Box::new(message)),
    };
    let bytes = rmp_serde::to_vec_named(&localized).unwrap();
    assert_eq!(
        rmp_serde::from_slice::<ToolItem>(&bytes).unwrap(),
        localized
    );
    #[eva::data]
    enum OldProgress {
        Progress { text: String },
    }
    assert_eq!(
        rmp_serde::from_slice::<OldProgress>(&bytes).unwrap(),
        OldProgress::Progress {
            text: "Hi Eva".into()
        }
    );
}

#[test]
fn catalogs_validate_parameters_and_po_structure() {
    for source in [
        "msgid \"Hello {name}\"\nmsgstr \"Привет {missing}\"",
        "msgid \"Hi\"\nmsgstr \"a\"\nmsgstr \"b\"",
        "msgid \"Hi\"\nmsgstr \"a\"\n\nmsgid \"Hi\"\nmsgstr \"b\"",
        "msgid \"Hi\"\nmsgstr \"broken",
        "msgid \"\"\nmsgstr \"Plural-Forms: nplurals=2; plural=1/0;\\n\"",
        "msgid \"\"\nmsgstr \"Plural-Forms: nplurals=2; plural=9;\\n\"",
    ] {
        assert!(Catalog::parse(source, "ru").is_err(), "{source}");
    }
}

#[test]
fn multiline_context_fuzzy_and_missing_translation() {
    let source = r#"
msgctxt "button"
msgid "Open"
msgstr "Открыть"

#, fuzzy
msgid "Pending"
msgstr "Черновик"

msgid "Hello {name}"
msgstr "Привет "
"{name}"

msgid "Empty"
msgstr ""
"#;
    let mut catalogs = Catalogs::default();
    catalogs
        .insert(Catalog::parse(source, "ru").unwrap())
        .unwrap();
    assert_eq!(
        catalogs.render(Message::new("Hello {name}").arg("name", "Eva"), "ru-RU"),
        "Привет Eva"
    );
    assert_eq!(
        catalogs.render(Message::new("Open").context("button"), "ru"),
        "Открыть"
    );
    assert_eq!(catalogs.render(Message::new("Open"), "ru"), "Open");
    assert_eq!(catalogs.render(Message::new("Pending"), "ru"), "Pending");
    assert_eq!(catalogs.render(Message::new("Empty"), "ru"), "Empty");
}

#[test]
fn plural_evaluation_short_circuits_and_falls_back_for_missing_forms() {
    let source = "msgid \"\"\nmsgstr \"Plural-Forms: nplurals=2; plural=n == 0 ? 0 : (n/n);\\n\"\n\nmsgid \"{count} file\"\nmsgid_plural \"{count} files\"\nmsgstr[0] \"zero\"\nmsgstr[1] \"many\"";
    let mut catalogs = Catalogs::default();
    catalogs
        .insert(Catalog::parse(source, "xx").unwrap())
        .unwrap();
    assert_eq!(
        catalogs.render(
            Message::new("{count} file").plural("{count} files", 0),
            "xx"
        ),
        "zero"
    );
    assert_eq!(
        catalogs.render(
            Message::new("{count} file").plural("{count} files", u64::MAX),
            "xx"
        ),
        "many"
    );
}

#[test]
fn interpolation_is_bounded_and_arguments_are_not_reinterpreted() {
    assert_eq!(
        Message::new("{{{value}}}")
            .arg("value", "{other}")
            .fallback(),
        "{{other}}"
    );
    let message = Message::new("{x}".repeat(50_000)).arg("x", "я".repeat(100_000));
    assert!(message.fallback().len() <= 65_536);
    assert!(
        Message::new(format!("prefix{{{}}}", "я".repeat(100_000)))
            .fallback()
            .len()
            <= 65_536
    );
    assert!(
        Message::new(format!("x{}", "я".repeat(100_000)))
            .fallback()
            .len()
            <= 65_536
    );
}

#[test]
fn adjacent_entries_keep_context_and_fuzzy_flags() {
    let source = "msgid \"One\"\nmsgstr \"Один\"\nmsgctxt \"button\"\nmsgid \"Open\"\nmsgstr \"Открыть\"\n#, fuzzy\nmsgid \"Pending\"\nmsgstr \"Черновик\"";
    let mut catalogs = Catalogs::default();
    catalogs
        .insert(Catalog::parse(source, "ru").unwrap())
        .unwrap();
    assert_eq!(catalogs.render(Message::new("One"), "ru"), "Один");
    assert_eq!(
        catalogs.render(Message::new("Open").context("button"), "ru"),
        "Открыть"
    );
    assert_eq!(catalogs.render(Message::new("Pending"), "ru"), "Pending");
}
