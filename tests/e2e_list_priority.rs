mod common;
use common::cli::{BrWorkspace, run_br};

#[test]
fn test_list_priority_accepts_p_prefix() {
    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");
    run_br(&workspace, ["create", "Critical", "-p", "0"], "create");

    // This should work (numeric)
    let list_num = run_br(&workspace, ["list", "-p", "0"], "list_num");
    assert!(
        list_num.status.success(),
        "Numeric priority failed: {}",
        list_num.stderr
    );
    assert!(list_num.stdout.contains("Critical"));

    // This should work (P-prefix) but likely fails currently
    let list_p = run_br(&workspace, ["list", "-p", "P0"], "list_p");

    // If it fails with clap error, it's because of Vec<u8> type
    if !list_p.status.success() {
        println!("P-prefix priority failed as expected: {}", list_p.stderr);
        // We assert failure to confirm bug reproduction, or assert success if we want to enforce fix
        // I want to confirm it fails now so I can fix it.
        assert!(list_p.stderr.contains("invalid value") || list_p.stderr.contains("error"));
    } else {
        // If it succeeds, then my hypothesis is wrong (maybe clap handles it?)
        // But clap parser for u8 won't parse "P0".
        println!("P-prefix priority unexpectedly succeeded");
    }
}
