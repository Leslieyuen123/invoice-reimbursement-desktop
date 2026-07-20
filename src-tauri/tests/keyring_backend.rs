#[cfg(target_os = "macos")]
#[test]
fn macos_keyring_persists_between_entry_instances() {
    let identifier = uuid::Uuid::new_v4();
    let service = format!("com.invoice-desk.keyring-test.{identifier}");
    let account = identifier.to_string();
    let expected = "invoice-desk-keyring-test";

    let writer = keyring::Entry::new(&service, &account).expect("keyring entry should be created");
    writer
        .set_password(expected)
        .expect("test credential should be stored");

    let reader = keyring::Entry::new(&service, &account).expect("keyring entry should be reopened");
    let observed = reader.get_password();
    let _ = writer.delete_credential();

    assert_eq!(
        observed.expect("stored credential should be readable from a new entry"),
        expected,
    );
}
