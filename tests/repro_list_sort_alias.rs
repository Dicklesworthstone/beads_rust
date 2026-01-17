use beads_rust::cli::ListArgs;
use beads_rust::cli::commands::list;
use beads_rust::config::CliOverrides;

#[test]
fn test_list_sort_aliases_are_accepted() {
    let args = ListArgs {
        sort: Some("created".to_string()),
        ..Default::default()
    };
    let overrides = CliOverrides::default();

    // This should now SUCCEED
    let result = list::execute(&args, false, &overrides);

    if let Err(e) = result {
        panic!("Expected Ok, got {e:?}");
    }
}
