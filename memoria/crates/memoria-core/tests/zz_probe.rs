use memoria_core::check_sensitivity;
#[test]
fn probe() {
    for t in [
        "555-867-5309 4111 1111 1111 1111",
        "555-867-5309 4111111111111111",
        "123-45-6789 4111 1111 1111 1111",
        "call 555-867-5309 123",
        "ip 192.168.100.200 ok",
        "4111 1111 1111 1111. 5555 5555 5555 4444",
    ] {
        let r = check_sensitivity(t);
        println!("{t:45} -> {:?} {:?}", r.redacted_content, r.matched_labels);
    }
}
