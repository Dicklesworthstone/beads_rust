use beads_rust::format::truncate_title;

#[test]
fn test_truncate_title_with_wide_characters() {
    // "🚀🚀🚀" is 3 chars, but 6 visual columns.
    // If max_len is 4, it should probably truncate to "🚀..." or similar?
    // Actually, truncate_title truncates by *characters*, not visual width.
    // So truncate_title("🚀🚀🚀", 4) with current logic:
    // title_len = 3 chars. <= 4. Returns "🚀🚀🚀".
    // Visual width is 6. If terminal is 4 columns wide, this overflows.

    let title = "🚀🚀🚀";
    let _truncated = truncate_title(title, 4);

    // With current buggy logic:
    // chars().count() is 3. 3 <= 4. Returns "🚀🚀🚀".
    // Visual width is 6.

    // We expect it to truncate to fit visual width 4.
    // "🚀..." is 2 + 3 = 5 width. Too wide.
    // "..." is 3 width. Fits.
    // So "🚀🚀🚀" should probably become "..." if max width is 4.

    // Let's assert what we *want* (correct behavior), which will fail.
    // Or simpler: "あaaaaa" (1 wide char + 5 ascii).
    // Width: 2 + 5 = 7.
    // Chars: 6.
    // If max_len = 6.
    // Current logic: chars count is 6. Returns "あaaaaa".
    // Actual width: 7. Overflow!

    let title_mixed = "あaaaaa";
    let truncated_mixed = truncate_title(title_mixed, 6);

    // We expect valid truncation to fit in 6 columns.
    // "あ..." -> 2 + 3 = 5 cols. Fits.
    // "あa..." -> 2 + 1 + 3 = 6 cols. Fits perfectly.
    // "あaa..." -> 2 + 2 + 3 = 7 cols. Overflow.

    // So result should be "あa..." or shorter.
    // With new logic, it should be "あa..." (width 6).
    assert_eq!(
        truncated_mixed, "あa...",
        "Logic should correctly truncate to visual width 6"
    );

    // Verify emoji case too
    let title_emoji = "🚀🚀🚀";
    let truncated_emoji = truncate_title(title_emoji, 4);
    // Max 4. ellipsis is 3. 1 column left. 🚀 is 2 cols.
    // So "🚀..." is 5 cols. Doesn't fit.
    // Should return "..." (3 cols).
    assert_eq!(
        truncated_emoji, "...",
        "Logic should handle wide emoji correctly"
    );
}
